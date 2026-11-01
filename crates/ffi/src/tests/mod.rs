//! The C API against a real server in this process (holders on threads of
//! their own), driven as an app would: through the extern functions, with
//! C callbacks feeding a libghostty terminal standing in for the app's.

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use osewa_ghostty::{Options, Terminal};

use crate::*;

mod api;
mod options;

/// A private temporary directory, short enough for Unix socket paths.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        // Before anything starts: sessions run in a sandbox home.
        osewa_paths::sandbox_home();
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(format!("/tmp/osf{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        TempDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Holder = (tokio::sync::oneshot::Sender<()>, JoinHandle<Result<(), String>>);

/// Holders run in this process, each on its own thread. Each spawn sets
/// spawning, then waits delay before the holder starts.
fn inproc(holders: &Rc<RefCell<Vec<Holder>>>, spawning: Arc<std::sync::atomic::AtomicBool>, delay: Duration) -> osewa_server::Spawn {
    let holders = holders.clone();
    Rc::new(move |mut cfg: osewa_holder::Config| {
        spawning.store(true, std::sync::atomic::Ordering::SeqCst);
        cfg.exit_retention = Duration::from_millis(200);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let h = std::thread::spawn(move || {
            osewa_holder::run_until(
                cfg,
                move || {
                    let _ = ready_tx.send(());
                },
                false,
                Some(stop_rx),
            )
        });
        holders.borrow_mut().push((stop_tx, h));
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
                Ok(Ok(())) => Ok(()),
                _ => Err("holder not ready".into()),
            }
        })
    })
}

enum Ctl {
    Restart(mpsc::Sender<()>),
    Stop,
}

/// A server with this test's device enrolled.
struct Host {
    addr: String,
    server_key: String,
    device: Vec<u8>,
    ctl: tokio::sync::mpsc::UnboundedSender<Ctl>,
    thread: Option<JoinHandle<()>>,
    dir: TempDir,
    /// A holder spawn has begun.
    spawning: Arc<std::sync::atomic::AtomicBool>,
}

impl Host {
    fn new() -> Host {
        Host::with_spawn_delay(Duration::ZERO)
    }

    /// A host whose holders take delay to start: a create stays in flight
    /// that long.
    fn with_spawn_delay(delay: Duration) -> Host {
        let spawning: Arc<std::sync::atomic::AtomicBool> = Arc::default();
        let sp = spawning.clone();
        let dir = TempDir::new();
        let sid = osewa_auth::Identity::generate();
        let device = osewa_auth::Identity::generate();
        let devices = osewa_auth::Devices::new(dir.path().join("devices"));
        devices.add(osewa_auth::Device { name: "phone".into(), key: osewa_auth::encode_key(&device.public()) }).unwrap();
        let server_key = osewa_auth::encode_key(&sid.public());
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap().to_string();
        drop(ln);
        let run_dir = dir.path().join("run");
        osewa_paths::sandbox_home();
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::set_permissions(&run_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let (ctl, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let a = addr.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            tokio::task::LocalSet::new().block_on(&rt, async move {
                let holders: Rc<RefCell<Vec<Holder>>> = Rc::default();
                let sid = Rc::new(sid);
                let mut ready = Some(ready_tx);
                loop {
                    let cfg = osewa_server::Config {
                        identity: Some(sid.clone()),
                        devices: Some(devices.clone()),
                        run_dir: run_dir.clone(),
                        name: "ffi".into(),
                        spawn: Some(inproc(&holders, sp.clone(), delay)),
                        shell: vec!["/bin/sh".into()],
                        ..Default::default()
                    };
                    let s = osewa_server::Server::new(cfg).unwrap();
                    let mut ln = None;
                    for _ in 0..100 {
                        if let Ok(l) = tokio::net::TcpListener::bind(&a).await {
                            ln = Some(l);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if let Some(r) = ready.take() {
                        let _ = r.send(());
                    }
                    let serving = s.serve(ln.expect("listen"));
                    tokio::pin!(serving);
                    let cmd = tokio::select! {
                        _ = &mut serving => None,
                        c = rx.recv() => c,
                    };
                    s.close().await;
                    match cmd {
                        Some(Ctl::Restart(done)) => {
                            let _ = done.send(());
                        }
                        _ => break,
                    }
                }
                for (stop, h) in holders.borrow_mut().drain(..) {
                    let _ = stop.send(());
                    let _ = h.join();
                }
            });
        });
        ready_rx.recv().unwrap();
        Host { addr, server_key, device: device.pkcs8().to_vec(), ctl, thread: Some(thread), dir, spawning }
    }

    /// Takes this test's device off the server's devices file.
    fn revoke(&self) {
        osewa_auth::Devices::new(self.dir.path().join("devices")).remove("phone").unwrap();
    }

    /// Drops every connection and serves again on the same address;
    /// holders survive.
    fn restart(&self) {
        let (tx, rx) = mpsc::channel();
        self.ctl.send(Ctl::Restart(tx)).unwrap();
        rx.recv().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.ctl.send(Ctl::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the callbacks saw.
struct Rec {
    term: Mutex<Terminal>,
    out: Mutex<Vec<u8>>,
    states: Mutex<Vec<(i32, Option<String>)>>,
    stats: Mutex<Vec<(u32, String)>>,
    events: Mutex<Vec<serde_json::Value>>,
    /// States and event types, in the order they came.
    log: Mutex<Vec<String>>,
    calls: std::sync::atomic::AtomicUsize,
    /// The session, for callbacks that call back in.
    h: std::sync::atomic::AtomicPtr<OsewaSession>,
}

// The terminal is only used under its lock.
unsafe impl Send for Rec {}
unsafe impl Sync for Rec {}

impl Rec {
    fn new(cols: usize, rows: usize) -> Arc<Rec> {
        Arc::new(Rec {
            term: Mutex::new(Terminal::new(cols, rows, Options::default()).unwrap()),
            out: Mutex::default(),
            states: Mutex::default(),
            stats: Mutex::default(),
            events: Mutex::default(),
            log: Mutex::default(),
            calls: Default::default(),
            h: Default::default(),
        })
    }

    fn callbacks(self: &Arc<Rec>) -> Callbacks {
        Callbacks { ctx: Arc::as_ptr(self) as *mut c_void, output: Some(on_output), state: Some(on_state), stats: Some(on_stats), event: Some(on_event) }
    }

    fn screen(&self) -> String {
        self.term.lock().unwrap().text()
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn last_state(&self) -> Option<(i32, Option<String>)> {
        self.states.lock().unwrap().last().cloned()
    }

    #[track_caller]
    fn wait_screen(&self, substr: &str) {
        let what = format!("{substr:?} on screen");
        eventually(&what, || self.screen().contains(substr), || self.screen());
    }

    #[track_caller]
    fn wait_state(&self, state: i32) -> Option<String> {
        eventually(&format!("state {state}"), || self.last_state().is_some_and(|s| s.0 == state), || format!("{:?}", self.states.lock().unwrap()));
        self.last_state().unwrap().1
    }

    #[track_caller]
    fn wait_event(&self, ty: &str) -> serde_json::Value {
        let find = || self.events.lock().unwrap().iter().find(|e| e["type"] == ty).cloned();
        eventually(&format!("event {ty}"), || find().is_some(), || format!("{:?}", self.events.lock().unwrap()));
        find().unwrap()
    }
}

fn rec(ctx: *mut c_void) -> &'static Rec {
    unsafe { &*(ctx as *const Rec) }
}

unsafe extern "C" fn on_output(ctx: *mut c_void, b: *const u8, n: usize) {
    let r = rec(ctx);
    r.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let b = unsafe { std::slice::from_raw_parts(b, n) };
    r.out.lock().unwrap().extend_from_slice(b);
    r.term.lock().unwrap().write(b);
}

unsafe extern "C" fn on_state(ctx: *mut c_void, state: i32, detail: *const c_char) {
    let r = rec(ctx);
    r.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = (!detail.is_null()).then(|| unsafe { CStr::from_ptr(detail) }.to_string_lossy().into_owned());
    r.log.lock().unwrap().push(format!("state {state}"));
    r.states.lock().unwrap().push((state, d));
}

unsafe extern "C" fn on_stats(ctx: *mut c_void, srtt: u32, carrier: *const c_char) {
    let r = rec(ctx);
    r.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    r.stats.lock().unwrap().push((srtt, unsafe { CStr::from_ptr(carrier) }.to_string_lossy().into_owned()));
}

unsafe extern "C" fn on_event(ctx: *mut c_void, json: *const c_char) {
    let r = rec(ctx);
    r.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let j = unsafe { CStr::from_ptr(json) }.to_string_lossy();
    let v: serde_json::Value = serde_json::from_str(&j).expect("event JSON");
    r.log.lock().unwrap().push(format!("event {}", v["type"].as_str().unwrap_or_default()));
    r.events.lock().unwrap().push(v);
}

const WAIT: Duration = Duration::from_secs(10);

#[track_caller]
fn eventually(what: &str, mut cond: impl FnMut() -> bool, show: impl Fn() -> String) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {}", show());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// C strings for osewa_open_options, kept alive with them.
#[derive(Default)]
struct Strings(Vec<CString>);

impl Strings {
    fn add(&mut self, s: &str) -> *const c_char {
        self.0.push(CString::new(s).unwrap());
        self.0.last().unwrap().as_ptr()
    }

    fn opt(&mut self, s: Option<&str>) -> *const c_char {
        s.map_or(std::ptr::null(), |s| self.add(s))
    }
}

/// What a test opens with; open turns it into osewa_open_options.
struct Open<'a> {
    addr: String,
    server_key: String,
    device: &'a [u8],
    session_id: Option<String>,
    argv: Option<&'a str>,
    appearance: Option<&'a str>,
    cols: u16,
    rows: u16,
    predict: i32,
}

impl Host {
    fn open_opts(&self) -> Open<'_> {
        Open {
            addr: self.addr.clone(),
            server_key: self.server_key.clone(),
            device: &self.device,
            session_id: None,
            argv: Some(r#"["/bin/sh"]"#),
            appearance: None,
            cols: 80,
            rows: 24,
            predict: 0,
        }
    }
}

/// Opens through osewa_open: the session, or the error.
fn try_open(o: &Open, r: &Arc<Rec>) -> Result<*mut OsewaSession, String> {
    let mut s = Strings::default();
    let opts = OpenOptions {
        addr: s.add(&o.addr),
        server_key: s.add(&o.server_key),
        quic_port: 0,
        device_key: o.device.as_ptr(),
        device_key_len: o.device.len(),
        client_id: s.add("phone-test"),
        device_name: s.add("phone"),
        session_id: s.opt(o.session_id.as_deref()),
        argv_json: s.opt(o.argv),
        cwd: std::ptr::null(),
        cols: o.cols,
        rows: o.rows,
        width_px: 0,
        height_px: 0,
        predict: o.predict,
        appearance_json: s.opt(o.appearance),
    };
    let mut err: *mut c_char = std::ptr::null_mut();
    let h = unsafe { osewa_open(&opts, r.callbacks(), &mut err) };
    if h.is_null() {
        assert!(!err.is_null(), "NULL without an error");
        let e = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        unsafe { osewa_string_free(err) };
        return Err(e);
    }
    assert!(err.is_null(), "an error with a session");
    Ok(h)
}

fn open(o: &Open, r: &Arc<Rec>) -> *mut OsewaSession {
    try_open(o, r).unwrap()
}

fn send(h: *mut OsewaSession, s: &str) {
    assert_eq!(unsafe { osewa_send(h, s.as_ptr(), s.len()) }, 0, "send {s:?}");
}
