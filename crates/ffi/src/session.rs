//! One open session: a thread that connects, creates or attaches, and then
//! follows the link and the session, takes the app's input in order, and
//! ticks predictions; and the callbacks, which go quiet on close.

use std::ffi::{c_char, c_void, CString};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use osewa_client::{AttachOptions, CreateOptions, Error, Link, Session};
use osewa_mirror::{Predictor, PredictorConfig};
use osewa_vt::{Attached, Ui};
use osewa_wire as wire;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use crate::options::Opened;

pub const CONNECTING: i32 = 0;
pub const CONNECTED: i32 = 1;
pub const RECONNECTING: i32 = 2;
pub const CLOSED: i32 = 3;

/// Bounds a create, an attach, and the requests input makes, so a
/// connection that died under them is retried on the next one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Between attach attempts that failed on a connection that looked fine.
const RETRY_AFTER: Duration = Duration::from_millis(250);
/// stats at most this often: a sample comes with every keystroke's ack.
const STATS_EVERY: Duration = Duration::from_secs(1);
/// Bounds the input the app may have outstanding: queued here, or with the
/// session awaiting the holder's ack. Past it send and paste refuse, so an
/// outage (or a connection that never comes) can't queue without end.
pub const INPUT_MAX: usize = 1 << 20;

/// Why send or paste refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The session is over.
    Closed,
    /// INPUT_MAX is outstanding.
    Full,
}

/// osewa_callbacks, as osewa.h lays it out.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub ctx: *mut c_void,
    pub output: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)>,
    pub state: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    pub stats: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_char)>,
    pub event: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
}

// The app hands ctx over to be used from osewa's threads (osewa.h).
unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

/// A C string for a callback: interior NULs, which only a remote string
/// could carry, are dropped rather than cutting it short.
fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Where everything for the app goes. Callbacks run under the read lock;
/// off takes the write lock, so once it returns none is running and none
/// will.
pub struct Sink {
    cb: RwLock<Option<Callbacks>>,
    /// The last state reported, to report changes only.
    last: Mutex<Option<(i32, Option<String>)>>,
}

impl Sink {
    pub fn new(cb: Callbacks) -> Sink {
        Sink { cb: RwLock::new(Some(cb)), last: Mutex::new(None) }
    }

    fn with(&self, f: impl FnOnce(&Callbacks)) {
        let g = self.cb.read().unwrap_or_else(|e| e.into_inner());
        if let Some(cb) = g.as_ref() {
            f(cb);
        }
    }

    pub fn output(&self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        self.with(|cb| {
            if let Some(f) = cb.output {
                unsafe { f(cb.ctx, b.as_ptr(), b.len()) }
            }
        });
    }

    /// Reports a state, unless it is the one reported last. CLOSED is
    /// reported once.
    pub fn state(&self, state: i32, detail: Option<&str>) {
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = (state, detail.map(str::to_string));
            if last.as_ref().is_some_and(|l| *l == now || l.0 == CLOSED) {
                return;
            }
            *last = Some(now);
        }
        let detail = detail.map(cstring);
        self.with(|cb| {
            if let Some(f) = cb.state {
                unsafe { f(cb.ctx, state, detail.as_ref().map_or(std::ptr::null(), |d| d.as_ptr())) }
            }
        });
    }

    fn stats(&self, srtt: Duration, carrier: &str) {
        let carrier = cstring(carrier);
        let us = u32::try_from(srtt.as_micros()).unwrap_or(u32::MAX);
        self.with(|cb| {
            if let Some(f) = cb.stats {
                unsafe { f(cb.ctx, us, carrier.as_ptr()) }
            }
        });
    }

    pub fn event(&self, json: &str) {
        let json = cstring(json);
        self.with(|cb| {
            if let Some(f) = cb.event {
                unsafe { f(cb.ctx, json.as_ptr()) }
            }
        });
    }

    /// Stops every callback, waiting out one that is running.
    pub fn off(&self) {
        *self.cb.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The Ui's terminal: the app's, through output.
struct Out(Arc<Sink>);

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.output(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What the app asks of the session thread, in order.
enum Cmd {
    Send(Vec<u8>),
    Paste(String),
    Redraw,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Size {
    cols: u16,
    rows: u16,
    width: u32,
    height: u32,
}

/// What close and kill need of the session thread's work.
#[derive(Default)]
struct Live {
    id: Option<String>,
    s: Option<Session>,
    a: Option<Attached>,
}

/// Input outstanding, in bytes: in the channel to the session thread, and
/// with the session unacked as the thread last saw it. Only atomics, as
/// send may come from inside a callback that runs with the session locked.
#[derive(Default)]
struct Backlog {
    queued: AtomicUsize,
    unacked: AtomicUsize,
}

/// An open session (osewa_session).
pub struct Handle {
    sink: Arc<Sink>,
    backlog: Arc<Backlog>,
    cmds: mpsc::UnboundedSender<Cmd>,
    size: watch::Sender<Size>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    link: Link,
    live: Arc<Mutex<Live>>,
}

impl Handle {
    /// Starts connecting in the background.
    pub fn open(o: Opened, cb: Callbacks) -> Handle {
        let sink = Arc::new(Sink::new(cb));
        let (cmds, cmd_rx) = mpsc::unbounded_channel();
        let size = Size { cols: o.cols, rows: o.rows, width: o.width, height: o.height };
        let (size, size_rx) = watch::channel(size);
        let (stop, stop_rx) = oneshot::channel();
        let live = Arc::new(Mutex::new(Live { id: o.session_id.clone(), ..Default::default() }));
        let mut cfg = o.cfg.clone();
        // The app may want to remember the QUIC port for the next open,
        // as the CLI keeps it in hosts.json.
        let csink = sink.clone();
        let known = o.cfg.quic_port;
        cfg.on_carriers = Some(Arc::new(move |cs: &[wire::Carrier]| {
            if let Some(q) = cs.iter().find(|c| c.carrier == "quic" && c.port > 0) {
                if q.port != i64::from(known) {
                    csink.event(&serde_json::json!({"type": "carriers", "quic_port": q.port}).to_string());
                }
            }
        }));
        let link = Link::new(cfg);
        let backlog = Arc::new(Backlog::default());
        let w = Worker { o, link: link.clone(), sink: sink.clone(), live: live.clone(), backlog: backlog.clone() };
        let thread = std::thread::Builder::new()
            .name("osewa-session".into())
            .spawn(move || {
                osewa_client::block_on(async move {
                    let mut cmd_rx = cmd_rx;
                    let mut size_rx = size_rx;
                    tokio::select! {
                        _ = stop_rx => {}
                        _ = w.run(&mut cmd_rx, &mut size_rx) => {}
                    }
                })
            })
            .expect("osewa: session thread");
        Handle { sink, backlog, cmds, size, stop: Some(stop), thread: Some(thread), link, live }
    }

    pub fn send(&self, b: &[u8]) -> Result<(), Refused> {
        self.queue(b.len(), Cmd::Send(b.to_vec()))
    }

    pub fn paste(&self, text: String) -> Result<(), Refused> {
        self.queue(text.len(), Cmd::Paste(text))
    }

    fn queue(&self, n: usize, cmd: Cmd) -> Result<(), Refused> {
        if self.cmds.is_closed() {
            return Err(Refused::Closed);
        }
        let b = &self.backlog;
        let queued = b.queued.fetch_add(n, Ordering::SeqCst);
        if queued + b.unacked.load(Ordering::SeqCst) + n > INPUT_MAX {
            b.queued.fetch_sub(n, Ordering::SeqCst);
            return Err(Refused::Full);
        }
        self.cmds.send(cmd).map_err(|_| Refused::Closed)
    }

    pub fn redraw(&self) {
        let _ = self.cmds.send(Cmd::Redraw);
    }

    pub fn resize(&self, cols: u16, rows: u16, width: u32, height: u32) {
        self.size.send_replace(Size { cols, rows, width, height });
    }

    pub fn network_changed(&self) {
        self.link.check();
    }

    pub fn pause(&self) {
        self.link.pause();
    }

    pub fn wake(&self) {
        self.link.wake();
    }

    /// Stops the callbacks, waiting out one that is running. Through a
    /// shared reference, before the handle is freed: a running callback may
    /// still be using it (osewa_send from inside one).
    pub fn quiesce(&self) {
        self.sink.off();
    }

    /// Stops the session thread, then lets go of the session in the
    /// background: detached (it keeps running on the host) or killed. Call
    /// quiesce first.
    pub fn close(mut self, kill: bool) {
        self.sink.off();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let live = std::mem::take(&mut *self.live.lock().unwrap_or_else(|e| e.into_inner()));
        let link = self.link.clone();
        // The network may be gone: the app's thread must not wait on it.
        std::thread::spawn(move || {
            osewa_client::block_on(async {
                if kill {
                    if let Some(id) = &live.id {
                        let _ = osewa_client::timeout(REQUEST_TIMEOUT, async {
                            let c = link.conn().await?;
                            c.kill(id).await
                        })
                        .await;
                    }
                } else if let Some(s) = &live.s {
                    let _ = osewa_client::timeout(Duration::from_secs(2), s.detach()).await;
                }
                if let Some(s) = &live.s {
                    s.close();
                }
                let _ = osewa_client::timeout(Duration::from_secs(2), link.close()).await;
            });
            if let Some(a) = live.a {
                a.m.close();
            }
        });
    }
}

/// The session thread's work.
struct Worker {
    o: Opened,
    link: Link,
    sink: Arc<Sink>,
    live: Arc<Mutex<Live>>,
    backlog: Arc<Backlog>,
}

impl Worker {
    async fn run(&self, cmds: &mut mpsc::UnboundedReceiver<Cmd>, size: &mut watch::Receiver<Size>) {
        self.sink.state(CONNECTING, None);
        let rtt = Arc::new(Notify::new());
        let Some((s, a)) = self.attach(size, &rtt).await else { return };
        {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            live.id = Some(s.id().to_string());
            live.s = Some(s.clone());
            live.a = Some(a.clone());
        }
        self.sink.event(&serde_json::json!({"type": "session", "id": s.id()}).to_string());
        a.send_appearance().await;
        let mut events = s.take_events();
        // Only follow ends (the others run until the thread is stopped), so
        // the end is always reported, after the last events.
        tokio::select! {
            _ = self.follow(&s, &rtt, &mut events) => {}
            _ = input(cmds, &s, &a, &self.backlog) => {}
            _ = resize(size, &s) => {}
            _ = osewa_vt::ticker(a.clone(), std::future::pending::<()>()) => {}
        }
        self.finish(&s, &mut events);
    }

    /// Reports the last events and why the session is over, once it is.
    fn finish(&self, s: &Session, events: &mut Option<mpsc::Receiver<wire::EventMsg>>) {
        if !s.is_done() {
            return;
        }
        while let Some(e) = events.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.sink.event(&event_json(&e));
        }
        self.sink.state(CLOSED, Some(&ended(s)));
    }

    /// Creates the session if asked to, and attaches it through the link,
    /// trying again on every new connection until it works or can't.
    async fn attach(&self, size: &mut watch::Receiver<Size>, rtt: &Arc<Notify>) -> Option<(Session, Attached)> {
        let o = &self.o;
        let mut id = o.session_id.clone();
        loop {
            let c = match self.or_fatal(self.link.conn()).await {
                None => return None,
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    if let Some(why) = fatal(&e) {
                        self.sink.state(CLOSED, Some(&why));
                        return None;
                    }
                    // Not connected yet; the link keeps dialing, and conn
                    // waits for its next attempt.
                    self.sink.state(CONNECTING, Some(&e.to_string()));
                    continue;
                }
            };
            let z = *size.borrow_and_update();
            if id.is_none() {
                let create = CreateOptions {
                    cols: z.cols.into(),
                    rows: z.rows.into(),
                    width: z.width.into(),
                    height: z.height.into(),
                    argv: o.argv.clone(),
                    cwd: o.cwd.clone(),
                    env: Vec::new(),
                };
                let created = self.or_fatal(osewa_client::timeout(REQUEST_TIMEOUT, c.create(create))).await?;
                match created {
                    Some(Ok(new)) => {
                        self.live.lock().unwrap_or_else(|e| e.into_inner()).id = Some(new.clone());
                        id = Some(new);
                    }
                    Some(Err(e)) => {
                        if let Some(why) = fatal(&e).or_else(|| e.code().map(|_| format!("create: {e}"))) {
                            self.sink.state(CLOSED, Some(&why));
                            return None;
                        }
                        osewa_client::sleep(RETRY_AFTER).await;
                        continue;
                    }
                    None => continue,
                }
            }
            let id = id.as_deref().expect("created");
            let ui = Ui::new(Box::new(Out(self.sink.clone())), true, Predictor::new(PredictorConfig { mode: o.predict, ..Default::default() }))
                .with_graphics();
            let a = Attached::new(ui, o.cfg.client_id.clone());
            a.set_appearance(o.appearance.dark, o.appearance.colors.clone());
            let (ar, rtt) = (a.clone(), rtt.clone());
            let opt = AttachOptions {
                cols: z.cols.into(),
                rows: z.rows.into(),
                width: z.width.into(),
                height: z.height.into(),
                // The app is in front of the user: its keys must count.
                own: true,
                renderer: Some(Box::new(a.m.clone())),
                on_rtt: Some(Box::new(move |d| {
                    ar.rtt(d);
                    rtt.notify_one();
                })),
                ..Default::default()
            };
            let Some(attached) = self.or_fatal(osewa_client::timeout(REQUEST_TIMEOUT, self.link.attach(id, opt))).await else {
                a.m.close();
                return None;
            };
            match attached {
                Some(Ok(s)) => {
                    *a.s.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.clone());
                    return Some((s, a));
                }
                Some(Err(e)) => {
                    a.m.close();
                    let gone = e.is_ended() || e.is_code(wire::ERR_NOT_FOUND) || e.is_code(wire::ERR_ENDED);
                    if let Some(why) = fatal(&e).or_else(|| gone.then(|| format!("ended: {e}"))) {
                        self.sink.state(CLOSED, Some(&why));
                        return None;
                    }
                    osewa_client::sleep(RETRY_AFTER).await;
                }
                None => a.m.close(),
            }
        }
    }

    /// Why the link can't serve this session however long it tries: it
    /// stopped (incompatible), or its last dial was refused (unauthorized).
    /// Once a connection was up, Link::conn waits through dial errors
    /// rather than returning them, so it is read from the state.
    fn fatal_now(&self) -> Option<String> {
        let st = self.link.state();
        st.fin.as_ref().map(|e| fatal(e).unwrap_or_else(|| e.to_string())).or_else(|| st.err.as_ref().and_then(fatal))
    }

    /// Runs f until it finishes, or until the link turns fatal: then it
    /// reports CLOSED, stops the link and returns None.
    async fn or_fatal<T>(&self, f: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::pin!(f);
        loop {
            let mut w = self.link.watch();
            if let Some(why) = self.fatal_now() {
                self.sink.state(CLOSED, Some(&why));
                self.link.close().await;
                return None;
            }
            tokio::select! {
                v = &mut f => return Some(v),
                _ = w.changed() => {}
            }
        }
    }

    /// Reports the state as the link and the session change, the stats,
    /// and the session's live events, until the session ends.
    async fn follow(&self, s: &Session, rtt: &Notify, events: &mut Option<mpsc::Receiver<wire::EventMsg>>) {
        let (mut lw, mut sw) = (self.link.watch(), s.watch());
        let mut carrier: Option<&'static str> = None;
        let (mut stats_due, mut stats_at) = (false, None::<Instant>);
        loop {
            self.backlog.unacked.store(s.pending_bytes(), Ordering::SeqCst);
            if s.is_done() {
                return; // run's finish reports it
            }
            let st = self.link.state();
            // Incompatible stops the link; a refusal on a redial (the
            // device revoked, the host re-keyed) doesn't, but no redial
            // will get past it either.
            if let Some(why) = self.fatal_now() {
                self.sink.state(CLOSED, Some(&why));
                s.close();
                self.link.close().await;
                return;
            }
            if st.up && s.connected() {
                self.sink.state(CONNECTED, None);
            } else if st.paused {
                // Asked for, not lost: the app keeps the state it had.
            } else {
                self.sink.state(RECONNECTING, st.err.as_ref().map(|e| e.to_string()).as_deref());
            }
            if st.carrier.is_some() && st.carrier != carrier {
                carrier = st.carrier;
                stats_due = true;
                stats_at = None;
            }
            let wait = match (stats_due, carrier) {
                (true, Some(c)) => {
                    let next = stats_at.map_or(Duration::ZERO, |at| STATS_EVERY.saturating_sub(at.elapsed()));
                    if next.is_zero() {
                        self.sink.stats(s.rtt(), c);
                        (stats_due, stats_at) = (false, Some(Instant::now()));
                        None
                    } else {
                        Some(next)
                    }
                }
                _ => None,
            };
            tokio::select! {
                up = lw.changed() => if !up { std::future::pending::<()>().await },
                up = sw.changed() => if !up { std::future::pending::<()>().await },
                e = async { match events.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => match e {
                    Some(e) => self.sink.event(&event_json(&e)),
                    None => *events = None,
                },
                _ = rtt.notified() => stats_due = true,
                _ = async { match wait { Some(d) => osewa_client::sleep(d).await, None => std::future::pending().await } } => {}
            }
        }
    }
}

/// Sends the app's input in order. Text goes once a rune is whole; the
/// app being in front, a session someone else took is taken back first.
async fn input(cmds: &mut mpsc::UnboundedReceiver<Cmd>, s: &Session, a: &Attached, backlog: &Backlog) {
    let mut carry: Vec<u8> = Vec::new();
    while let Some(cmd) = cmds.recv().await {
        let n = match &cmd {
            Cmd::Send(b) => b.len(),
            Cmd::Paste(t) => t.len(),
            Cmd::Redraw => 0,
        };
        match cmd {
            Cmd::Send(b) => {
                carry.extend_from_slice(&b);
                let (text, keep) = osewa_vt::split_utf8(&carry);
                let (text, keep) = (String::from_utf8_lossy(text).into_owned(), keep.to_vec());
                carry = keep;
                if !text.is_empty() {
                    own(s, a).await;
                    a.predict(&text);
                    let _ = s.send_text(&text);
                }
            }
            Cmd::Paste(text) => {
                own(s, a).await;
                let _ = s.paste(&text);
            }
            Cmd::Redraw => a.redraw(),
        }
        // Handed to the session, whose own count (unacked) takes over.
        backlog.unacked.store(s.pending_bytes(), Ordering::SeqCst);
        backlog.queued.fetch_sub(n, Ordering::SeqCst);
    }
    std::future::pending::<()>().await
}

/// Takes the session back when another client owns it.
async fn own(s: &Session, a: &Attached) {
    if s.owner() == a.id {
        return;
    }
    if let Some(Ok(())) = osewa_client::timeout(REQUEST_TIMEOUT, s.own()).await {
        a.send_appearance().await;
    }
}

/// Sends the latest size whenever it changes.
async fn resize(size: &mut watch::Receiver<Size>, s: &Session) {
    while size.changed().await.is_ok() {
        let z = *size.borrow_and_update();
        let _ = osewa_client::timeout(REQUEST_TIMEOUT, s.resize(z.cols.into(), z.rows.into(), z.width.into(), z.height.into())).await;
    }
    std::future::pending::<()>().await
}

/// Why connecting can't work however often it is tried, as the CLOSED
/// detail; None when it may.
pub fn fatal(e: &Error) -> Option<String> {
    if e.is_incompatible() {
        return Some(format!("incompatible: {e}"));
    }
    if unauthorized(e) {
        return Some(format!("unauthorized: {e}"));
    }
    None
}

/// A TLS refusal: the server isn't the one pinned, or this device isn't
/// enrolled there (the server's alert). Neither fixes itself.
fn unauthorized(e: &Error) -> bool {
    // The errors come stringified through the carrier (auth/src/tls.rs
    // words the pin check; rustls the server's refusal).
    let s = e.to_string();
    s.contains("does not match pin") || s.contains("received fatal alert")
}

/// Why the session ended, for CLOSED.
fn ended(s: &Session) -> String {
    if let Some(code) = s.exit_code() {
        return format!("exit {code}");
    }
    match s.err() {
        Some(e) => fatal(&e).unwrap_or_else(|| if e.is_ended() { format!("ended: {e}") } else { e.to_string() }),
        None => "detached".into(),
    }
}

/// An event as the app gets it: the holder's, with its kind as "type".
pub fn event_json(e: &wire::EventMsg) -> String {
    let mut v = serde_json::to_value(e).unwrap_or_default();
    if let Some(m) = v.as_object_mut() {
        let kind = m.remove("kind").unwrap_or_default();
        m.insert("type".into(), kind);
    }
    v.to_string()
}
