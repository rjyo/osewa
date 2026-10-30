use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use osewa_ghostty as ghostty;
use osewa_wire as wire;

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
    /// SNAPSHOT_HISTORY bytes per generation, by index in gens.
    pub hist_bytes: HashMap<usize, usize>,
    /// Generations, by index in gens, that reached SNAPSHOT_END.
    pub snap_ends: HashMap<usize, bool>,
    pub closed: bool,
}

impl Client {
    pub fn dial(sock: &std::path::Path, client_id: &str) -> Client {
        let nc = UnixStream::connect(sock).unwrap();
        let r = nc.try_clone().unwrap();
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
            raw: Vec::new(),
            events: Vec::new(),
            ended: None,
            acked: 0,
            gaps: 0,
            stale: 0,
            snap: Vec::new(),
            no_credit: false,
            counted: 0,
            hist_bytes: HashMap::new(),
            snap_ends: HashMap::new(),
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

    fn write(&self, f: &wire::Frame) {
        let _ = wire::write(&mut *self.w.lock().unwrap(), f);
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
                        self.write(&wire::credit(f.stream, f.payload.len() as u32));
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
                }
            }
            wire::TYPE_SNAPSHOT_READY => {
                let term = self.dec.as_mut().unwrap().ready(ghostty::Callbacks::default()).unwrap();
                self.term = Some(term);
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

/// Bounds the flood and shaped-link tests that run at once: all of them
/// together saturate the machine, and timing assertions then measure the
/// contention rather than the holder.
pub fn heavy() -> std::sync::MutexGuard<'static, ()> {
    static HEAVY: Mutex<()> = Mutex::new(());
    HEAVY.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn flush(w: &Arc<Mutex<UnixStream>>) {
    let _ = w.lock().unwrap().flush();
}
