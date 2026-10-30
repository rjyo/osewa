//! The Go osewa CLI against the Rust `osewa server`: ls, new, kill, exec
//! (stdin, exit codes, a large output under flow control) and forward,
//! over pinned mutual TLS and WebSocket, with Rust holders spawned by the
//! Rust server. Skips without a Go toolchain or the Go checkout (set
//! OSEWA_GO_DIR to point at one).

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const RUST_OSEWA: &str = env!("CARGO_BIN_EXE_osewa");

/// The Go osewa checkout, or None with a note.
fn go_repo() -> Option<&'static Path> {
    static REPO: OnceLock<Option<PathBuf>> = OnceLock::new();
    REPO.get_or_init(|| {
        let repo = std::env::var_os("OSEWA_GO_DIR").map(PathBuf::from).or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|a| a.join("osewa")).find(|p| {
                fs::read_to_string(p.join("go.mod")).is_ok_and(|m| m.starts_with("module github.com/rjyo/osewa\n"))
            })
        });
        if repo.is_none() {
            eprintln!("skipping Go client interop: no Go osewa checkout (set OSEWA_GO_DIR)");
        }
        repo
    })
    .as_deref()
}

/// tests/goclient built against the checkout's client library.
fn go_client_lib() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let repo = go_repo()?;
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("goclient");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/goclient/main.go"), dir.join("main.go")).unwrap();
        // The checkout's go line and sums, so the toolchain neither upgrades
        // nor fetches.
        let repo_mod = fs::read_to_string(repo.join("go.mod")).unwrap();
        let go_line = repo_mod.lines().find(|l| l.starts_with("go ")).unwrap_or("go 1.24");
        let gomod = format!(
            "module goclient\n\n{go_line}\n\nrequire github.com/rjyo/osewa v0.0.0\n\nreplace github.com/rjyo/osewa => {}\n",
            repo.display()
        );
        fs::write(dir.join("go.mod"), gomod).unwrap();
        fs::copy(repo.join("go.sum"), dir.join("go.sum")).unwrap();
        let bin = dir.join("goclient");
        let built = Command::new("go").args(["build", "-o"]).arg(&bin).arg(".").current_dir(&dir).env("GOFLAGS", "-mod=mod").status();
        match built {
            Ok(s) if s.success() => Some(bin),
            Ok(s) => panic!("building goclient: {s}"),
            Err(e) => {
                eprintln!("skipping Go client interop: go: {e}");
                None
            }
        }
    })
    .as_deref()
}

/// The Go osewa CLI built from the checkout into a temporary directory, or
/// None with a note.
fn go_osewa() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let repo = go_repo()?;
        let bin = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("osewa-go");
        match Command::new("go").args(["build", "-o"]).arg(&bin).arg("./cmd/osewa").current_dir(repo).status() {
            Ok(s) if s.success() => Some(bin),
            Ok(s) => panic!("building the Go osewa: {s}"),
            Err(e) => {
                eprintln!("skipping Go client interop: go: {e}");
                None
            }
        }
    })
    .as_deref()
}

struct TempDir(PathBuf);

impl TempDir {
    /// Short, for Unix socket paths.
    fn new(tag: &str) -> TempDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = PathBuf::from(format!("/tmp/osw-{tag}-{}-{n}", std::process::id()));
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

/// A Rust `osewa server` with its own config and run directories, and a Go
/// client configuration that knows it as host "rs".
struct Rig {
    server: Child,
    addr: String,
    server_key: String,
    client_dir: PathBuf,
    run_dir: PathBuf,
    _dir: TempDir,
}

impl Rig {
    /// Starts the server; holder, if given, is the binary whose holders it
    /// spawns (OSEWA_HOLDER).
    fn start(holder: Option<&Path>, extra: &[&str]) -> Rig {
        let dir = TempDir::new("goc");
        let (server_dir, client_dir, run_dir) = (dir.0.join("server"), dir.0.join("client"), dir.0.join("run"));
        for d in [&server_dir, &client_dir] {
            fs::create_dir_all(d).unwrap();
            fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // The server's key, and the Go client's, made here so each side can
        // pin the other: what `osewa login` would set up over ssh.
        let server_id = osewa_auth::Identity::load_or_create(&server_dir.join("server.key")).unwrap();
        let client_id = osewa_auth::Identity::load_or_create(&client_dir.join("client.key")).unwrap();
        osewa_auth::Devices::new(server_dir.join("devices"))
            .add(osewa_auth::Device { name: "go".into(), key: osewa_auth::encode_key(&client_id.public()) })
            .unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let hosts = format!(
            "[{{\"name\":\"rs\",\"addr\":\"127.0.0.1:{port}\",\"server_key\":\"{}\"}}]\n",
            osewa_auth::encode_key(&server_id.public())
        );
        let hosts_path = client_dir.join("hosts.json");
        fs::write(&hosts_path, hosts).unwrap();
        fs::set_permissions(&hosts_path, fs::Permissions::from_mode(0o600)).unwrap();

        let listen = format!("127.0.0.1:{port}");
        let mut args = vec!["server", "--listen", &listen, "--name", "rust"];
        args.extend_from_slice(extra);
        let mut cmd = Command::new(RUST_OSEWA);
        cmd.args(&args).env("OSEWA_CONFIG_DIR", &server_dir).env("OSEWA_RUN_DIR", &run_dir).stdin(Stdio::null());
        match holder {
            Some(h) => cmd.env("OSEWA_HOLDER", h),
            None => cmd.env_remove("OSEWA_HOLDER"),
        };
        let server = cmd.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(&listen).is_err() {
            assert!(Instant::now() < deadline, "the Rust server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let server_key = osewa_auth::encode_key(&server_id.public());
        Rig { server, addr: listen, server_key, client_dir, run_dir, _dir: dir }
    }

    fn go(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(go_osewa().unwrap())
            .args(args)
            .env("OSEWA_CONFIG_DIR", &self.client_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        eprintln!("osewa {}: {} {}", args.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim());
        out
    }

    fn go_ok(&self, args: &[&str]) -> String {
        let out = self.go(args, b"");
        assert!(out.status.success(), "osewa {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
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

#[test]
fn go_cli_against_rust_server() {
    let Some(go) = go_osewa() else { return };
    let rig = Rig::start(None, &["--no-quic"]);

    assert_eq!(rig.go_ok(&["ls", "rs"]).lines().count(), 1, "a fresh server lists only the header");

    // A session: created by the Rust server's own holder, listed, killed.
    let id = rig.go_ok(&["new", "rs", "--", "/bin/sh", "-c", "sleep 300"]).trim().to_string();
    assert_eq!(id.len(), 32, "session id {id:?}");
    let ls = rig.go_ok(&["ls", "rs"]);
    assert!(ls.lines().any(|l| l.starts_with(&id) && l.contains("running")), "ls:\n{ls}");
    rig.go_ok(&["kill", "rs", &id]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ls = rig.go_ok(&["ls", "rs"]);
        if ls.lines().any(|l| l.starts_with(&id) && l.contains("exited 129")) {
            break;
        }
        assert!(Instant::now() < deadline, "killed session not exited 129:\n{ls}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // exec: stdout, stderr, the exit code and stdin.
    let out = rig.go(&["exec", "rs", "--", "sh", "-c", "echo out; echo err >&2; exit 3"], b"");
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("err\n"));
    let out = rig.go(&["exec", "rs", "--", "cat"], b"through stdin\n");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "through stdin\n");
    // More than the 4 MiB window of credit: the Go client's CREDIT and the
    // Rust server's in-flight windows keep it moving.
    let out = rig.go(&["exec", "rs", "--", "head", "-c", "10000000", "/dev/zero"], b"");
    assert!(out.status.success());
    assert_eq!(out.stdout.len(), 10_000_000);
    // And the other way: an upload through GRANT.
    let upload = vec![b'x'; 6_000_000];
    let out = rig.go(&["exec", "rs", "--", "wc", "-c"], &upload);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "6000000");
    let out = rig.go(&["exec", "rs", "--", "osewa-no-such-program"], b"");
    assert!(!out.status.success(), "a missing program ran");

    // forward: a local port through the Rust server to a TCP echo server.
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let tport = target.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in target.incoming().flatten() {
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
    let lport = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let spec = format!("127.0.0.1:{lport}:127.0.0.1:{tport}");
    let mut fwd = Command::new(go)
        .args(["forward", "rs", &spec])
        .env("OSEWA_CONFIG_DIR", &rig.client_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut conn = loop {
        match TcpStream::connect(("127.0.0.1", lport)) {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("forward did not listen: {e}"),
        }
    };
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let msg = b"through a Rust server".repeat(1000);
    conn.write_all(&msg).unwrap();
    let mut back = vec![0u8; msg.len()];
    conn.read_exact(&mut back).unwrap();
    assert_eq!(back, msg);
    let _ = fwd.kill();
    let _ = fwd.wait();
}

// --no-exec and --no-forward reach the Go client as missing features.
#[test]
fn go_cli_sees_disabled_features() {
    let Some(go) = go_osewa() else { return };
    let _ = go;
    let rig = Rig::start(None, &["--no-quic", "--no-exec"]);
    let out = rig.go(&["exec", "rs", "--", "true"], b"");
    assert!(!out.status.success(), "exec ran with --no-exec");
    rig.go_ok(&["ls", "rs"]);
}

// OSEWA_HOLDER makes the Rust server spawn another build's holders: here
// the Go osewa's, which the Go CLI then lists and kills through it.
#[test]
fn rust_server_spawns_go_holders() {
    let Some(go) = go_osewa() else { return };
    let rig = Rig::start(Some(go), &["--no-quic"]);
    let id = rig.go_ok(&["new", "rs", "--", "/bin/sh", "-c", "sleep 300"]).trim().to_string();
    assert_eq!(id.len(), 32, "session id {id:?}");
    let ps = Command::new("ps").args(["-axo", "command"]).output().unwrap();
    let go_holder = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .any(|l| l.starts_with(go.to_str().unwrap()) && l.contains(" holder ") && l.contains(&id));
    assert!(go_holder, "no Go holder for {id}");
    let ls = rig.go_ok(&["ls", "rs"]);
    assert!(ls.lines().any(|l| l.starts_with(&id) && l.contains("running")), "ls:\n{ls}");
    rig.go_ok(&["kill", "rs", &id]);
}

// The Go client library: attach with a snapshot, echo, a dropped
// connection resumed on a new one without a snapshot, list and kill, all
// relayed by the Rust server to a Rust holder.
#[test]
fn go_client_library_sessions() {
    let Some(helper) = go_client_lib() else { return };
    let rig = Rig::start(None, &["--no-quic"]);
    let out = Command::new(helper)
        .arg(&rig.addr)
        .arg(&rig.server_key)
        .arg(rig.client_dir.join("client.key"))
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(out.status.success() && out.stdout == b"ok\n", "goclient: {} {:?}", out.status, String::from_utf8_lossy(&out.stdout));
}
