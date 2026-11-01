//! Sessions: attach, snapshots, output, acks, input and resume.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use std::time::{Duration, Instant};

use osewa_wire as wire;
use serde_json::json;
use tokio::sync::{mpsc, Notify};
use tokio::task::AbortHandle;

use crate::conn::{Conn, Stream};
use crate::link::LinkInner;
use crate::rt::{self, Changed, Latch};
use crate::{Error, Result};

/// Ack batching: the holder only needs a rough idea of what we applied.
pub(crate) const ACK_BYTES: u64 = 32 << 10;
pub(crate) const ACK_DELAY: Duration = Duration::from_millis(100);

/// Bounds the events channel; a consumer that falls behind loses events
/// rather than stalling the connection.
const EVENT_BUFFER: usize = 256;

/// Bounds input payload sent and not yet acked (PROTOCOL.md "Input"): past
/// it input waits here, and goes out as INPUT_ACKs return.
pub(crate) const MAX_UNACKED: usize = 512 << 10;

/// A Renderer's failure: the stream can't be rendered, so the connection
/// ends with it.
pub type RendererError = Box<dyn std::error::Error + Send + Sync>;

/// Keeps a terminal from a session's stream; `osewa_mirror::Mirror` is one.
/// Its methods run in stream order on the connection's applier thread,
/// with the session locked, before the [`AttachOptions`] callbacks for the
/// same frame. An error ends the connection: the stream can't be rendered.
///
/// `snapshot_begin` before the previous snapshot's `snapshot_ready` means
/// that snapshot was abandoned (PROTOCOL.md "Lagging clients and
/// catch-up"): drop its partial decode and keep the current terminal until
/// the new READY. `end` runs once, when the session ends for this client.
pub trait Renderer: Send {
    fn snapshot_begin(&mut self, b: &wire::SnapshotBegin) -> Result<(), RendererError>;
    fn snapshot_data(&mut self, data: &[u8], history: bool) -> Result<(), RendererError>;
    fn snapshot_ready(&mut self) -> Result<(), RendererError>;
    fn snapshot_end(&mut self) -> Result<(), RendererError>;
    fn output(&mut self, data: &[u8]) -> Result<(), RendererError>;
    fn event(&mut self, e: &wire::EventMsg, live: bool) -> Result<(), RendererError>;
    fn end(&mut self);
}

type Cb<T> = Option<Box<T>>;
type DataFn = dyn FnMut(&[u8], bool) + Send;
type BytesFn = dyn FnMut(&[u8]) + Send;
type EventFn = dyn FnMut(&wire::EventMsg, bool) + Send;

/// A terminal's colors as far as they are known: None where it didn't say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermColors {
    pub foreground: Option<[u8; 3]>,
    pub background: Option<[u8; 3]>,
    pub cursor: Option<[u8; 3]>,
    pub palette: [Option<[u8; 3]>; 256],
}

impl Default for TermColors {
    fn default() -> TermColors {
        TermColors { foreground: None, background: None, cursor: None, palette: [None; 256] }
    }
}

/// Configures an attach.
#[derive(Default)]
pub struct AttachOptions {
    pub cols: i64,
    pub rows: i64,
    /// The text area in pixels, for programs that draw images; 0 when
    /// unknown.
    pub width: i64,
    pub height: i64,
    /// Claims ownership on attach. Resume never claims it: another client
    /// may have taken over meanwhile, and the holder keeps our ownership
    /// through its grace period anyway.
    pub own: bool,
    /// If set, gets the stream first (see [`Renderer`]).
    pub renderer: Option<Box<dyn Renderer>>,

    /// The raw stream, for an app that feeds it into its own libghostty.
    /// All run with the session locked: they must not call back into the
    /// Session. `on_snapshot_data` gets the bytes through READY with
    /// history false (before `on_ready`), then the history bytes;
    /// `on_snapshot_end` follows the last of them. A begin with history
    /// false (a catch-up screen) brings no history. An `on_snapshot_begin`
    /// before `on_ready` abandons the snapshot in progress.
    pub on_snapshot_begin: Cb<dyn FnMut(&wire::SnapshotBegin) + Send>,
    pub on_snapshot_data: Cb<DataFn>,
    pub on_ready: Cb<dyn FnMut() + Send>,
    pub on_snapshot_end: Cb<dyn FnMut() + Send>,
    /// Runs with each OUTPUT's bytes.
    pub on_output: Cb<BytesFn>,
    /// Runs with every applied event; live is false for events from a
    /// resume catch-up, whose effects must not fire.
    pub on_event: Cb<EventFn>,
    /// Runs after each frame that moved the position.
    pub on_applied: Cb<dyn FnMut() + Send>,
    /// Gets each round trip sample, timed from an input's send to its
    /// INPUT_ACK.
    pub on_rtt: Cb<dyn FnMut(Duration) + Send>,

    /// Returns no CREDIT for this stream until the caller reports bytes
    /// consumed with [`Session::credit`], for a consumer that relays the
    /// stream elsewhere and must not buffer it without bound. The callbacks
    /// must then not block: the holder's catch-up (screen snapshots instead
    /// of a queue) takes care of a consumer that falls behind. `on_counted`
    /// runs after the callbacks of each frame that counts toward credit,
    /// with its counted size: OUTPUT and EVENT payloads including their
    /// 8-byte end, SNAPSHOT_HISTORY, and SNAPSHOT_DATA of every snapshot but
    /// an attach's first (PROTOCOL.md "Flow control").
    pub manual_credit: bool,
    pub on_counted: Cb<dyn FnMut(usize) + Send>,
}

/// The PTY's line discipline as the holder last reported it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputMode {
    /// An input_mode event has arrived.
    pub known: bool,
    pub echo: bool,
    pub canonical: bool,
}

/// One attached session. It outlives the [`Conn`] it was attached on:
/// after a connection loss, [`Session::resume`] continues it on a new Conn
/// from where it left off. Clones share it.
#[derive(Clone)]
pub struct Session(pub(crate) Arc<SessionInner>);

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("id", &self.0.id).finish()
    }
}

impl PartialEq for Session {
    fn eq(&self, o: &Session) -> bool {
        Arc::ptr_eq(&self.0, &o.0)
    }
}

pub(crate) struct SessionInner {
    id: String,
    me: Weak<SessionInner>,
    manual_credit: bool,
    st: Mutex<State>,
    changed: Changed,
    lost: Notify,
    done: Latch,
    /// Resumes the session; set before the first attach.
    link: Option<Weak<LinkInner>>,
}

struct State {
    opt: AttachOptions,
    conn: Option<Conn>,
    stream: u32,
    /// Pending input was resent on conn.
    in_ready: bool,
    /// The next INPUT_ACK is the one the holder sends right after the
    /// attach, carrying the last seq it applied for our client_id.
    await_base: bool,
    /// Seqs are numbered above the holder's base.
    based: bool,

    snap_pos: u64,
    /// The snapshot in progress; gen is the screen's.
    snap_gen: u32,
    /// A snapshot is in progress.
    in_snap: bool,
    /// One reached READY, so output can be applied.
    screen: bool,
    /// The snapshot in progress carries history. A catch-up screen snapshot
    /// doesn't (PROTOCOL.md "Lagging clients and catch-up").
    snap_history: bool,
    /// The screen lacks history the log can't give back (a snapshot was cut
    /// short, or was screen-only); the next attach asks for a full snapshot.
    fresh: bool,
    gen: u32,
    epoch: String,
    /// End of the last record applied.
    pos: u64,
    /// Records ending at or before head are catch-up.
    head: u64,
    acked: u64,
    ack_timer: Option<AbortHandle>,
    /// Counted bytes received on this attach and not yet credited back
    /// through credit() (manual credit only).
    owed: usize,

    seq: u64,
    pending: VecDeque<wire::Input>,
    /// The payload sizes of pending's first sent.len() events, those sent on
    /// the current connection; unacked is their sum.
    sent: VecDeque<usize>,
    unacked: usize,

    owner: String,
    mode: InputMode,
    exit_code: Option<i64>,

    /// Times inputs sent once on the current connection; resent ones would
    /// understate the round trip.
    sent_at: HashMap<u64, Instant>,
    srtt: Duration,
    events: Option<mpsc::Sender<wire::EventMsg>>,
    events_rx: Option<mpsc::Receiver<wire::EventMsg>>,
    ended: bool,
    err: Option<Error>,
}

fn call<T: ?Sized>(cb: &mut Option<Box<T>>) -> Option<&mut T> {
    cb.as_deref_mut()
}

impl SessionInner {
    pub(crate) fn create(id: &str, opt: AttachOptions, link: Option<Weak<LinkInner>>) -> Session {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        Session(Arc::new_cyclic(|me| SessionInner {
            id: id.into(),
            me: me.clone(),
            manual_credit: opt.manual_credit,
            st: Mutex::new(State {
                opt,
                conn: None,
                stream: 0,
                in_ready: false,
                await_base: false,
                based: false,
                snap_pos: 0,
                snap_gen: 0,
                in_snap: false,
                screen: false,
                snap_history: false,
                fresh: false,
                gen: 0,
                epoch: String::new(),
                pos: 0,
                head: 0,
                acked: 0,
                ack_timer: None,
                owed: 0,
                seq: 0,
                pending: VecDeque::new(),
                sent: VecDeque::new(),
                unacked: 0,
                owner: String::new(),
                mode: InputMode::default(),
                exit_code: None,
                sent_at: HashMap::new(),
                srtt: Duration::ZERO,
                events: Some(tx),
                events_rx: Some(rx),
                ended: false,
                err: None,
            }),
            changed: Changed::default(),
            lost: Notify::new(),
            done: Latch::default(),
            link,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    fn arc(&self) -> Arc<SessionInner> {
        self.me.upgrade().expect("osewa: a session in use is alive")
    }

    /// Attaches on c, resuming from the last applied position when there is
    /// a whole screen to resume onto.
    pub(crate) async fn attach(&self, c: &Conn, own: bool) -> Result<()> {
        let stream = c.add_stream(self.arc())?;
        let req = {
            let st = self.lock();
            wire::Request {
                method: wire::METHOD_ATTACH.into(),
                session: self.id.clone(),
                stream,
                cols: st.opt.cols,
                rows: st.opt.rows,
                width: st.opt.width,
                height: st.opt.height,
                own,
                epoch: st.epoch.clone(),
                // Without a whole screen there is nothing to resume onto;
                // ask for a snapshot.
                position: (st.screen && !st.fresh).then_some(st.pos),
                ..Default::default()
            }
        };
        // abandoned: the attach gave up; dead: c died before the hook ran.
        // Both under s.st's lock order (s.st, then flags).
        let flags = Arc::new(Mutex::new((false, false)));
        let hook = {
            let (s, c, flags) = (self.arc(), c.clone(), flags.clone());
            Box::new(move |r: &wire::Response| {
                let mut st = s.lock();
                let mut fl = flags.lock().unwrap();
                if !r.error.is_empty() || fl.0 || st.ended {
                    return;
                }
                // c can fail between taking this response and here; its
                // lost() then found the session not yet on c, so installing
                // c would leave the session on a dead connection with
                // nothing to resume it. Once c.err is unset here, a later
                // fail() calls lost() after this hook releases s.st and finds
                // the session on c.
                if c.err().is_some() {
                    fl.1 = true;
                    return;
                }
                st.conn = Some(c.clone());
                s.changed.notify();
                st.stream = stream;
                st.in_ready = false;
                st.await_base = true;
                // A new stream starts with a fresh window; credit owed to the
                // old one would only inflate it.
                st.owed = 0;
                st.epoch = r.epoch.clone();
                st.owner = r.owner.clone();
                if r.resumed {
                    // Generations count snapshots per holder stream, and a
                    // resumed stream has had none yet.
                    st.gen = 0;
                }
                st.head = r.head.unwrap_or(st.pos);
            })
        };
        // Dropped before the answer (the caller gave up): undo as for an
        // error that isn't the server's.
        struct Abandon<'a> {
            s: &'a SessionInner,
            c: &'a Conn,
            stream: u32,
            flags: &'a Mutex<(bool, bool)>,
            armed: bool,
        }
        impl Drop for Abandon<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.s.abandon(self.c, self.stream, self.flags, &Error::Closed(None));
                }
            }
        }
        let mut guard = Abandon { s: self, c, stream, flags: &flags, armed: true };
        let mut res = c.request(req, Some(hook)).await.map(drop);
        guard.armed = false;
        if res.is_ok() {
            let _st = self.lock();
            if flags.lock().unwrap().1 {
                res = Err(c.err().unwrap_or(Error::Closed(None)));
            }
        }
        if let Err(e) = res {
            self.abandon(c, stream, &flags, &e);
            if e.is_code(wire::ERR_ENDED) || e.is_code(wire::ERR_NOT_FOUND) {
                self.finish(Some(Error::Ended(Some(e.to_string()))));
            } else if e.is_incompatible() {
                // No server of this build will ever serve us; retrying can't
                // help.
                self.finish(Some(e.clone()));
            }
            return Err(e);
        }
        // Input flows once the holder's post-attach INPUT_ACK says where its
        // seq stands; see resend.
        Ok(())
    }

    fn abandon(&self, c: &Conn, stream: u32, flags: &Mutex<(bool, bool)>, e: &Error) {
        c.drop_stream(stream);
        {
            let mut st = self.lock();
            flags.lock().unwrap().0 = true;
            if st.conn.as_ref() == Some(c) && st.stream == stream {
                st.conn = None;
                self.changed.notify();
            }
        }
        if !matches!(e, Error::Response { .. }) {
            // The holder may still attach the stream after we gave up; detach
            // it so it doesn't stream (or own) for nobody.
            c.notify(wire::Request { method: wire::METHOD_DETACH.into(), stream, ..Default::default() });
        }
    }

    fn detach_locked(st: &mut State) {
        st.conn = None;
        st.in_ready = false;
        st.sent_at.clear();
        st.sent.clear();
        st.unacked = 0;
        if st.in_snap {
            st.in_snap = false;
            st.fresh = true;
        }
    }

    fn signal_lost(&self) {
        if let Some(l) = self.link.as_ref().and_then(Weak::upgrade) {
            l.lost_session();
            return;
        }
        if self.link.is_none() {
            self.lost.notify_one();
        }
    }

    pub(crate) fn finish_pub(&self, err: Option<Error>) {
        self.finish(err)
    }

    fn finish(&self, err: Option<Error>) {
        let mut st = self.lock();
        self.end_locked(&mut st, err);
    }

    fn end_locked(&self, st: &mut State, err: Option<Error>) {
        if st.ended {
            return;
        }
        st.ended = true;
        st.err = err;
        st.conn = None;
        if let Some(l) = self.link.as_ref().and_then(Weak::upgrade) {
            l.forget(self.key());
        }
        if let Some(t) = st.ack_timer.take() {
            t.abort();
        }
        st.in_snap = false;
        if let Some(r) = st.opt.renderer.as_mut() {
            r.end();
        }
        st.events = None;
        self.done.set();
        self.changed.notify();
    }

    /// The session's key in a Link.
    pub(crate) fn key(&self) -> usize {
        self as *const SessionInner as usize
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.lock().ended
    }

    pub(crate) fn on(&self, c: &Conn) -> bool {
        self.lock().conn.as_ref() == Some(c)
    }

    /// Sends unacked input again on the current connection and lets new
    /// input go out. Sending under the lock keeps seqs in order.
    fn resend_locked(st: &mut State) {
        st.sent.clear();
        st.unacked = 0;
        st.in_ready = true;
        Self::send_more_locked(st);
    }

    /// Sends pending input while the unacked total allows. One event larger
    /// than the bound still goes when nothing else is in flight, or it would
    /// never go.
    fn send_more_locked(st: &mut State) {
        while st.in_ready && st.sent.len() < st.pending.len() {
            let Some(c) = st.conn.clone() else { return };
            let e = &st.pending[st.sent.len()];
            let f = wire::json(wire::TYPE_INPUT, st.stream, e);
            let n = f.payload.len();
            if st.unacked > 0 && st.unacked + n > MAX_UNACKED {
                return;
            }
            let seq = e.seq;
            // A failed send stays pending for the next resume.
            let _ = c.send(f);
            st.sent.push_back(n);
            st.unacked += n;
            st.sent_at.insert(seq, Instant::now());
            // An INPUT_ACK shows the link carries what was typed; if none
            // comes, the connection turns suspect within about a second.
            c.probe_now();
        }
    }

    /// Applies f with the session locked.
    fn apply(&self, st: &mut State, c: &Conn, f: wire::Frame) -> Result<()> {
        let rerr = |e: RendererError| Error::Other(e.to_string());
        let derr = |e: wire::Error| Error::Other(e.to_string());
        match f.ty {
            wire::TYPE_SNAPSHOT_BEGIN => {
                let b: wire::SnapshotBegin = f.decode().map_err(derr)?;
                // A BEGIN before the previous snapshot's READY abandons that
                // one (PROTOCOL.md "Lagging clients and catch-up"): position
                // and generation stay the old ones until the new READY.
                if let Some(r) = st.opt.renderer.as_mut() {
                    r.snapshot_begin(&b).map_err(rerr)?;
                }
                st.in_snap = true;
                st.snap_gen = b.generation;
                st.snap_pos = b.position;
                st.epoch = b.epoch.clone();
                st.snap_history = b.history.unwrap_or(true);
                if let Some(cb) = call(&mut st.opt.on_snapshot_begin) {
                    cb(&b);
                }
            }
            wire::TYPE_SNAPSHOT_DATA | wire::TYPE_SNAPSHOT_HISTORY => {
                if !st.in_snap {
                    return Err(Error::Other("osewa: snapshot data outside a snapshot".into()));
                }
                let history = f.ty == wire::TYPE_SNAPSHOT_HISTORY;
                if let Some(r) = st.opt.renderer.as_mut() {
                    r.snapshot_data(&f.payload, history).map_err(rerr)?;
                }
                if let Some(cb) = call(&mut st.opt.on_snapshot_data) {
                    cb(&f.payload, history);
                }
                // An attach's first snapshot sends its screen uncounted.
                if history || st.snap_gen != 0 {
                    self.counted(st, f.payload.len());
                }
            }
            wire::TYPE_SNAPSHOT_READY => {
                if !st.in_snap {
                    return Err(Error::Other("osewa: SNAPSHOT_READY outside a snapshot".into()));
                }
                if let Some(r) = st.opt.renderer.as_mut() {
                    r.snapshot_ready().map_err(rerr)?;
                }
                st.screen = true;
                // Position and generation move only now: until READY the old
                // screen, and so the old position and generation, are what we
                // ack and resume from.
                st.pos = st.snap_pos;
                st.acked = st.snap_pos;
                st.gen = st.snap_gen;
                if let Some(tr) = &c.0.tr {
                    tr.event("ready", &[("stream", json!(st.stream)), ("generation", json!(st.gen))]);
                }
                st.head = st.head.max(st.pos);
                // Tell the holder this screen is up: in catch-up it paces the
                // next screen snapshot on what the client has.
                let _ = c.send(wire::ack(st.stream, st.gen, st.pos));
                if let Some(cb) = call(&mut st.opt.on_ready) {
                    cb();
                }
                Self::applied(st);
                self.changed.notify();
            }
            wire::TYPE_SNAPSHOT_END => {
                if !st.in_snap {
                    return Err(Error::Other("osewa: SNAPSHOT_END outside a snapshot".into()));
                }
                st.in_snap = false;
                if let Some(r) = st.opt.renderer.as_mut() {
                    r.snapshot_end().map_err(rerr)?;
                }
                st.fresh = !st.snap_history;
                if let Some(cb) = call(&mut st.opt.on_snapshot_end) {
                    cb();
                }
                self.changed.notify();
            }
            wire::TYPE_OUTPUT => {
                let (end, data) = wire::split_end(&f.payload).map_err(derr)?;
                if !st.screen {
                    return Err(Error::Other("osewa: OUTPUT before a snapshot".into()));
                }
                if end <= st.pos {
                    self.counted(st, f.payload.len());
                    return Ok(());
                }
                if let Some(r) = st.opt.renderer.as_mut() {
                    r.output(data).map_err(rerr)?;
                }
                st.pos = end;
                if let Some(cb) = call(&mut st.opt.on_output) {
                    cb(data);
                }
                Self::applied(st);
                self.counted(st, f.payload.len());
                self.changed.notify();
                self.ack(st, c);
            }
            wire::TYPE_EVENT => {
                let (end, data) = wire::split_end(&f.payload).map_err(derr)?;
                if !st.screen {
                    return Err(Error::Other("osewa: EVENT before a snapshot".into()));
                }
                if end <= st.pos {
                    self.counted(st, f.payload.len());
                    return Ok(());
                }
                let e: wire::EventMsg = wire::decode_json(data).map_err(|e| Error::Other(format!("wire: frame 0x{:02x}: {e}", f.ty)))?;
                st.pos = end;
                Self::apply_event(st, &e);
                let live = end > st.head;
                if let Some(r) = st.opt.renderer.as_mut() {
                    r.event(&e, live).map_err(rerr)?;
                }
                if live {
                    if let Some(tx) = &st.events {
                        let _ = tx.try_send(e.clone());
                    }
                }
                if let Some(cb) = call(&mut st.opt.on_event) {
                    cb(&e, live);
                }
                Self::applied(st);
                self.counted(st, f.payload.len());
                self.changed.notify();
                self.ack(st, c);
            }
            wire::TYPE_INPUT_ACK => {
                let seq = wire::parse_u64(&f.payload).map_err(derr)?;
                if let Some(tr) = &c.0.tr {
                    tr.event("input_ack", &[("stream", json!(st.stream)), ("seq", json!(seq))]);
                }
                if st.await_base && !st.based {
                    // The holder keeps seq per client_id across processes, so
                    // input queued before we knew its base is renumbered above
                    // it.
                    st.await_base = false;
                    st.based = true;
                    for (i, e) in st.pending.iter_mut().enumerate() {
                        e.seq = seq + 1 + i as u64;
                    }
                    st.seq = seq + st.pending.len() as u64;
                    Self::resend_locked(st);
                } else {
                    Self::sample_rtt(st, seq);
                    let mut i = 0;
                    while i < st.pending.len() && st.pending[i].seq <= seq {
                        i += 1;
                    }
                    for n in st.sent.drain(..i.min(st.sent.len())) {
                        st.unacked -= n;
                    }
                    st.pending.drain(..i);
                    if st.await_base {
                        st.await_base = false;
                        Self::resend_locked(st);
                    } else {
                        Self::send_more_locked(st);
                    }
                }
                self.changed.notify();
            }
            wire::TYPE_STREAM_END => {
                let e: wire::StreamEnd = f.decode().map_err(derr)?;
                if e.exit_code.is_some() {
                    st.exit_code = e.exit_code;
                }
                c.drop_stream(st.stream);
                match e.reason.as_str() {
                    wire::END_DETACHED | wire::END_EXITED => self.end_locked(st, None),
                    wire::END_ERROR => {
                        // The server lost its holder connection; the session
                        // itself may be fine.
                        Self::detach_locked(st);
                        self.signal_lost();
                        self.changed.notify();
                    }
                    r => self.end_locked(st, Some(Error::Other(format!("osewa: stream ended: {r}")))),
                }
            }
            t => return Err(Error::Other(format!("osewa: unexpected frame 0x{t:02x} on a stream"))),
        }
        Ok(())
    }

    fn applied(st: &mut State) {
        if let Some(cb) = call(&mut st.opt.on_applied) {
            cb();
        }
    }

    /// Accounts n counted bytes the caller now has, under manual credit.
    fn counted(&self, st: &mut State, n: usize) {
        if !self.manual_credit || n == 0 {
            return;
        }
        st.owed += n;
        if let Some(cb) = call(&mut st.opt.on_counted) {
            cb(n);
        }
    }

    /// Applies an event's state, live or not. Effects are the caller's
    /// business.
    fn apply_event(st: &mut State, e: &wire::EventMsg) {
        match e.kind.as_str() {
            wire::EVENT_OWNER => st.owner = e.owner.clone(),
            wire::EVENT_EXIT => st.exit_code = e.code,
            wire::EVENT_INPUT_MODE => {
                st.mode.known = true;
                if let Some(v) = e.echo {
                    st.mode.echo = v;
                }
                if let Some(v) = e.canonical {
                    st.mode.canonical = v;
                }
            }
            _ => {}
        }
    }

    /// Sends an ACK once enough is unacked; otherwise it rides with the
    /// CREDIT due for these bytes (credit_sent), in the same write and
    /// without a timer of its own, or goes when its timer fires.
    fn ack(&self, st: &mut State, c: &Conn) {
        if st.pos - st.acked >= ACK_BYTES {
            st.acked = st.pos;
            let _ = c.send(wire::ack(st.stream, st.gen, st.pos));
            return;
        }
        if st.ack_timer.is_none() && !c.credit_due(st.stream) {
            let s = self.arc();
            st.ack_timer = Some(rt::spawn(async move {
                tokio::time::sleep(ACK_DELAY).await;
                s.flush_ack();
            }));
        }
    }

    fn flush_ack(&self) {
        let mut st = self.lock();
        st.ack_timer = None;
        let Some(c) = st.conn.clone() else { return };
        if st.pos == st.acked {
            return;
        }
        st.acked = st.pos;
        let _ = c.send(wire::ack(st.stream, st.gen, st.pos));
    }

    /// Takes a round trip sample from the INPUT_ACK for seq.
    fn sample_rtt(st: &mut State, seq: u64) {
        let at = st.sent_at.get(&seq).copied();
        st.sent_at.retain(|&k, _| k > seq);
        let Some(at) = at else { return };
        let d = at.elapsed();
        st.srtt = if st.srtt.is_zero() {
            d
        } else {
            Duration::from_nanos((st.srtt.as_nanos() as i128 + (d.as_nanos() as i128 - st.srtt.as_nanos() as i128) / 8) as u64)
        };
        if let Some(cb) = call(&mut st.opt.on_rtt) {
            cb(d);
        }
    }

    fn input(&self, mut e: wire::Input) -> Result<()> {
        let mut st = self.lock();
        if st.ended {
            return Err(Error::Ended(None));
        }
        st.seq += 1;
        e.seq = st.seq;
        st.pending.push_back(e);
        Self::send_more_locked(&mut st);
        Ok(())
    }
}

impl Stream for SessionInner {
    fn handle(&self, c: &Conn, f: wire::Frame) -> Result<()> {
        let mut st = self.lock();
        if st.conn.as_ref() != Some(c) || st.stream != f.stream {
            return Ok(());
        }
        self.apply(&mut st, c, f)
    }

    fn lost(&self, c: &Conn) {
        let mut st = self.lock();
        if st.conn.as_ref() != Some(c) {
            return;
        }
        Self::detach_locked(&mut st);
        self.signal_lost();
        self.changed.notify();
    }

    fn suspect(&self) {
        self.signal_lost();
    }

    fn credits_on_receipt(&self) -> bool {
        !self.manual_credit
    }

    fn unacked_since(&self, after: Instant) -> Option<Instant> {
        self.lock().sent_at.values().copied().filter(|&t| t > after).min()
    }

    fn credit_sent(&self, c: &Conn) {
        // This runs on the network thread, which must never wait on an
        // apply (a slow renderer holds the lock): a busy session gets its
        // ACK a little later instead.
        let mut st = match self.st.try_lock() {
            Ok(st) => st,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                let (s, c) = (self.arc(), c.clone());
                rt::spawn(async move {
                    tokio::time::sleep(ACK_DELAY).await;
                    Stream::credit_sent(&*s, &c);
                });
                return;
            }
        };
        if st.conn.as_ref() != Some(c) || st.pos == st.acked {
            return;
        }
        if let Some(t) = st.ack_timer.take() {
            t.abort();
        }
        st.acked = st.pos;
        let _ = c.send(wire::ack(st.stream, st.gen, st.pos));
    }
}

impl Session {
    /// The session id.
    pub fn id(&self) -> &str {
        &self.0.id
    }

    /// Re-attaches on c from the last applied position, falling back to a
    /// fresh snapshot when the holder no longer has it, and resends unacked
    /// input. It never claims ownership. It fails with [`Error::Ended`]
    /// when the session is gone.
    pub async fn resume(&self, c: &Conn) -> Result<()> {
        let (old, old_stream) = {
            let mut st = self.0.lock();
            if st.ended {
                return Err(Error::Ended(None));
            }
            let old = st.conn.clone();
            SessionInner::detach_locked(&mut st);
            (old, st.stream)
        };
        if let Some(old) = old {
            old.drop_stream(old_stream);
        }
        self.0.attach(c, false).await
    }

    /// Waits for a sign that the session should be resumed on a new Conn
    /// (never for a session attached through a Link, which resumes it
    /// itself): the connection died or turned suspect (a ping went
    /// unanswered), or the server ended the stream with an error. A suspect
    /// connection may still be attached; resume moves off it. One sign is
    /// kept until waited for. [`Session::done`], not this, reports a session
    /// that is over.
    pub async fn lost(&self) {
        self.0.lost.notified().await
    }

    /// Waits until the stream ends for good: the child exited, the session
    /// was killed or detached, or a resume found it gone. A lost connection
    /// does not end a session.
    pub async fn done(&self) {
        self.0.done.wait().await
    }

    pub fn is_done(&self) -> bool {
        self.0.done.is_set()
    }

    /// Why the session ended, or None for an exit or a detach.
    pub fn err(&self) -> Option<Error> {
        self.0.lock().err.clone()
    }

    /// Live events. Events from a snapshot or a resume catch-up never
    /// appear, so their effects (bell, clipboard) never fire twice. The
    /// channel closes when the session ends. There is one receiver: the
    /// first call takes it.
    pub fn take_events(&self) -> Option<mpsc::Receiver<wire::EventMsg>> {
        self.0.lock().events_rx.take()
    }

    /// The epoch and the end of the last applied record.
    pub fn position(&self) -> (String, u64) {
        let st = self.0.lock();
        (st.epoch.clone(), st.pos)
    }

    /// The session's owner as last reported.
    pub fn owner(&self) -> String {
        self.0.lock().owner.clone()
    }

    /// Hears the session's changes: attached on a connection or off it,
    /// input acked, owner, ended. Read the state ([`Session::connected`],
    /// [`Session::is_done`] ...) after each.
    pub fn watch(&self) -> crate::Watch {
        self.0.changed.public()
    }

    /// Reports whether the session is attached on a live connection.
    pub fn connected(&self) -> bool {
        self.0.lock().conn.is_some()
    }

    /// The child's exit code once known.
    pub fn exit_code(&self) -> Option<i64> {
        self.0.lock().exit_code
    }

    /// The smoothed input round trip, zero before the first sample.
    pub fn rtt(&self) -> Duration {
        self.0.lock().srtt
    }

    /// The PTY's echo and canonical flags as last reported.
    pub fn input_mode(&self) -> InputMode {
        self.0.lock().mode
    }

    /// How many input events await an INPUT_ACK.
    pub fn pending(&self) -> usize {
        self.0.lock().pending.len()
    }

    /// Ends the session locally, without detaching.
    pub fn close(&self) {
        let mut st = self.0.lock();
        if let Some(c) = &st.conn {
            c.drop_stream(st.stream);
        }
        self.0.end_locked(&mut st, None);
    }

    /// Sends a key event in protocol names (PROTOCOL.md "Input"): code is a
    /// W3C KeyboardEvent.code, mods any of shift, ctrl, alt and super, text
    /// what the key types, action "press" (or ""), "repeat" or "release".
    /// The holder encodes it for the terminal's current modes, and drops a
    /// key it can't. Input is queued until the holder acks it, and resent on
    /// resume; while disconnected it is only queued.
    pub fn key(&self, code: &str, mods: &[&str], text: &str, action: &str) -> Result<()> {
        for m in mods {
            if !matches!(*m, "shift" | "ctrl" | "alt" | "super") {
                return Err(Error::Other(format!("osewa: key modifier {}", wire::quote(m))));
            }
        }
        let action = match action {
            "" | "press" => "",
            "repeat" | "release" => action,
            _ => return Err(Error::Other(format!("osewa: key action {}", wire::quote(action)))),
        };
        if code.is_empty() {
            return Err(Error::Other("osewa: key without a code".into()));
        }
        self.0.input(wire::Input {
            ty: "key".into(),
            key: code.into(),
            mods: mods.iter().map(|m| m.to_string()).collect(),
            text: text.into(),
            action: action.into(),
            ..Default::default()
        })
    }

    /// Writes text to the PTY as-is.
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.0.input(wire::Input { ty: "text".into(), text: text.into(), ..Default::default() })
    }

    /// Sends pasted text; the holder brackets it if the program asked.
    pub fn paste(&self, text: &str) -> Result<()> {
        self.0.input(wire::Input { ty: "paste".into(), text: text.into(), ..Default::default() })
    }

    /// Reports a focus change.
    pub fn focus(&self, focused: bool) -> Result<()> {
        self.0.input(wire::Input { ty: "focus".into(), focused, ..Default::default() })
    }

    /// Waits until the holder has acked every input sent so far, across any
    /// resumes in between.
    pub async fn flush(&self) -> Result<()> {
        loop {
            let w = {
                let st = self.0.lock();
                if st.pending.is_empty() {
                    return Ok(());
                }
                if st.ended {
                    return Err(Error::Ended(None));
                }
                self.0.changed.watch()
            };
            w.changed().await;
        }
    }

    /// Returns n counted bytes the caller consumed (manual credit; see
    /// [`AttachOptions::on_counted`]). Credit is per attach: after a resume,
    /// bytes from before it are no longer owed, and credit never returns
    /// more than the current attach received.
    pub fn credit(&self, n: usize) {
        let mut st = self.0.lock();
        let n = n.min(st.owed);
        let Some(c) = st.conn.clone() else { return };
        if n == 0 {
            return;
        }
        st.owed -= n;
        let _ = c.send(wire::credit(st.stream, n as u32));
    }

    async fn request(&self, mut req: wire::Request) -> Result<wire::Response> {
        let c = {
            let st = self.0.lock();
            let Some(c) = st.conn.clone() else { return Err(Error::Closed(None)) };
            req.stream = st.stream;
            c
        };
        c.request(req, None).await
    }

    /// Takes ownership: the PTY follows this client's size from now on.
    pub async fn own(&self) -> Result<()> {
        let r = self.request(wire::Request { method: wire::METHOD_OWN.into(), ..Default::default() }).await?;
        self.0.lock().owner = r.owner;
        Ok(())
    }

    /// Asks for a new size, in cells and (0 when unknown) pixels. Only the
    /// owner's applies; a renderer follows the holder's resize event, not
    /// this call.
    pub async fn resize(&self, cols: i64, rows: i64, width: i64, height: i64) -> Result<()> {
        {
            let mut st = self.0.lock();
            st.opt.cols = cols;
            st.opt.rows = rows;
            st.opt.width = width;
            st.opt.height = height;
        }
        let req = wire::Request { method: wire::METHOD_RESIZE.into(), cols, rows, width, height, ..Default::default() };
        self.request(req).await.map(drop)
    }

    /// Tells the holder whether this client's terminal is dark or light, for
    /// programs that ask (DSR 996, mode 2031). Only the owner's applies.
    pub async fn color_scheme(&self, dark: bool) -> Result<()> {
        let s = if dark { "dark" } else { "light" };
        let req = wire::Request { method: wire::METHOD_COLOR_SCHEME.into(), color_scheme: s.into(), ..Default::default() };
        self.request(req).await.map(drop)
    }

    /// Tells the holder this client's terminal colors, which programs'
    /// color queries are answered with. Only the owner's apply; None, and
    /// a palette shorter than 256, leave the holder's as they are.
    pub async fn colors(&self, c: &TermColors) -> Result<()> {
        let hex = |c: Option<[u8; 3]>| c.map(wire::rgb_hex).unwrap_or_default();
        let palette = if c.palette.iter().all(Option::is_none) { Vec::new() } else { (0..256).map(|i| hex(c.palette.get(i).copied().flatten())).collect() };
        let req = wire::Request {
            method: wire::METHOD_COLORS.into(),
            foreground: hex(c.foreground),
            background: hex(c.background),
            cursor: hex(c.cursor),
            palette,
            ..Default::default()
        };
        self.request(req).await.map(drop)
    }

    /// Answers the program's oldest clipboard read (OSC 52) with data; empty
    /// tells it the clipboard is empty. Only the owner's apply.
    pub async fn clipboard(&self, data: &[u8]) -> Result<()> {
        let req = wire::Request { method: wire::METHOD_CLIPBOARD.into(), data: data.to_vec(), ..Default::default() };
        self.request(req).await.map(drop)
    }

    /// Detaches the stream. The session keeps running on the host.
    pub async fn detach(&self) -> Result<()> {
        let r = self.request(wire::Request { method: wire::METHOD_DETACH.into(), ..Default::default() }).await;
        let mut st = self.0.lock();
        if let Some(c) = &st.conn {
            c.drop_stream(st.stream);
        }
        self.0.end_locked(&mut st, None);
        r.map(drop)
    }
}
