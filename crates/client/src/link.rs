//! Link: one server kept connected, its sessions
//! resumed on every new connection.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;

use crate::conn::Conn;
use crate::rt::{self, Changed, Latch};
use crate::session::{AttachOptions, Session, SessionInner};
use crate::{Config, Error, Result};

/// Reconnecting: the first try goes at once and later ones stay frequent,
/// so sessions are back about one handshake after the network is. Each try
/// is short-lived: a dial started while the network was down must not hold
/// up one that would succeed now.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(1);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub(crate) type DialFn = Arc<dyn Fn(Config) -> BoxFuture<Result<Conn>> + Send + Sync>;

/// Keeps a connection to one server alive: when it dies or turns suspect
/// (a ping went unanswered), it dials a new one, racing dials as
/// [`Conn::dial`] does, and resumes every session attached through it
/// there. Channel streams ([`Conn::dial_forward`], [`Conn::exec`]) end with
/// their connection; callers open new ones on the next Conn. Clones share
/// the link.
#[derive(Clone)]
pub struct Link(Arc<LinkInner>);

pub(crate) struct LinkInner {
    cfg: Config,
    dial: DialFn,
    st: Mutex<LState>,
    changed: Changed,
    /// A session needs resuming.
    kick: Notify,
    cancel: Latch,
    done: Latch,
}

struct LState {
    conn: Option<Conn>,
    /// The last dial's error.
    err: Option<Error>,
    /// The link has stopped for good: closed, or incompatible.
    fin: Option<Error>,
    /// A dial has succeeded.
    connected: bool,
    sessions: HashMap<usize, Session>,
}

fn timed_out() -> Error {
    Error::Io(Arc::new(io::Error::new(io::ErrorKind::TimedOut, "osewa: timed out")))
}

impl Link {
    /// Starts connecting to cfg's server in the background.
    pub fn new(cfg: Config) -> Link {
        Link::with_dial(cfg, Arc::new(|cfg| Box::pin(Conn::dial(cfg))))
    }

    pub(crate) fn with_dial(cfg: Config, dial: DialFn) -> Link {
        let l = Arc::new(LinkInner {
            cfg,
            dial,
            st: Mutex::new(LState { conn: None, err: None, fin: None, connected: false, sessions: HashMap::new() }),
            changed: Changed::default(),
            kick: Notify::new(),
            cancel: Latch::default(),
            done: Latch::default(),
        });
        rt::spawn(l.clone().run());
        Link(l)
    }

    /// The current connection, waiting for one while the link reconnects.
    /// Until a first connection is up, a failed dial is returned, so a wrong
    /// address or key shows at once (the link keeps trying; call again to
    /// wait for the next attempt); after that, it waits through failures
    /// (bound it with a timeout; [`Link::last_error`] says why). The
    /// connection may be suspect.
    pub async fn conn(&self) -> Result<Conn> {
        loop {
            let w = {
                let mut st = self.0.lock();
                if let Some(e) = &st.fin {
                    return Err(e.clone());
                }
                if let Some(c) = st.conn.as_ref().filter(|c| !c.is_done()) {
                    return Ok(c.clone());
                }
                if !st.connected {
                    // Reported; the next call waits for a new attempt.
                    if let Some(e) = st.err.take() {
                        return Err(e);
                    }
                }
                self.0.changed.watch()
            };
            w.changed().await;
        }
    }

    /// The last dial's error, while the link is down.
    pub fn last_error(&self) -> Option<Error> {
        self.0.lock().err.clone()
    }

    /// Reports whether a connection is up now, without waiting for one.
    pub fn up(&self) -> bool {
        let st = self.0.lock();
        st.fin.is_none() && st.conn.as_ref().is_some_and(|c| !c.is_done())
    }

    /// Reports whether a connection has ever been up.
    pub fn connected(&self) -> bool {
        self.0.lock().connected
    }

    /// Attaches to session id on the current connection. The link resumes
    /// the session on every new connection until it ends or the link
    /// closes; its [`Session::lost`] never fires.
    pub async fn attach(&self, id: &str, opt: AttachOptions) -> Result<Session> {
        let c = self.conn().await?;
        let own = opt.own;
        let s = SessionInner::create(id, opt, Some(Arc::downgrade(&self.0)));
        struct CloseOnDrop(Option<Session>);
        impl Drop for CloseOnDrop {
            fn drop(&mut self) {
                if let Some(s) = self.0.take() {
                    s.close();
                }
            }
        }
        let mut guard = CloseOnDrop(Some(s.clone()));
        s.0.attach(&c, own).await?;
        // Registered only once attached, or the run loop could resume it
        // alongside this first attach.
        {
            let mut st = self.0.lock();
            if let Some(e) = &st.fin {
                return Err(e.clone());
            }
            st.sessions.insert(s.0.key(), s.clone());
        }
        guard.0 = None;
        if s.is_done() {
            // It ended before it was registered.
            self.0.forget(s.0.key());
        }
        // A loss between the attach and now went unseen by the loop.
        self.0.lost_session();
        Ok(s)
    }

    /// Stops reconnecting and closes the connection. Sessions stay as they
    /// are: resumable on another Conn, not detached.
    pub async fn close(&self) {
        {
            let mut st = self.0.lock();
            if st.fin.is_none() {
                st.fin = Some(Error::LinkClosed);
                self.0.changed.notify();
            }
        }
        self.0.cancel.set();
        self.0.done.wait().await;
        let c = self.0.lock().conn.clone();
        if let Some(c) = c {
            c.close().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn session_count(&self) -> usize {
        self.0.lock().sessions.len()
    }

    #[cfg(test)]
    pub(crate) fn kick(&self) {
        self.0.lost_session();
    }
}

impl LinkInner {
    fn lock(&self) -> MutexGuard<'_, LState> {
        self.st.lock().unwrap()
    }

    /// Asks the run loop to look at the sessions again.
    pub(crate) fn lost_session(&self) {
        self.kick.notify_one();
    }

    pub(crate) fn forget(&self, key: usize) {
        self.lock().sessions.remove(&key);
    }

    async fn run(self: Arc<Self>) {
        struct Done<'a>(&'a Latch);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.set();
            }
        }
        let _done = Done(&self.done);
        let mut c: Option<Conn> = None;
        let mut wait = Duration::ZERO;
        loop {
            let retry;
            if c.as_ref().is_none_or(|c| c.is_done() || c.is_suspect()) {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = self.cancel.wait() => return,
                }
                wait = (wait * 2).clamp(BACKOFF_MIN, BACKOFF_MAX);
                let deadline = tokio::time::Instant::now() + ATTEMPT_TIMEOUT;
                let r = match tokio::time::timeout_at(deadline, (self.dial)(self.cfg.clone())).await {
                    Err(_) => Err(timed_out()),
                    Ok(Err(e)) => Err(e),
                    Ok(Ok(nc)) => {
                        // Sessions resume behind HELLO, pipelined (PROTOCOL.md),
                        // rather than a round trip later behind WELCOME: an
                        // incompatible server fails their ATTACH too, ending
                        // them with the reason, and on a connection that fails
                        // they wait for the next one.
                        let resumed = tokio::spawn(self.clone().resume_all(nc.clone()));
                        let w = match tokio::time::timeout_at(deadline, nc.welcome()).await {
                            Err(_) => Err(timed_out()),
                            Ok(r) => r,
                        };
                        match w {
                            Ok(_) => Ok((nc, resumed)),
                            Err(e) => {
                                nc.close_now();
                                let _ = resumed.await;
                                Err(e)
                            }
                        }
                    }
                };
                if self.cancel.is_set() {
                    if let Ok((nc, resumed)) = r {
                        nc.close_now();
                        let _ = resumed.await;
                    }
                    return;
                }
                let (nc, resumed) = match r {
                    Ok(v) => v,
                    Err(e) => {
                        let incompatible = e.is_incompatible();
                        {
                            let mut st = self.lock();
                            st.err = Some(e.clone());
                            if incompatible {
                                // No server of this build will serve us;
                                // retrying can't help, and the sessions end
                                // with the reason.
                                st.fin = Some(e.clone());
                            }
                            self.changed.notify();
                        }
                        if incompatible {
                            self.end_sessions(e);
                            return;
                        }
                        continue;
                    }
                };
                wait = Duration::ZERO;
                let old = c.replace(nc.clone());
                {
                    let mut st = self.lock();
                    st.conn = Some(nc);
                    st.err = None;
                    st.connected = true;
                    self.changed.notify();
                }
                retry = resumed.await.unwrap_or(None);
                // The old connection may be half-open: don't wait on it.
                if let Some(old) = old {
                    old.close_now();
                }
            } else {
                retry = self.clone().resume_all(c.clone().unwrap()).await;
            }
            let c = c.clone().unwrap();
            tokio::select! {
                _ = c.done() => {}
                _ = c.suspect() => {}
                _ = self.kick.notified() => {}
                _ = async { match retry { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => {}
                _ = self.cancel.wait() => return,
            }
        }
    }

    /// Resumes, on c, every session not attached there, in parallel. It
    /// returns when to try again if one failed on a connection that still
    /// looks fine.
    async fn resume_all(self: Arc<Self>, c: Conn) -> Option<Duration> {
        let todo: Vec<Session> = self.lock().sessions.values().cloned().collect();
        let mut set = tokio::task::JoinSet::new();
        for s in todo {
            if s.0.is_ended() || s.0.on(&c) {
                continue;
            }
            let c = c.clone();
            set.spawn(async move {
                match tokio::time::timeout(ATTEMPT_TIMEOUT, s.resume(&c)).await {
                    Ok(Ok(())) => false,
                    Ok(Err(e)) => !e.is_ended() && !s.is_done(),
                    Err(_) => !s.is_done(),
                }
            });
        }
        let mut failed = false;
        while let Some(r) = set.join_next().await {
            failed |= r.unwrap_or(false);
        }
        failed.then_some(BACKOFF_MIN)
    }

    fn end_sessions(&self, err: Error) {
        let all: Vec<Session> = self.lock().sessions.values().cloned().collect();
        for s in all {
            s.0.finish_pub(Some(err.clone()));
        }
    }
}
