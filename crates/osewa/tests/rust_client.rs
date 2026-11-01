//! The client library (osewa-client, osewa-mirror) against a real
//! `osewa server` with its holders. Attach with a
//! snapshot, input echo, resume after a dropped connection without a new
//! snapshot, exec with large output and input, forward, list and kill, and
//! login's check of a saved host.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use osewa_client::{block_on, hosts, timeout, AttachOptions, Config, Conn, CreateOptions, ExecOptions, Renderer, RendererError};
use osewa_mirror::Mirror;
use osewa_wire as wire;

const RUST_OSEWA: &str = env!("CARGO_BIN_EXE_osewa");
const WAIT: Duration = Duration::from_secs(10);

fn run<F: std::future::Future>(f: F) -> F::Output {
    block_on(timeout(WAIT, f)).expect("timed out")
}

struct TempDir(PathBuf);

impl TempDir {
    /// Short, for Unix socket paths.
    fn new(tag: &str) -> TempDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = PathBuf::from(format!("/tmp/osr-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A server with its own config and run directories, and a
/// client directory with this device's key and the host in hosts.json, as
/// `osewa login` leaves them.
struct Rig {
    server: Child,
    host: hosts::Host,
    /// Config.carrier for every dial: "ws", "quic", or "" to race once
    /// hosts.json knows the QUIC port.
    carrier: &'static str,
    client_dir: PathBuf,
    run_dir: PathBuf,
    _dir: TempDir,
}

impl Rig {
    fn start(bin: &Path, tag: &str) -> Rig {
        let dir = TempDir::new(tag);
        let (server_dir, client_dir, run_dir) = (dir.0.join("server"), dir.0.join("client"), dir.0.join("run"));
        for d in [&server_dir, &client_dir] {
            fs::create_dir_all(d).unwrap();
            fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let server_id = osewa_auth::Identity::load_or_create(&server_dir.join("server.key")).unwrap();
        let client_id = osewa_auth::Identity::load_or_create(&hosts::key_path(&client_dir)).unwrap();
        osewa_auth::Devices::new(server_dir.join("devices"))
            .add(osewa_auth::Device { name: "rust-client".into(), key: osewa_auth::encode_key(&client_id.public()) })
            .unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let listen = format!("127.0.0.1:{port}");
        let host = hosts::Host {
            name: tag.into(),
            addr: listen.clone(),
            server_key: osewa_auth::encode_key(&server_id.public()),
            ..Default::default()
        };
        hosts::save(&client_dir, host.clone()).unwrap();
        let mut cmd = Command::new(bin);
        // Its sessions and exec run in a sandbox home, never the real one.
        osewa_paths::sandbox(&mut cmd);
        let server = cmd
            .args(["server", "--listen", &listen, "--name", tag])
            .env("OSEWA_CONFIG_DIR", &server_dir)
            .env("OSEWA_RUN_DIR", &run_dir)
            .env_remove("OSEWA_HOLDER")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + WAIT;
        while TcpStream::connect(&listen).is_err() {
            assert!(Instant::now() < deadline, "the server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Rig { server, host, carrier: "ws", client_dir, run_dir, _dir: dir }
    }

    fn config(&self, client_id: &str) -> Config {
        let h = hosts::find(&self.client_dir, &self.host.name).unwrap();
        let mut cfg = h.config(&self.client_dir, client_id, "rust-test").unwrap();
        cfg.carrier = self.carrier.into();
        cfg
    }

    fn dial(&self, client_id: &str) -> Conn {
        let c = run(Conn::dial(self.config(client_id))).unwrap();
        let w = run(c.welcome()).unwrap();
        let want = if self.carrier.is_empty() { "quic" } else { self.carrier };
        assert_eq!(c.carrier(), want, "carrier");
        assert!(w.features.iter().any(|f| f == wire::FEATURE_EXEC), "welcome {w:?}");
        c
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        unsafe { libc::kill(self.server.id() as i32, libc::SIGTERM) };
        let _ = self.server.wait();
        // Holders outlive the server by design; these are ours.
        let _ = Command::new("pkill").arg("-KILL").arg("-f").arg(self.run_dir.as_os_str()).status();
    }
}

/// The mirror as a renderer that also counts snapshots begun.
struct Counting(Mirror, Arc<Mutex<usize>>);

impl Renderer for Counting {
    fn snapshot_begin(&mut self, b: &wire::SnapshotBegin) -> Result<(), RendererError> {
        *self.1.lock().unwrap() += 1;
        Renderer::snapshot_begin(&mut self.0, b)
    }
    fn snapshot_data(&mut self, data: &[u8], history: bool) -> Result<(), RendererError> {
        Renderer::snapshot_data(&mut self.0, data, history)
    }
    fn snapshot_ready(&mut self) -> Result<(), RendererError> {
        Renderer::snapshot_ready(&mut self.0)
    }
    fn snapshot_end(&mut self) -> Result<(), RendererError> {
        Renderer::snapshot_end(&mut self.0)
    }
    fn output(&mut self, data: &[u8]) -> Result<(), RendererError> {
        Renderer::output(&mut self.0, data)
    }
    fn event(&mut self, e: &wire::EventMsg, live: bool) -> Result<(), RendererError> {
        Renderer::event(&mut self.0, e, live)
    }
    fn end(&mut self) {
        Renderer::end(&mut self.0)
    }
}

fn wait_text(m: &Mirror, s: &str) {
    if let Err(e) = block_on(timeout(WAIT, m.wait_text(s))).unwrap_or(Err("timed out".into())) {
        panic!("waiting for {s:?}: {e}\nscreen:\n{}", m.visible_text());
    }
}

fn echo_server() -> u16 {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in ln.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = c;
                let mut b = [0u8; 4096];
                while let Ok(n) = c.read(&mut b) {
                    if n == 0 || c.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Everything the Rust client does, against one server.
fn client_against(bin: &Path, tag: &str, carrier: &'static str) {
    let mut rig = Rig::start(bin, tag);
    if carrier != "ws" {
        // The first connection learns the QUIC port from WELCOME, and
        // hosts.json keeps it for the next ones.
        let c = rig.dial("rust-probe");
        let deadline = Instant::now() + WAIT;
        while hosts::find(&rig.client_dir, &rig.host.name).unwrap().quic_port == 0 {
            assert!(Instant::now() < deadline, "WELCOME's QUIC port not saved");
            std::thread::sleep(Duration::from_millis(10));
        }
        run(c.close());
    }
    rig.carrier = carrier;

    // login's check of the saved host: a full handshake as this device.
    run(osewa_client::login::check_host(&rig.client_dir, &rig.host, Duration::from_secs(5))).unwrap();

    let c = rig.dial("rust-a");
    assert!(run(c.list()).unwrap().is_empty(), "a fresh server lists sessions");

    // Attach with a snapshot of output from before the attach.
    let id = run(c.create(CreateOptions {
        cols: 80,
        rows: 24,
        argv: argv(&["/bin/sh", "-c", "printf 'before attach\\nhome %s\\ncwd %s\\n' \"$HOME\" \"$(pwd -P)\"; exec cat"]),
        ..Default::default()
    }))
    .unwrap();
    assert_eq!(id.len(), 32);
    let deadline = Instant::now() + WAIT;
    while !run(c.list()).unwrap().iter().any(|s| s.id == id) {
        assert!(Instant::now() < deadline, "session not listed");
    }
    std::thread::sleep(Duration::from_millis(200));
    let m = Mirror::new(Default::default());
    let begins = Arc::new(Mutex::new(0));
    let s = run(c.attach(
        &id,
        AttachOptions {
            cols: 80,
            rows: 24,
            own: true,
            renderer: Some(Box::new(Counting(m.clone(), begins.clone()))),
            ..Default::default()
        },
    ))
    .unwrap();
    let home = osewa_paths::sandbox_home().display().to_string();
    wait_text(&m, &format!("before attach\nhome {home}\ncwd {home}"));

    // Input echo: the tty echoes, then cat.
    s.send_text("one\r").unwrap();
    run(s.flush()).unwrap();
    let deadline = Instant::now() + WAIT;
    while m.visible_text().matches("one").count() < 2 {
        assert!(Instant::now() < deadline, "no echo:\n{}", m.visible_text());
        std::thread::sleep(Duration::from_millis(10));
    }

    // A dropped connection: typing meanwhile is queued, and resume on a new
    // one continues from the position, without a snapshot, and delivers the
    // input once.
    c.close_now();
    let deadline = Instant::now() + WAIT;
    while s.connected() {
        assert!(Instant::now() < deadline, "still connected");
        std::thread::sleep(Duration::from_millis(10));
    }
    s.send_text("two\r").unwrap();
    let c2 = rig.dial("rust-a");
    run(s.resume(&c2)).unwrap();
    run(s.flush()).unwrap();
    let deadline = Instant::now() + WAIT;
    while m.visible_text().matches("two").count() < 2 {
        assert!(Instant::now() < deadline, "no echo after resume:\n{}", m.visible_text());
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(m.visible_text().matches("two").count(), 2, "input applied more than once:\n{}", m.visible_text());
    assert_eq!(*begins.lock().unwrap(), 1, "resume took a snapshot instead of continuing from the position");

    // exec: exit codes, both channels, more output than the window of
    // credit, and an upload through GRANT.
    let o = run(c2.output(ExecOptions { argv: argv(&["sh", "-c", "echo out; echo err >&2; exit 3"]), ..Default::default() })).unwrap();
    assert_eq!((o.code, o.stdout.as_slice(), o.stderr.as_slice()), (3, &b"out\n"[..], &b"err\n"[..]));
    let o = block_on(timeout(
        Duration::from_secs(60),
        c2.output(ExecOptions { argv: argv(&["head", "-c", "10000000", "/dev/zero"]), ..Default::default() }),
    ))
    .expect("timed out")
    .unwrap();
    assert!(o.code == 0 && o.stdout.len() == 10_000_000, "exec: {} bytes, code {}", o.stdout.len(), o.code);
    let e = run(c2.exec(ExecOptions { argv: argv(&["wc", "-c"]), ..Default::default() })).unwrap();
    let upload = vec![b'x'; 6_000_000];
    block_on(timeout(Duration::from_secs(60), e.stdin().write_all(&upload))).expect("timed out").unwrap();
    e.stdin().close().unwrap();
    let out = run(e.stdout().read_to_end()).unwrap();
    assert_eq!(String::from_utf8_lossy(&out).trim(), "6000000");
    assert_eq!(run(e.wait()).unwrap(), 0);
    let err = run(c2.exec(ExecOptions { argv: argv(&["osewa-no-such-program"]), ..Default::default() })).err().unwrap();
    assert!(err.is_code(wire::ERR_NOT_FOUND), "missing program: {err}");

    // forward: to a TCP echo server, through the server.
    let port = echo_server();
    let fc = run(c2.dial_forward("127.0.0.1", port)).unwrap();
    let msg = b"through the server".repeat(1000);
    run(fc.write_all(&msg)).unwrap();
    let mut back = vec![0u8; msg.len()];
    let mut n = 0;
    while n < back.len() {
        let k = run(fc.read(&mut back[n..])).unwrap();
        assert!(k > 0, "EOF after {n} bytes");
        n += k;
    }
    assert_eq!(back, msg);
    fc.close();

    // Kill: the session ends for the attached client with the code.
    run(c2.kill(&id)).unwrap();
    block_on(timeout(WAIT, s.done())).expect("killed session not done");
    let list = run(c2.list()).unwrap();
    assert!(list.iter().all(|i| i.id != id || i.exited), "killed session still running: {list:?}");
    run(c2.close());
    m.close();
}

#[test]
fn rust_client_against_rust_server() {
    client_against(Path::new(RUST_OSEWA), "rs", "ws");
}

#[test]
fn rust_client_against_rust_server_quic() {
    client_against(Path::new(RUST_OSEWA), "rsq", "quic");
}

/// No carrier asked for and the port known: QUIC wins the race.
#[test]
fn rust_client_against_rust_server_race() {
    client_against(Path::new(RUST_OSEWA), "rsr", "");
}

/// A Link survives its connection dropping: the session is resumed by
/// itself, and input typed meanwhile arrives once.
#[test]
fn link_resumes_after_drop() {
    let rig = Rig::start(Path::new(RUST_OSEWA), "lk");
    let l = osewa_client::Link::new(rig.config("rust-l"));
    let c = run(l.conn()).unwrap();
    let id = run(c.create(CreateOptions { cols: 80, rows: 24, argv: argv(&["/bin/cat"]), ..Default::default() })).unwrap();
    let m = Mirror::new(Default::default());
    let s = run(l.attach(&id, AttachOptions { cols: 80, rows: 24, own: true, renderer: Some(Box::new(m.clone())), ..Default::default() })).unwrap();
    s.send_text("first\r").unwrap();
    wait_text(&m, "first\nfirst");
    // Cut the connection under the link: the server drops it.
    c.close_now();
    s.send_text("second\r").unwrap();
    wait_text(&m, "second\nsecond");
    let c2 = run(l.conn()).unwrap();
    assert!(c2 != c, "same connection after the drop");
    run(l.close());
    m.close();
}

/// One connection, many downloads, each ended early with output still in
/// flight, as a user stops a transfer: every new one must get the
/// connection's full room again, not what the earlier ones left uncredited.
#[test]
fn downloads_closed_early_leave_room() {
    let rig = Rig::start(Path::new(RUST_OSEWA), "closed-early");
    let c = rig.dial("rust-closed-early");
    let sess_id = run(c.create(CreateOptions { cols: 80, rows: 24, argv: argv(&["cat"]), ..Default::default() })).unwrap();
    let sess = run(c.attach(&sess_id, AttachOptions { cols: 80, rows: 24, own: true, ..Default::default() })).unwrap();
    for i in 0..40 {
        let e = run(c.exec(ExecOptions { argv: argv(&["cat", "/dev/urandom"]), ..Default::default() })).unwrap();
        let out = e.stdout();
        let want = 4 << 20;
        let (mut got, mut buf, t0) = (0usize, vec![0u8; 64 << 10], Instant::now());
        while got < want {
            let n = block_on(timeout(Duration::from_secs(5), out.read(&mut buf))).unwrap_or_else(|| panic!("download {i} stalled after {got} bytes")).unwrap();
            assert!(n > 0, "download {i} ended");
            got += n;
            // Someone types meanwhile, now and then.
            if i % 3 == 0 && got % (1 << 20) < n {
                sess.send_text("x").unwrap();
            }
        }
        let el = t0.elapsed();
        assert!(el < Duration::from_secs(5), "download {i}: {want} bytes took {el:?}");
        run(e.close()).unwrap();
    }
    sess.close();
    run(c.close());
}
