//! Link: one server kept connected, its sessions
//! resumed on every new connection.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;

use crate::conn::Conn;
use crate::rt::{self, Changed, Latch, Watch};
use crate::session::{AttachOptions, Session, SessionInner};
use crate::{Config, Error, Result};

/// Reconnecting: the first try goes at once and later ones stay frequent,
/// so sessions are back about one handshake after the network is. Each try
/// is short-lived: a dial started while the network was down must not hold
/// up one that would succeed now.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(1);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
/// Bounds the closing handshake on pause: the connection may be half-open.
const PAUSE_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// The QUIC port the server last advertised, which later dials race
    /// the WebSocket on; 0 for none yet.
    quic_port: AtomicU16,
    dial: DialFn,
    st: Mutex<LState>,
    changed: Changed,
    /// A session needs resuming.
    kick: Notify,
    /// pause or wake was called.
    toggle: Notify,
    cancel: Latch,
    done: Latch,
}

/// Where a [`Link`] stands, as [`Link::state`] reads it.
#[derive(Debug, Clone, Default)]
pub struct LinkState {
    /// A connection is up (it may be suspect).
    pub up: bool,
    /// A connection has ever been up.
    pub connected: bool,
    /// The connection's carrier while up: "quic" or "ws".
    pub carrier: Option<&'static str>,
    /// The last dial's error, while down.
    pub err: Option<Error>,
    /// Why the link stopped for good: closed, or an incompatible server.
    pub fin: Option<Error>,
    /// Paused, or redialing after a wake and not yet failed: the link is
    /// down because it was asked to be, not lost.
    pub paused: bool,
}

struct LState {
    conn: Option<Conn>,
    /// The last dial's error.
    err: Option<Error>,
    /// The link has stopped for good: closed, or incompatible.
    fin: Option<Error>,
    /// A dial has succeeded.
    connected: bool,
    /// pause was called and wake not since.
    paused: bool,
    /// Woken, and the first redial since has no outcome yet.
    waking: bool,
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
            quic_port: AtomicU16::new(cfg.quic_port),
            cfg,
            dial,
            st: Mutex::new(LState { conn: None, err: None, fin: None, connected: false, paused: false, waking: false, sessions: HashMap::new() }),
            changed: Changed::default(),
            kick: Notify::new(),
            toggle: Notify::new(),
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

    /// Where the link stands now.
    pub fn state(&self) -> LinkState {
        let st = self.0.lock();
        let conn = st.conn.as_ref().filter(|c| !c.is_done() && st.fin.is_none());
        LinkState {
            up: conn.is_some(),
            connected: st.connected,
            carrier: conn.map(|c| c.carrier()),
            err: st.err.clone(),
            fin: st.fin.clone(),
            paused: st.paused || st.waking,
        }
    }

    /// Hears every change of [`Link::state`]: a connection up or lost, a
    /// dial failing, the link stopping.
    pub fn watch(&self) -> Watch {
        self.0.changed.public()
    }

    /// Checks the connection now (a ping, [`Conn::check`]) rather than at
    /// the next keepalive, for when the network may have changed under
    /// it; one found dead is replaced at once.
    pub fn check(&self) {
        let c = self.0.lock().conn.clone();
        if let Some(c) = c {
            c.check();
        }
    }

    /// Closes the connection and stops reconnecting until [`Link::wake`]:
    /// no keepalive and no dials meanwhile, for an app in the background.
    /// Sessions stay attached to the link and resume on wake; conn and
    /// attach wait for it.
    pub fn pause(&self) {
        let mut st = self.0.lock();
        if st.paused || st.fin.is_some() {
            return;
        }
        (st.paused, st.waking) = (true, false);
        self.0.changed.notify();
        self.0.toggle.notify_one();
    }

    /// Ends a pause: dials at once and resumes the sessions.
    pub fn wake(&self) {
        let mut st = self.0.lock();
        if !st.paused {
            return;
        }
        (st.paused, st.waking) = (false, true);
        self.0.changed.notify();
        self.0.toggle.notify_one();
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
        s.0.attach(&c, own, false).await?;
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
    pub(crate) fn quic_port(&self) -> u16 {
        self.0.quic_port.load(Ordering::Relaxed)
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
            if self.lock().paused {
                if !self.sit_out(&mut c).await {
                    return;
                }
                wait = Duration::ZERO;
            }
            let retry;
            if c.as_ref().is_none_or(|c| c.is_done() || c.is_suspect()) {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    // A pause: don't dial.
                    _ = self.toggle.notified() => continue,
                    _ = self.cancel.wait() => return,
                }
                wait = (wait * 2).clamp(BACKOFF_MIN, BACKOFF_MAX);
                let deadline = tokio::time::Instant::now() + self.cfg.attempt_timeout.unwrap_or(ATTEMPT_TIMEOUT);
                let mut cfg = self.cfg.clone();
                cfg.quic_port = self.quic_port.load(Ordering::Relaxed);
                let r = match tokio::time::timeout_at(deadline, (self.dial)(cfg)).await {
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
                            Ok(w) => {
                                if let Some(q) = w.carriers.iter().find(|c| c.carrier == "quic" && c.port > 0) {
                                    self.quic_port.store(u16::try_from(q.port).unwrap_or(0), Ordering::Relaxed);
                                }
                                Ok((nc, resumed))
                            }
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
                            st.waking = false;
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
                self.woken();
                // The old connection may be half-open: don't wait on it.
                if let Some(old) = old {
                    old.close_now();
                }
            } else {
                // Woken before the pause was acted on.
                self.woken();
                retry = self.clone().resume_all(c.clone().unwrap()).await;
            }
            let c = c.clone().unwrap();
            tokio::select! {
                _ = c.done() => {}
                _ = c.suspect() => {}
                _ = self.kick.notified() => {}
                _ = self.toggle.notified() => {}
                _ = async { match retry { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => {}
                _ = self.cancel.wait() => return,
            }
            // Watchers see a lost connection now, not at the next dial's
            // outcome.
            if c.is_done() {
                self.changed.notify();
            }
        }
    }

    /// Closes c and waits out the pause; false when the link is cancelled.
    async fn sit_out(&self, c: &mut Option<Conn>) -> bool {
        if let Some(old) = c.take() {
            {
                let mut st = self.lock();
                st.conn = None;
                self.changed.notify();
            }
            // Gracefully, so the server lets go of it now rather than at
            // its idle timeout.
            let _ = tokio::time::timeout(PAUSE_CLOSE_TIMEOUT, old.close()).await;
            old.close_now();
        }
        loop {
            if !self.lock().paused {
                return true;
            }
            tokio::select! {
                _ = self.toggle.notified() => {}
                _ = self.cancel.wait() => return false,
            }
        }
    }

    /// The sessions are back after a wake, or as back as they will get.
    fn woken(&self) {
        let mut st = self.lock();
        if st.waking {
            st.waking = false;
            self.changed.notify();
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
