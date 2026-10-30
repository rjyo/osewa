//! Connections (Go's client/client.go): HELLO and WELCOME, requests, the
//! reader, applier, writer and keepalive, and credit on receipt.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use osewa_carrier::ws::STATUS_PROTOCOL_ERROR;
use osewa_wire as wire;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::AbortHandle;

use crate::dial::{self, DialStats, Transport};
use crate::rt::{self, Latch};
use crate::session::{Session, SessionInner};
use crate::trace::{self, Tracer};
use crate::{Config, Error, Result, GHOSTTY_COMMIT, SNAPSHOT_VERSION};

/// Flow control (PROTOCOL.md "Flow control"): credit counted bytes as they
/// arrive, before applying them.
pub(crate) const CREDIT_BYTES: u32 = 16 << 10;
pub(crate) const CREDIT_DELAY: Duration = Duration::from_millis(50);

/// Bounds bytes received but not yet applied. Past it credit is held back,
/// so a client that can't keep up makes the holder queue (and eventually
/// resnapshot) instead of us buffering without end.
const APPLY_MAX: usize = 16 << 20;

/// Bounds the frames the writer sends in one write.
const BATCH_BYTES: usize = 64 << 10;

/// Paces keepalive pings. A half-open connection (laptop sleep, network
/// switch, NAT timeout) never reports an error, so two pings without a pong
/// count as dead.
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// The least a ping waits for its pong before the connection is suspect;
/// the wait is at least 3 ping round trips.
const SUSPECT_MIN: Duration = Duration::from_secs(1);

/// Describes a new session.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub cols: i64,
    pub rows: i64,
    /// Empty runs the user's login shell.
    pub argv: Vec<String>,
    /// Empty is the home directory.
    pub cwd: String,
    /// KEY=VALUE, added to the server's environment.
    pub env: Vec<String>,
}

/// One connection to a server. Clones share it; it is gone once closed or
/// lost, and stays so: a [`Link`](crate::Link) dials new ones.
#[derive(Clone)]
pub struct Conn(pub(crate) Arc<Inner>);

impl PartialEq for Conn {
    fn eq(&self, o: &Conn) -> bool {
        Arc::ptr_eq(&self.0, &o.0)
    }
}

impl Eq for Conn {}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn").field("carrier", &self.0.carrier).field("client_id", &self.0.cfg.client_id).finish()
    }
}

pub(crate) struct Inner {
    /// What the connection runs on: "ws" or "quic".
    carrier: &'static str,
    pub(crate) cfg: Config,
    pub(crate) tr: Option<Arc<Tracer>>,
    welcome: OnceLock<wire::Welcome>,
    welcomed: Latch,
    st: Mutex<State>,
    done: Latch,
    /// The runtime tasks stop.
    cancel: Latch,
    suspect: Latch,
    /// Asks keepalive to watch for an answer to input just sent.
    probe: Notify,
    /// Pings sent, which the tests bound.
    pub(crate) pings: AtomicU64,
    close: mpsc::UnboundedSender<CloseCmd>,

    // Outgoing frames go through a queue drained by one writer, so neither
    // the applier nor a caller holding a session lock ever blocks on the
    // network.
    wq: Mutex<WriteQ>,
    wake_writer: Notify,

    // Received frames wait here for the applier, so a slow apply (or a slow
    // on_output) never delays reading, pongs or credit.
    aq: Mutex<ApplyQ>,
    acond: Condvar,
    amax: usize,
    ping: Duration,
}

struct State {
    next_id: u64,
    next_stream: u32,
    pending: HashMap<u64, Call>,
    streams: HashMap<u32, Arc<dyn Stream>>,
    err: Option<Error>,
    credits: HashMap<u32, Credit>,
    /// Credit withheld while the apply queue is over amax.
    held: bool,
}

#[derive(Default)]
pub(crate) struct WriteQ {
    q: VecDeque<Vec<u8>>,
    /// Channel DATA and EOF, written only when q is empty, so an upload
    /// never delays a keystroke by more than one frame.
    bulk: VecDeque<Vec<u8>>,
    closed: bool,
}

impl WriteQ {
    pub fn push(&mut self, f: &wire::Frame) {
        let b = f.marshal();
        if f.ty == wire::TYPE_DATA || f.ty == wire::TYPE_EOF {
            self.bulk.push_back(b);
        } else {
            self.q.push_back(b);
        }
    }

    /// The next frames to write together: anything before channel DATA and
    /// EOF (PROTOCOL.md "In flight"), up to BATCH_BYTES; else one of those,
    /// which are large and must not hold back a keystroke queued after.
    pub fn pop_batch(&mut self, out: &mut Vec<Vec<u8>>) {
        let mut n = 0;
        while n < BATCH_BYTES {
            let Some(b) = self.q.pop_front() else { break };
            n += b.len();
            out.push(b);
        }
        if out.is_empty() {
            out.extend(self.bulk.pop_front());
        }
    }
}

#[derive(Default)]
struct ApplyQ {
    q: VecDeque<Inbound>,
    bytes: usize,
    closed: bool,
}

enum Inbound {
    Frame(wire::Frame),
    /// The read failed: fail once everything before it is applied.
    Err(Error),
}

enum CloseCmd {
    Graceful(u16, &'static str, Option<oneshot::Sender<()>>),
    Now,
}

#[derive(Default)]
struct Credit {
    n: u32,
    timer: Option<AbortHandle>,
    /// The stream's current snapshot counts its SNAPSHOT_DATA. Only a
    /// stream's first snapshot (generation 0) doesn't.
    data_counted: bool,
}

pub(crate) type Hook = Box<dyn FnOnce(&wire::Response) + Send>;

struct Call {
    tx: oneshot::Sender<Result<wire::Response>>,
    /// Runs on the applier before any later frame is dispatched, so stream
    /// state set from a response is in place before that stream's first
    /// data frame.
    hook: Option<Hook>,
}

/// What a stream id on a Conn leads to: a session or a channel.
pub(crate) trait Stream: Send + Sync {
    /// Applies one frame for the stream, on the applier.
    fn handle(&self, c: &Conn, f: wire::Frame) -> Result<()>;
    /// c died under the stream.
    fn lost(&self, c: &Conn);
    /// c turned suspect.
    fn suspect(&self);
    /// Credited on receipt: a session without manual credit. Channels are
    /// credited as their reader consumes.
    fn credits_on_receipt(&self) -> bool;
    /// When the oldest input sent after `after` and not yet acked went out.
    fn unacked_since(&self, _after: Instant) -> Option<Instant> {
        None
    }
    /// A CREDIT for the stream was just queued after its delay, for what is
    /// due to go with it.
    fn credit_sent(&self, _c: &Conn) {}
}

impl Conn {
    /// Connects and sends HELLO. It returns without waiting for WELCOME:
    /// requests may follow HELLO at once (PROTOCOL.md "Frames"), so the
    /// first request, or a resume's attach, goes out in the same flight.
    /// [`Conn::welcome`] waits for it.
    pub async fn dial(mut cfg: Config) -> Result<Conn> {
        // Without a pinned config, keystrokes would go to whoever presents
        // a certificate.
        if cfg.tls.is_none() {
            return Err(Error::Other("osewa: Config.tls is required (osewa_auth::client_config pins the server)".into()));
        }
        if cfg.carrier.is_empty() {
            cfg.carrier = std::env::var("OSEWA_CARRIER").unwrap_or_default();
        }
        let t0 = Instant::now();
        rt::run_local(move || async move {
            let d = dial::dial(&cfg).await?;
            Ok(Conn::start(d.transport, cfg, d.carrier, t0, d.stats))
        })
        .await
    }

    /// Runs a connection on a carrier: HELLO out, and the reader, applier,
    /// writer and keepalive. On the runtime thread.
    fn start(t: Transport, cfg: Config, carrier: &'static str, t0: Instant, stats: Option<DialStats>) -> Conn {
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let tr = match &cfg.knobs.trace_dir {
            Some(d) => trace::open_dir(&d.to_string_lossy()),
            None => trace::open(),
        };
        let c = Conn(Arc::new(Inner {
            carrier,
            amax: cfg.knobs.apply_max.unwrap_or(APPLY_MAX),
            ping: cfg.knobs.ping.unwrap_or(PING_INTERVAL),
            cfg,
            tr,
            welcome: OnceLock::new(),
            welcomed: Latch::default(),
            st: Mutex::new(State {
                next_id: 0,
                next_stream: 1,
                pending: HashMap::new(),
                streams: HashMap::new(),
                err: None,
                credits: HashMap::new(),
                held: false,
            }),
            done: Latch::default(),
            cancel: Latch::default(),
            suspect: Latch::default(),
            probe: Notify::new(),
            pings: AtomicU64::new(0),
            close: close_tx,
            wq: Mutex::default(),
            wake_writer: Notify::new(),
            aq: Mutex::default(),
            acond: Condvar::new(),
        }));
        if let Some(tr) = &c.0.tr {
            let ms = t0.elapsed().as_millis() as i64;
            match stats {
                Some(s) => tr.event("dial", &[("ms", json!(ms)), ("attempts", json!(s.attempts)), ("winner", json!(s.winner))]),
                None => tr.event("dial", &[("ms", json!(ms)), ("carrier", json!(carrier))]),
            }
        }
        let _ = c.send(wire::json(
            wire::TYPE_HELLO,
            0,
            &wire::Hello {
                version: wire::VERSION,
                client_id: c.0.cfg.client_id.clone(),
                name: c.0.cfg.name.clone(),
                features: vec![wire::FEATURE_FORWARD.into(), wire::FEATURE_EXEC.into()],
            },
        ));
        let t = Rc::new(t);
        tokio::task::spawn_local(c.clone().read_loop(t.clone()));
        tokio::task::spawn_local(c.clone().write_loop(t.clone()));
        tokio::task::spawn_local(c.clone().keepalive(t.clone()));
        tokio::task::spawn_local(async move {
            while let Some(cmd) = close_rx.recv().await {
                match cmd {
                    CloseCmd::Now => t.close_now(),
                    CloseCmd::Graceful(code, reason, reply) => {
                        t.close(code, reason).await;
                        if let Some(r) = reply {
                            let _ = r.send(());
                        }
                    }
                }
            }
        });
        let ac = c.clone();
        std::thread::Builder::new().name("osewa-apply".into()).spawn(move || ac.apply_loop()).expect("osewa: applier thread");
        c
    }

    /// What the connection runs on: "ws" or "quic".
    pub fn carrier(&self) -> &'static str {
        self.0.carrier
    }

    /// The id this connection sent in HELLO.
    pub fn client_id(&self) -> &str {
        &self.0.cfg.client_id
    }

    /// Waits for the server's WELCOME. It fails, as the whole connection
    /// does, if the server can't serve this client: another protocol or
    /// libghostty ([`Error::Incompatible`]). A WELCOME that came in stays
    /// the answer after the connection ends.
    pub async fn welcome(&self) -> Result<wire::Welcome> {
        let (welcomed, done) = (self.0.welcomed.wait(), self.0.done.wait());
        tokio::select! {
            biased;
            _ = welcomed => {}
            _ = done => {
                if !self.0.welcomed.is_set() {
                    return Err(self.err().unwrap_or(Error::Closed(None)));
                }
            }
        }
        Ok(self.0.welcome.get().cloned().unwrap_or_default())
    }

    /// Reports whether the server offers feature (wire::FEATURE_FORWARD,
    /// FEATURE_EXEC). It waits for WELCOME, as [`Conn::welcome`] does.
    pub async fn has(&self, feature: &str) -> Result<bool> {
        Ok(wire::has_feature(&self.welcome().await?.features, feature))
    }

    /// Waits until the connection is gone.
    pub async fn done(&self) {
        self.0.done.wait().await
    }

    pub fn is_done(&self) -> bool {
        self.0.done.is_set()
    }

    /// Why the connection ended, once it has.
    pub fn err(&self) -> Option<Error> {
        self.0.st.lock().unwrap().err.clone()
    }

    /// Waits until a ping went unanswered for longer than the link explains:
    /// the connection may be half-open. Attached sessions see it as lost.
    pub async fn suspect(&self) {
        self.0.suspect.wait().await
    }

    pub fn is_suspect(&self) -> bool {
        self.0.suspect.is_set()
    }

    /// Closes the connection with the closing handshake. Attached sessions
    /// stay resumable.
    pub async fn close(&self) {
        self.fail(Error::Closed(None));
        let (tx, rx) = oneshot::channel();
        if self.0.close.send(CloseCmd::Graceful(osewa_carrier::ws::STATUS_NORMAL, "", Some(tx))).is_ok() {
            let _ = rx.await;
        }
    }

    /// Closes without the closing handshake, which a half-open connection
    /// would never answer.
    pub fn close_now(&self) {
        self.fail(Error::Closed(None));
        let _ = self.0.close.send(CloseCmd::Now);
    }

    pub(crate) fn fail(&self, err: Error) {
        let (pending, streams) = {
            let mut st = self.0.st.lock().unwrap();
            if st.err.is_some() {
                return;
            }
            st.err = Some(err.clone());
            for cr in st.credits.values() {
                if let Some(t) = &cr.timer {
                    t.abort();
                }
            }
            st.credits.clear();
            (std::mem::take(&mut st.pending), std::mem::take(&mut st.streams))
        };
        {
            let mut aq = self.0.aq.lock().unwrap();
            aq.closed = true;
            aq.q.clear();
            self.0.acond.notify_all();
        }
        {
            let mut wq = self.0.wq.lock().unwrap();
            wq.closed = true;
            wq.q.clear();
            wq.bulk.clear();
        }
        self.0.cancel.set();
        for (_, p) in pending {
            let _ = p.tx.send(Err(err.clone()));
        }
        for (_, s) in streams {
            s.lost(self);
        }
        self.0.done.set();
    }

    async fn read_loop(self, t: Rc<Transport>) {
        loop {
            let b = tokio::select! {
                _ = self.0.cancel.wait() => return,
                b = t.read() => b,
            };
            let b = match b {
                Ok(b) => b,
                Err(e) => {
                    self.enqueue(Inbound::Err(Error::closed(e)));
                    return;
                }
            };
            let f = match wire::Frame::unmarshal(&b) {
                Ok(f) => f,
                Err(e) => {
                    self.fail(Error::Other(e.to_string()));
                    t.close(STATUS_PROTOCOL_ERROR, "bad frame").await;
                    return;
                }
            };
            if let Some(tr) = &self.0.tr {
                tr.frame_recv(&f);
            }
            let n = self.counted(&f);
            if n > 0 {
                self.credit(f.stream, n);
            }
            self.enqueue(Inbound::Frame(f));
        }
    }

    fn enqueue(&self, i: Inbound) {
        let mut aq = self.0.aq.lock().unwrap();
        if aq.closed {
            return;
        }
        if let Inbound::Frame(f) = &i {
            aq.bytes += f.payload.len();
        }
        aq.q.push_back(i);
        self.0.acond.notify_one();
    }

    /// Dispatches received frames in order.
    fn apply_loop(self) {
        loop {
            let i = {
                let mut aq = self.0.aq.lock().unwrap();
                while aq.q.is_empty() && !aq.closed {
                    aq = self.0.acond.wait(aq).unwrap();
                }
                if aq.closed {
                    return;
                }
                aq.q.pop_front().unwrap()
            };
            let f = match i {
                Inbound::Err(e) => {
                    self.fail(e);
                    return;
                }
                Inbound::Frame(f) => f,
            };
            let n = f.payload.len();
            if let Err(e) = self.dispatch(f) {
                self.fail(e);
                let _ = self.0.close.send(CloseCmd::Graceful(STATUS_PROTOCOL_ERROR, "bad frame", None));
                return;
            }
            let below = {
                let mut aq = self.0.aq.lock().unwrap();
                aq.bytes -= n;
                aq.bytes <= self.0.amax / 2
            };
            if below {
                self.release_credit();
            }
        }
    }

    fn dispatch(&self, f: wire::Frame) -> Result<()> {
        if !self.0.welcomed.is_set() {
            let w = check_welcome(&f)?;
            if let Some(cb) = self.0.cfg.on_carriers.clone() {
                let cs = w.carriers.clone();
                std::thread::spawn(move || cb(&cs));
            }
            let _ = self.0.welcome.set(w);
            self.0.welcomed.set();
            return Ok(());
        }
        if f.ty == wire::TYPE_RESPONSE {
            let r: wire::Response = f.decode().map_err(|e| Error::Other(e.to_string()))?;
            let Some(p) = self.0.st.lock().unwrap().pending.remove(&r.id) else { return Ok(()) };
            if let Some(k) = &self.0.cfg.knobs.response_taken {
                k(self, &r);
            }
            if let Some(h) = p.hook {
                h(&r);
            }
            let _ = p.tx.send(Ok(r));
            return Ok(());
        }
        if f.stream == 0 {
            return Err(Error::Other(format!("osewa: unexpected frame 0x{:02x} on stream 0", f.ty)));
        }
        let s = self.0.st.lock().unwrap().streams.get(&f.stream).cloned();
        // Frames for a stream we already dropped (after detach, or a failed
        // attach) are harmless.
        match s {
            Some(s) => s.handle(self, f),
            None => Ok(()),
        }
    }

    /// A frame's flow-controlled bytes. A stream's first snapshot sends its
    /// SNAPSHOT_DATA uncounted, so the first screen never waits on credit;
    /// crediting it would hand the holder window it never used. Later
    /// snapshots (catch-up screens) are counted: the holder paces them on
    /// credit.
    fn counted(&self, f: &wire::Frame) -> usize {
        match f.ty {
            wire::TYPE_SNAPSHOT_HISTORY | wire::TYPE_OUTPUT | wire::TYPE_EVENT => f.payload.len(),
            wire::TYPE_SNAPSHOT_BEGIN => {
                if let Ok(b) = f.decode::<wire::SnapshotBegin>() {
                    let mut st = self.0.st.lock().unwrap();
                    if let Some(cr) = credit_for(&mut st, f.stream) {
                        cr.data_counted = b.generation != 0;
                    }
                }
                0
            }
            wire::TYPE_SNAPSHOT_DATA => {
                let mut st = self.0.st.lock().unwrap();
                match credit_for(&mut st, f.stream) {
                    Some(cr) if cr.data_counted => f.payload.len(),
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn backlogged(&self) -> bool {
        self.0.aq.lock().unwrap().bytes > self.0.amax
    }

    /// Records n received bytes on stream and sends CREDIT once enough has
    /// built up, or soon after.
    fn credit(&self, stream: u32, n: usize) {
        let backlogged = self.backlogged();
        let mut st = self.0.st.lock().unwrap();
        let Some(cr) = credit_for(&mut st, stream) else { return };
        cr.n += n as u32;
        if !backlogged {
            if cr.n >= CREDIT_BYTES {
                self.send_credit(stream, cr);
            } else if cr.timer.is_none() {
                let c = self.clone();
                cr.timer = Some(rt::spawn(async move {
                    tokio::time::sleep(CREDIT_DELAY).await;
                    c.flush_credit(stream);
                }));
            }
            return;
        }
        st.held = true;
    }

    fn flush_credit(&self, stream: u32) {
        let backlogged = self.backlogged();
        let s = {
            let mut st = self.0.st.lock().unwrap();
            let Some(cr) = st.credits.get_mut(&stream) else { return };
            cr.timer = None;
            if backlogged {
                st.held = true;
            } else {
                self.send_credit(stream, cr);
            }
            st.streams.get(&stream).cloned()
        };
        // Outside the lock: the stream takes its own, and holds it while
        // asking credit_due.
        if let Some(s) = s {
            s.credit_sent(self);
        }
    }

    /// Whether a CREDIT for stream waits on its delay, so what is due to go
    /// with it (an ACK) can wait for it.
    pub(crate) fn credit_due(&self, stream: u32) -> bool {
        self.0.st.lock().unwrap().credits.get(&stream).is_some_and(|cr| cr.timer.is_some())
    }

    /// Sends credit held back while the apply queue was full.
    fn release_credit(&self) {
        let mut st = self.0.st.lock().unwrap();
        if !st.held {
            return;
        }
        st.held = false;
        for (&stream, cr) in st.credits.iter_mut() {
            self.send_credit(stream, cr);
        }
    }

    fn send_credit(&self, stream: u32, cr: &mut Credit) {
        if let Some(t) = cr.timer.take() {
            t.abort();
        }
        if cr.n > 0 {
            let _ = self.send(wire::credit(stream, cr.n));
            cr.n = 0;
        }
    }

    /// Queues f for the writer. It never blocks.
    pub(crate) fn send(&self, f: wire::Frame) -> Result<()> {
        {
            let mut wq = self.0.wq.lock().unwrap();
            if wq.closed {
                return Err(Error::Closed(None));
            }
            wq.push(&f);
        }
        self.0.wake_writer.notify_one();
        Ok(())
    }

    /// The next frames to write into batch; false once closed.
    fn next_frames(&self, batch: &mut Vec<Vec<u8>>) -> bool {
        let mut wq = self.0.wq.lock().unwrap();
        if wq.closed {
            return false;
        }
        wq.pop_batch(batch);
        true
    }

    /// Writes what is queued. Frames queued together (a CREDIT and the ACK
    /// riding with it, inputs typed while a write was out) go in one write:
    /// one TLS record and one syscall, here and at the server.
    async fn write_loop(self, t: Rc<Transport>) {
        let mut batch = Vec::new();
        loop {
            batch.clear();
            if !self.next_frames(&mut batch) {
                return;
            }
            if batch.is_empty() {
                tokio::select! {
                    _ = self.0.cancel.wait() => return,
                    _ = self.0.wake_writer.notified() => continue,
                }
            }
            let r = tokio::select! {
                _ = self.0.cancel.wait() => return,
                r = t.write_all(&batch) => r,
            };
            if let Err(e) = r {
                self.fail(Error::closed(e));
                t.close_now();
                return;
            }
            if let Some(tr) = &self.0.tr {
                for b in &batch {
                    if b.first() == Some(&wire::TYPE_INPUT) {
                        tr.input_sent(b);
                    }
                }
            }
        }
    }

    /// Pings every interval, and when typed input goes unacked (probe_now).
    /// An INPUT_ACK shows the link carried what was typed, so typing costs
    /// no ping of its own; only input still unacked at half the suspect
    /// wait (lost, or parked by the holder) gets one. No INPUT_ACK or pong
    /// for max(3 round trips, 1 s) after the input, or no pong that long
    /// after a regular ping, makes the connection suspect, so the session
    /// can resume on a fresh one while this one may still recover; two
    /// pings without a pong kill it.
    async fn keepalive(self, t: Rc<Transport>) {
        let ping = self.0.ping;
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + ping, ping);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut missed = 0;
        let mut rtt = Duration::ZERO;
        // Input sent before the last answered ping reached the server: the
        // main stream is ordered.
        let mut vouched = Instant::now();
        loop {
            let probe = tokio::select! {
                _ = self.0.cancel.wait() => return,
                _ = tick.tick() => false,
                _ = self.0.probe.notified() => true,
            };
            let wait = (rtt * 3).max(SUSPECT_MIN);
            let mut late = Instant::now() + wait;
            if probe {
                let Some(sent) = self.unacked(vouched, wait / 2).await else { return };
                let Some(sent) = sent else { continue };
                late = sent + wait;
            }
            let start = Instant::now();
            self.0.pings.fetch_add(1, Ordering::Relaxed);
            let pong = tokio::time::timeout(ping, t.ping());
            tokio::pin!(pong);
            let res = tokio::select! {
                _ = self.0.cancel.wait() => return,
                r = &mut pong => r,
                _ = tokio::time::sleep_until(late.into()) => {
                    // An INPUT_ACK that came in meanwhile answers the input,
                    // whatever keeps the pong.
                    if !probe || self.oldest_unacked(vouched).is_some_and(|s| s + wait <= Instant::now()) {
                        self.mark_suspect();
                    }
                    tokio::select! {
                        _ = self.0.cancel.wait() => return,
                        r = &mut pong => r,
                    }
                }
            };
            if self.0.cancel.is_set() {
                return;
            }
            match res {
                Ok(Ok(())) => {
                    missed = 0;
                    vouched = vouched.max(start);
                    let d = start.elapsed();
                    rtt = if rtt.is_zero() {
                        d
                    } else {
                        // rtt += (d - rtt) / 8, in signed arithmetic.
                        Duration::from_nanos((rtt.as_nanos() as i128 + (d.as_nanos() as i128 - rtt.as_nanos() as i128) / 8) as u64)
                    };
                }
                _ => {
                    missed += 1;
                    if missed >= 2 {
                        self.fail(Error::closed(format!("no pong in {:?}", 2 * ping)));
                        t.close_now();
                        return;
                    }
                }
            }
        }
    }

    /// When the oldest input sent after `after` and still unacked went out.
    fn oldest_unacked(&self, after: Instant) -> Option<Instant> {
        let streams: Vec<_> = self.0.st.lock().unwrap().streams.values().cloned().collect();
        streams.iter().filter_map(|s| s.unacked_since(after)).min()
    }

    /// Waits until input sent after `after` has been unacked for grace.
    /// Some(Some(sent)) with when that input went out, Some(None) once all
    /// of it is acked; None once cancelled.
    async fn unacked(&self, after: Instant, grace: Duration) -> Option<Option<Instant>> {
        loop {
            let Some(sent) = self.oldest_unacked(after) else { return Some(None) };
            if Instant::now() >= sent + grace {
                return Some(Some(sent));
            }
            tokio::select! {
                _ = self.0.cancel.wait() => return None,
                _ = tokio::time::sleep_until((sent + grace).into()) => {}
            }
        }
    }

    /// Has keepalive watch input just sent for its INPUT_ACK.
    pub(crate) fn probe_now(&self) {
        self.0.probe.notify_one();
    }

    fn mark_suspect(&self) {
        if !self.0.suspect.set() {
            return;
        }
        let streams: Vec<_> = self.0.st.lock().unwrap().streams.values().cloned().collect();
        for s in streams {
            s.suspect();
        }
    }

    /// Sends a REQUEST and waits for its RESPONSE. Dropping the future
    /// forgets the request: its response, and hook, are dropped.
    pub(crate) async fn request(&self, mut req: wire::Request, hook: Option<Hook>) -> Result<wire::Response> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.0.st.lock().unwrap();
            if let Some(e) = &st.err {
                return Err(e.clone());
            }
            st.next_id += 1;
            req.id = st.next_id;
            st.pending.insert(req.id, Call { tx, hook });
        }
        struct Forget<'a>(&'a Conn, u64);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0 .0.st.lock().unwrap().pending.remove(&self.1);
            }
        }
        let _forget = Forget(self, req.id);
        self.send(wire::json(wire::TYPE_REQUEST, 0, &req))?;
        let r = rx.await.unwrap_or(Err(Error::Closed(None)))?;
        if !r.error.is_empty() {
            return Err(Error::Response { code: r.error.clone(), message: r.message.clone() });
        }
        Ok(r)
    }

    /// Sends a request whose response nobody waits for.
    pub(crate) fn notify(&self, mut req: wire::Request) {
        {
            let mut st = self.0.st.lock().unwrap();
            st.next_id += 1;
            req.id = st.next_id;
        }
        let _ = self.send(wire::json(wire::TYPE_REQUEST, 0, &req));
    }

    /// Every session on the server.
    pub async fn list(&self) -> Result<Vec<wire::SessionInfo>> {
        Ok(self.request(wire::Request { method: wire::METHOD_LIST.into(), ..Default::default() }, None).await?.sessions)
    }

    /// Starts a session and returns its id.
    pub async fn create(&self, o: CreateOptions) -> Result<String> {
        let req = wire::Request {
            method: wire::METHOD_CREATE.into(),
            cols: o.cols,
            rows: o.rows,
            argv: o.argv,
            cwd: o.cwd,
            env: o.env,
            ..Default::default()
        };
        Ok(self.request(req, None).await?.session_id)
    }

    /// Ends a session.
    pub async fn kill(&self, id: &str) -> Result<()> {
        self.request(wire::Request { method: wire::METHOD_KILL.into(), session: id.into(), ..Default::default() }, None)
            .await
            .map(drop)
    }

    /// Attaches to session id and waits for the holder's answer. The
    /// screen comes once the snapshot is ready (on_ready, or the
    /// renderer's).
    pub async fn attach(&self, id: &str, opt: crate::AttachOptions) -> Result<Session> {
        let own = opt.own;
        let s = SessionInner::create(id, opt, None);
        // Dropped before the attach is through: the session is let go of.
        struct CloseOnDrop(Option<Session>);
        impl Drop for CloseOnDrop {
            fn drop(&mut self) {
                if let Some(s) = self.0.take() {
                    s.close();
                }
            }
        }
        let mut guard = CloseOnDrop(Some(s.clone()));
        s.0.attach(self, own).await?;
        guard.0 = None;
        Ok(s)
    }

    /// Allocates a stream id for s.
    pub(crate) fn add_stream(&self, s: Arc<dyn Stream>) -> Result<u32> {
        let mut st = self.0.st.lock().unwrap();
        if let Some(e) = &st.err {
            return Err(e.clone());
        }
        let id = st.next_stream;
        st.next_stream += 1;
        st.streams.insert(id, s);
        Ok(id)
    }

    /// Allocates a stream id and makes s from it, under the lock, so the
    /// applier only ever sees the stream with its id set.
    pub(crate) fn add_stream_with<S: Stream + 'static>(&self, make: impl FnOnce(u32) -> Arc<S>) -> Result<Arc<S>> {
        let mut st = self.0.st.lock().unwrap();
        if let Some(e) = &st.err {
            return Err(e.clone());
        }
        let id = st.next_stream;
        st.next_stream += 1;
        let s = make(id);
        st.streams.insert(id, s.clone());
        Ok(s)
    }

    pub(crate) fn drop_stream(&self, id: u32) {
        let mut st = self.0.st.lock().unwrap();
        st.streams.remove(&id);
        if let Some(cr) = st.credits.remove(&id) {
            if let Some(t) = cr.timer {
                t.abort();
            }
        }
    }
}

/// The stream's credit state, or None for a stream not credited on
/// receipt: one we don't have, a channel, or a manual-credit session.
fn credit_for(st: &mut State, stream: u32) -> Option<&mut Credit> {
    if !st.streams.get(&stream)?.credits_on_receipt() {
        return None;
    }
    Some(st.credits.entry(stream).or_default())
}

fn check_welcome(f: &wire::Frame) -> Result<wire::Welcome> {
    if f.ty != wire::TYPE_WELCOME {
        return Err(Error::Other(format!("osewa: expected WELCOME, got 0x{:02x}", f.ty)));
    }
    let w: wire::Welcome = f.decode().map_err(|e| Error::Other(e.to_string()))?;
    if w.version != wire::VERSION {
        return Err(Error::Other(format!("osewa: server speaks protocol {}, want {}", w.version, wire::VERSION)));
    }
    if w.snapshot_version != SNAPSHOT_VERSION || w.ghostty != GHOSTTY_COMMIT {
        return Err(Error::Incompatible {
            server_commit: w.ghostty,
            client_commit: GHOSTTY_COMMIT.into(),
            server_snapshot: w.snapshot_version,
            client_snapshot: SNAPSHOT_VERSION,
        });
    }
    Ok(w)
}
