use std::collections::HashMap;
use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use osewa_ghostty as ghostty;
use osewa_wire as wire;

use crate::queue::SendQueue;
use crate::session::{Conn, Session, Stream};
use crate::{socket_path, Config};

pub const WAIT: Duration = Duration::from_secs(10);

/// A short temporary run dir: Unix socket paths are capped near 104 bytes.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = PathBuf::from(format!("/tmp/oh{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct TestHolder {
    pub cfg: Config,
    pub sock: PathBuf,
    pub dir: TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: Option<JoinHandle<Result<(), String>>>,
}

/// sh starts /bin/sh -c script.
pub fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

pub fn start(mut cfg: Config) -> TestHolder {
    let dir = TempDir::new();
    if cfg.run_dir.as_os_str().is_empty() {
        cfg.run_dir = dir.0.join("run");
    }
    if cfg.id.is_empty() {
        cfg.id = "s1".into();
    }
    if cfg.cols == 0 {
        cfg.cols = 40;
        cfg.rows = 10;
    }
    if cfg.home.is_none() {
        // Sessions run in a sandbox home, never the real one.
        cfg.home = Some(osewa_paths::sandbox_home());
    }
    let sock = socket_path(&cfg.run_dir, &cfg.id);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let c = cfg.clone();
    let done = std::thread::spawn(move || {
        crate::run_until(c, move || ready_tx.send(()).unwrap(), false, Some(stop_rx))
    });
    if ready_rx.recv_timeout(WAIT).is_err() {
        let r = done.join().unwrap();
        panic!("holder not ready: {r:?}");
    }
    TestHolder { cfg, sock, dir, stop: Some(stop_tx), done: Some(done) }
}

impl TestHolder {
    /// Waits for the holder to stop on its own.
    pub fn done(&mut self, within: Duration) -> Result<(), String> {
        let h = self.done.take().expect("already done");
        let deadline = Instant::now() + within;
        while !h.is_finished() {
            assert!(Instant::now() < deadline, "holder still running");
            std::thread::sleep(Duration::from_millis(10));
        }
        h.join().unwrap()
    }

    pub fn dial(&self, client_id: &str) -> Client {
        Client::dial(&self.sock, client_id)
    }

    pub fn dial_via(&self, client_id: &str, link: Link) -> Client {
        Client::dial_via(&self.sock, client_id, link)
    }

    /// Stops the holder as SIGTERM would.
    pub fn cancel(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl Drop for TestHolder {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(h) = self.done.take() {
            let deadline = Instant::now() + WAIT;
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !std::thread::panicking() {
                assert!(h.is_finished(), "holder did not stop");
            }
        }
    }
}

pub type OnReady = Box<dyn FnMut(&Client)>;

/// A test peer speaking holder frames, with a libghostty terminal restored
/// from snapshots and fed output like a real client.
pub struct Client {
    pub w: Arc<Mutex<UnixStream>>,
    pub frames: Receiver<wire::Frame>,
    pub id: String,
    req_id: u64,
    pub stream: u32,

    dec: Option<ghostty::Decoder>,
    pub term: Option<ghostty::Terminal>,
    pub gens: Vec<wire::SnapshotBegin>,
    /// The position after the last record.
    pub end: u64,
    /// The start position of each record frame received.
    pub starts: Vec<u64>,
    /// Output bytes received live.
    pub raw: Vec<u8>,
    pub events: Vec<wire::EventMsg>,
    pub ended: Option<wire::StreamEnd>,
    pub acked: u64,
    /// Records that didn't start where the last one ended.
    pub gaps: usize,
    /// Records after a SNAPSHOT_BEGIN that the snapshot already holds.
    pub stale: usize,
    /// The latest snapshot's bytes.
    pub snap: Vec<u8>,
    /// Never send CREDIT.
    pub no_credit: bool,
    /// Counted bytes received (docs/PROTOCOL.md, Flow control).
    pub counted: usize,
    /// SNAPSHOT_DATA bytes before the last READY.
    pub ready_at: usize,
    /// SNAPSHOT_HISTORY bytes per generation, by index in gens.
    pub hist_bytes: HashMap<usize, usize>,
    /// Generations, by index in gens, that reached SNAPSHOT_END.
    pub snap_ends: HashMap<usize, bool>,
    /// Runs after each SNAPSHOT_READY is applied.
    pub on_ready: Option<OnReady>,
    pub closed: bool,
}

impl Client {
    pub fn dial(sock: &std::path::Path, client_id: &str) -> Client {
        Client::dial_via(sock, client_id, Box::new(|nc| Box::new(nc)))
    }

    /// dial, reading the holder through link(nc), for a client behind a slow
    /// link; writes go straight to the socket.
    pub fn dial_via(sock: &std::path::Path, client_id: &str, link: Link) -> Client {
        let nc = UnixStream::connect(sock).unwrap();
        let r = link(nc.try_clone().unwrap());
        // Unbuffered, so a client that stops reading really stops reading.
        let (tx, rx) = mpsc::sync_channel(0);
        std::thread::spawn(move || {
            let mut r = BufReader::new(r);
            while let Ok(f) = wire::read(&mut r) {
                if tx.send(f).is_err() {
                    return;
                }
            }
        });
        let c = Client {
            w: Arc::new(Mutex::new(nc)),
            frames: rx,
            id: client_id.into(),
            req_id: 0,
            stream: 0,
            dec: None,
            term: None,
            gens: Vec::new(),
            end: 0,
            starts: Vec::new(),
            raw: Vec::new(),
            events: Vec::new(),
            ended: None,
            acked: 0,
            gaps: 0,
            stale: 0,
            snap: Vec::new(),
            no_credit: false,
            counted: 0,
            ready_at: 0,
            hist_bytes: HashMap::new(),
            snap_ends: HashMap::new(),
            on_ready: None,
            closed: false,
        };
        if !client_id.is_empty() {
            c.send(&wire::json(
                wire::TYPE_HELLO,
                0,
                &wire::Hello { version: wire::VERSION, client_id: client_id.into(), ..Default::default() },
            ));
        }
        c
    }

    pub fn send(&self, f: &wire::Frame) {
        wire::write(&mut *self.w.lock().unwrap(), f).unwrap();
    }

    /// send for other threads, and when the holder may have gone.
    pub fn write(&self, f: &wire::Frame) -> std::io::Result<()> {
        wire::write(&mut *self.w.lock().unwrap(), f)
    }

    pub fn close(&self) {
        let _ = self.w.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }

    /// Sends req and handles frames until its response.
    pub fn request(&mut self, mut req: wire::Request) -> wire::Response {
        self.req_id += 1;
        req.id = self.req_id;
        self.send(&wire::json(wire::TYPE_REQUEST, 0, &req));
        let id = req.id;
        let mut resp = None;
        self.until(|f| {
            if f.ty != wire::TYPE_RESPONSE {
                return false;
            }
            let r: wire::Response = f.decode().unwrap();
            let mine = r.id == id;
            if mine {
                resp = Some(r);
            }
            mine
        });
        resp.unwrap()
    }

    pub fn attach(&mut self, mut req: wire::Request) -> wire::Response {
        req.method = wire::METHOD_ATTACH.into();
        if req.stream == 0 {
            req.stream = 7;
        }
        self.stream = req.stream;
        let resp = self.request(req);
        assert!(resp.error.is_empty(), "attach: {}: {}", resp.error, resp.message);
        resp
    }

    pub fn attach_size(&mut self, cols: i64, rows: i64, own: bool) -> wire::Response {
        self.attach(wire::Request { cols, rows, own, ..Default::default() })
    }

    pub fn info(&mut self) -> wire::SessionInfo {
        let resp = self.request(wire::Request { method: wire::METHOD_INFO.into(), ..Default::default() });
        assert_eq!(resp.sessions.len(), 1, "info: {resp:?}");
        resp.sessions[0].clone()
    }

    pub fn input(&self, input: wire::Input) {
        self.send(&wire::json(wire::TYPE_INPUT, self.stream, &input));
    }

    pub fn text_input(&self, seq: u64, text: &str) {
        self.input(wire::Input { seq, ty: "text".into(), text: text.into(), ..Default::default() });
    }

    /// The next frame, or None when the connection closed.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<wire::Frame>, ()> {
        match self.frames.recv_timeout(timeout) {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Ok(None)
            }
            Err(RecvTimeoutError::Timeout) => Err(()),
        }
    }

    /// Handles frames until stop returns true for one of them.
    pub fn until(&mut self, mut stop: impl FnMut(&wire::Frame) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next_frame(left) {
                Ok(Some(f)) => {
                    self.handle(&f);
                    if stop(&f) {
                        return;
                    }
                }
                Ok(None) => panic!("connection closed; text so far:\n{}", self.text()),
                Err(()) => panic!("timed out; text so far:\n{}", self.text()),
            }
        }
    }

    /// Handles frames until cond holds.
    pub fn wait_for(&mut self, what: &str, mut cond: impl FnMut(&Client) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !cond(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next_frame(left) {
                Ok(Some(f)) => self.handle(&f),
                Ok(None) => panic!("connection closed waiting for {what}; text so far:\n{}", self.text()),
                Err(()) => panic!("timed out waiting for {what}; text so far:\n{}", self.text()),
            }
        }
    }

    pub fn wait_text(&mut self, s: &str) {
        let s = s.to_string();
        self.wait_for(&format!("text {s:?}"), move |c| c.text().contains(&s));
    }

    /// Handles frames for d, or until the connection closes.
    pub fn drain(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.next_frame(left) {
                Ok(Some(f)) => self.handle(&f),
                Ok(None) | Err(()) => return,
            }
        }
    }

    pub fn text(&self) -> String {
        self.term.as_ref().map(|t| t.text()).unwrap_or_default()
    }

    /// Handles every frame the holder has ready right now, without waiting.
    pub fn drain_now(&mut self) {
        while let Ok(f) = self.frames.try_recv() {
            self.handle(&f);
        }
    }

    /// Takes the frame right after the last one handled.
    pub fn next_handled(&mut self) -> wire::Frame {
        match self.next_frame(WAIT) {
            Ok(Some(f)) => {
                self.handle(&f);
                f
            }
            Ok(None) => panic!("connection closed"),
            Err(()) => panic!("no frame"),
        }
    }

    pub fn want_input_ack(&mut self, seq: u64) {
        let f = self.next_handled();
        assert!(
            f.ty == wire::TYPE_INPUT_ACK && f.stream == self.stream,
            "frame after attach = 0x{:02x} stream {}, want INPUT_ACK",
            f.ty,
            f.stream
        );
        assert_eq!(wire::parse_u64(&f.payload).unwrap(), seq, "INPUT_ACK after attach");
    }

    pub fn has_event(&self, mut f: impl FnMut(&wire::EventMsg) -> bool) -> bool {
        self.events.iter().any(&mut f)
    }

    pub fn event_kinds(&self) -> Vec<String> {
        self.events.iter().map(|e| e.kind.clone()).collect()
    }

    pub fn handle(&mut self, f: &wire::Frame) {
        match f.ty {
            wire::TYPE_SNAPSHOT_DATA | wire::TYPE_SNAPSHOT_HISTORY | wire::TYPE_OUTPUT | wire::TYPE_EVENT => {
                // SNAPSHOT_DATA of a stream's first snapshot is not counted.
                let first = f.ty == wire::TYPE_SNAPSHOT_DATA && self.gens.last().is_some_and(|g| g.generation == 0);
                if !first {
                    // Credit on receipt, before applying, as the protocol
                    // asks.
                    self.counted += f.payload.len();
                    if !self.no_credit {
                        let _ = self.write(&wire::credit(f.stream, f.payload.len() as u32));
                    }
                }
            }
            _ => {}
        }
        match f.ty {
            wire::TYPE_SNAPSHOT_BEGIN => {
                let b: wire::SnapshotBegin = f.decode().unwrap();
                self.end = b.position;
                self.gens.push(b);
                self.dec = Some(ghostty::Decoder::new().unwrap());
                self.snap.clear();
            }
            wire::TYPE_SNAPSHOT_DATA | wire::TYPE_SNAPSHOT_HISTORY => {
                let dec = self.dec.as_mut().unwrap_or_else(|| panic!("frame 0x{:02x} outside a snapshot", f.ty));
                dec.feed(&f.payload);
                self.snap.extend_from_slice(&f.payload);
                if f.ty == wire::TYPE_SNAPSHOT_HISTORY {
                    *self.hist_bytes.entry(self.gens.len() - 1).or_default() += f.payload.len();
                } else {
                    self.ready_at = self.snap.len();
                }
            }
            wire::TYPE_SNAPSHOT_READY => {
                let term = self.dec.as_mut().unwrap().ready(ghostty::Callbacks::default()).unwrap();
                self.term = Some(term);
                if let Some(mut f) = self.on_ready.take() {
                    f(self);
                    if self.on_ready.is_none() {
                        self.on_ready = Some(f);
                    }
                }
            }
            wire::TYPE_SNAPSHOT_END => {
                let g = self.gens.len() - 1;
                self.snap_ends.insert(g, true);
                // A screen snapshot stops at READY: there is no FINISH to
                // check.
                if self.gens[g].history.unwrap_or(true) {
                    self.dec.as_mut().unwrap().finish().unwrap();
                }
                self.dec = None;
            }
            wire::TYPE_OUTPUT | wire::TYPE_EVENT => {
                let (end, data) = wire::split_end(&f.payload).unwrap();
                // One OUTPUT frame may carry k consecutive records, so it
                // spans len(data) + 5k positions; anything else is a gap or
                // overlap.
                let span = end.wrapping_sub(self.end);
                if end < self.end || span < wire::record_size(data.len()) || !(span - data.len() as u64).is_multiple_of(5) {
                    self.gaps += 1;
                }
                self.starts.push(self.end);
                if self.gens.last().is_some_and(|g| end <= g.position) {
                    self.stale += 1;
                }
                self.end = end;
                if f.ty == wire::TYPE_OUTPUT {
                    self.raw.extend_from_slice(data);
                    self.term.as_mut().expect("output before a terminal").write(data);
                } else {
                    self.events.push(serde_json::from_slice(data).unwrap());
                }
            }
            wire::TYPE_STREAM_END => self.ended = Some(f.decode().unwrap()),
            wire::TYPE_INPUT_ACK => self.acked = wire::parse_u64(&f.payload).unwrap(),
            _ => {}
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

/// Writes b to a file a child can cat, so tests needn't shell-quote escape
/// sequences.
pub fn payload(dir: &TempDir, name: &str, b: &[u8]) -> String {
    let p = dir.0.join(name);
    std::fs::write(&p, b).unwrap();
    p.to_string_lossy().into_owned()
}

pub fn eventually(d: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Bounds the flood and shaped-link tests that run at once. Each floods a
/// PTY at ten times a simulated link and times what arrives; all of them
/// together saturate the machine, and timing assertions then measure the
/// contention rather than the holder.
pub const HEAVY_UNITS: usize = 4;

static HEAVY: Mutex<usize> = Mutex::new(HEAVY_UNITS);
static HEAVY_FREE: Condvar = Condvar::new();

/// Held units of HEAVY_UNITS, given back on drop.
pub struct Heavy(usize);

/// Takes n of HEAVY_UNITS until the guard drops; HEAVY_UNITS makes the test
/// run alone among heavy tests.
pub fn heavy(n: usize) -> Heavy {
    let mut free = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    while *free < n {
        free = HEAVY_FREE.wait(free).unwrap_or_else(|e| e.into_inner());
    }
    *free -= n;
    Heavy(n)
}

impl Drop for Heavy {
    fn drop(&mut self) {
        *HEAVY.lock().unwrap_or_else(|e| e.into_inner()) += self.0;
        HEAVY_FREE.notify_all();
    }
}

pub fn flush(w: &Arc<Mutex<UnixStream>>) {
    let _ = w.lock().unwrap().flush();
}

/// What tests compare between terminals.
pub fn state(t: &ghostty::Terminal) -> String {
    let vt = t.format(ghostty::Format::Vt, false, false).unwrap();
    let (cols, rows) = t.size();
    let (x, y) = t.cursor();
    format!(
        "{cols}x{rows} cursor={x},{y} alt={} title={:?}\n{:?}",
        t.alt_screen(),
        t.title(),
        String::from_utf8_lossy(&vt)
    )
}

/// state without scrollback, which a catch-up screen doesn't carry: ED 3
/// erases it on the terminal before comparing.
pub fn screen_state(t: &mut ghostty::Terminal) -> String {
    t.write(b"\x1b[3J");
    state(t)
}

pub fn new_ref(cols: usize, rows: usize) -> ghostty::Terminal {
    ghostty::Terminal::new(cols, rows, ghostty::Options { continuation: true, ..Default::default() }).unwrap()
}

/// Whether a snapshot is a catch-up screen.
pub fn is_screen(b: &wire::SnapshotBegin) -> bool {
    b.history == Some(false)
}

// Links: how a client reads the holder.

pub type Link = Box<dyn FnOnce(UnixStream) -> Box<dyn Read + Send> + Send>;

/// A narrow link: at most rate bytes a second.
pub fn slow_link(rate: usize) -> Link {
    Box::new(move |nc| Box::new(SlowConn::new(nc, rate)))
}

/// Delivers what the holder sends d later: a long, wide link.
pub fn delay_link(d: Duration) -> Link {
    Box::new(move |nc| Box::new(delayed(nc, d)))
}

/// A narrow link with latency: rate bytes a second, each byte delivered d
/// after it left. Credit goes back at once, so the credit round trip is
/// about d plus the transfer.
pub fn slow_delay_link(rate: usize, d: Duration) -> Link {
    Box::new(move |nc| Box::new(delayed(SlowConn::new(nc, rate), d)))
}

/// A narrow link behind a server-like relay: the relay reads the holder at
/// once into up to 4 MiB of buffer, and the link drains it at rate. Pacing
/// has to come from credit, not from the socket.
pub fn buffered_link(rate: usize) -> Link {
    Box::new(move |mut nc: UnixStream| {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>((4 << 20) / (32 << 10));
        std::thread::spawn(move || loop {
            let mut b = vec![0; 32 << 10];
            match nc.read(&mut b) {
                Ok(n) if n > 0 => {
                    b.truncate(n);
                    if tx.send(b).is_err() {
                        return;
                    }
                }
                _ => return,
            }
        });
        Box::new(SlowConn::new(ChanReader { rx, left: Vec::new(), at: 0 }, rate))
    })
}

/// Reads the chunks a channel carries.
struct ChanReader {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
    at: usize,
}

impl Read for ChanReader {
    fn read(&mut self, p: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.left.len() {
            match self.rx.recv() {
                Ok(b) => (self.left, self.at) = (b, 0),
                Err(_) => return Ok(0),
            }
        }
        let n = p.len().min(self.left.len() - self.at);
        p[..n].copy_from_slice(&self.left[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Reads r on a thread and hands each chunk on d after it was read.
fn delayed<R: Read + Send + 'static>(mut r: R, d: Duration) -> impl Read + Send {
    let (tx, rx) = mpsc::channel::<(Vec<u8>, Instant)>();
    std::thread::spawn(move || loop {
        let mut b = vec![0; 32 << 10];
        match r.read(&mut b) {
            Ok(n) if n > 0 => {
                b.truncate(n);
                if tx.send((b, Instant::now())).is_err() {
                    return;
                }
            }
            _ => return,
        }
    });
    let (dtx, drx) = mpsc::sync_channel::<Vec<u8>>(0);
    std::thread::spawn(move || {
        for (b, at) in rx {
            std::thread::sleep((at + d).saturating_duration_since(Instant::now()));
            if dtx.send(b).is_err() {
                return;
            }
        }
    });
    ChanReader { rx: drx, left: Vec::new(), at: 0 }
}

/// Reads at most rate bytes a second, like a narrow link.
pub struct SlowConn<R> {
    r: R,
    rate: usize,
    start: Option<Instant>,
    n: usize,
}

impl<R: Read> SlowConn<R> {
    pub fn new(r: R, rate: usize) -> SlowConn<R> {
        SlowConn { r, rate, start: None, n: 0 }
    }

    fn due(&self) -> Instant {
        self.start.unwrap() + Duration::from_secs_f64(self.n as f64 / self.rate as f64)
    }
}

impl<R: Read> Read for SlowConn<R> {
    fn read(&mut self, p: &mut [u8]) -> std::io::Result<usize> {
        if self.start.is_none() {
            self.start = Some(Instant::now());
        }
        let cap = p.len().min((self.rate / 50).max(1));
        let n = self.r.read(&mut p[..cap])?;
        // An idle link doesn't bank bandwidth for a burst later: more than a
        // moment behind schedule, the schedule restarts from now.
        if Instant::now().saturating_duration_since(self.due()) > Duration::from_millis(20) {
            (self.start, self.n) = (Some(Instant::now()), 0);
        }
        self.n += n;
        std::thread::sleep(self.due().saturating_duration_since(Instant::now()));
        Ok(n)
    }
}

// Sessions without a holder loop, for tests of the state machine.

/// Runs f inside a current-thread runtime and LocalSet, where session
/// timers can be spawned.
pub fn local<T>(f: impl std::future::Future<Output = T>) -> T {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, f)
}

/// A session with no child and no PTY: the termios poll finds no fd and
/// does nothing.
pub fn unit_session(cfg: Config) -> Rc<RefCell<Session>> {
    Session::new(cfg, -1, Rc::new(Instant::now)).unwrap()
}

/// Adds a connection for client with stream (if any), returning its queue.
pub fn add_conn(s: &Rc<RefCell<Session>>, cid: u64, client: &str, stream: Option<Stream>) -> Rc<RefCell<SendQueue>> {
    let mut b = s.borrow_mut();
    let q = Rc::new(RefCell::new(SendQueue::new(b.core.clock.clone())));
    let mut c = Conn::new(cid, q.clone());
    c.client = client.into();
    c.stream = stream;
    b.conns.insert(cid, c);
    q
}

/// A stream with id that has sent snaps snapshots.
pub fn unit_stream(uid: u64, id: u32, snaps: u32, catch_up: bool) -> Stream {
    let mut st = Stream::new(uid, id, 0, 0, 0, 0);
    st.snaps = snaps;
    st.catch_up = catch_up;
    st
}

/// The INPUT_ACKs queued on q: how many, and the last one.
pub fn acks(q: &RefCell<SendQueue>) -> (usize, u64) {
    let q = q.borrow();
    let mut n = 0;
    let mut last = 0;
    for f in &q.prio {
        if f.ty == wire::TYPE_INPUT_ACK {
            n += 1;
            last = wire::parse_u64(&f.payload).unwrap();
        }
    }
    (n, last)
}
