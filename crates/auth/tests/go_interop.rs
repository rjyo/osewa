//! Interop with the Go osewa auth package: files Go wrote (testdata/go,
//! made by testdata/goauth's "fixtures" mode) load here and get rewritten
//! byte-for-byte as Go would, and, when a Go toolchain and the Go checkout
//! are around, Go reads our files and Go and Rust complete (and refuse)
//! TLS handshakes with each other.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use osewa_auth::*;
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection, StreamOwned};

const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "osewa-auth-interop-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
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

/// Copies a fixture in with mode 0600 (git keeps only the executable bit).
fn fixture(name: &str, to: &Path) {
    fs::copy(Path::new(TESTDATA).join("go").join(name), to).unwrap();
    fs::set_permissions(to, fs::Permissions::from_mode(0o600)).unwrap();
}

fn go_fixture(name: &str) -> String {
    fs::read_to_string(Path::new(TESTDATA).join("go").join(name)).unwrap()
}

#[test]
fn go_key_loads() {
    let t = TempDir::new();
    let path = t.0.join("server.key");
    fixture("server.key", &path);
    let id = Identity::load_or_create(&path).unwrap();
    assert_eq!(encode_key(&id.public()), go_fixture("server.pub").trim());

    // A key we create has Go's layout: same length, same PKCS#8 header.
    let ours = t.0.join("ours.key");
    Identity::load_or_create(&ours).unwrap();
    let (go, rs) = (go_fixture("server.key"), fs::read_to_string(&ours).unwrap());
    assert_eq!(go.len(), rs.len());
    let header = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEI";
    assert!(go.starts_with(header) && rs.starts_with(header), "{rs}");
}

#[test]
fn go_devices_load_and_rewrite_identically() {
    let t = TempDir::new();
    let d = Devices::new(t.0.join("devices"));
    fixture("devices.full", &d.path);
    let names: Vec<String> = d.list().unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["phone", "laptop", "jyo's <iPad> & co\u{2028}"]);
    // Go removed "laptop" from the same file; our rewrite is the same bytes.
    d.remove("laptop").unwrap();
    assert_eq!(fs::read_to_string(&d.path).unwrap(), go_fixture("devices.removed"));
    assert_eq!(fs::metadata(&d.path).unwrap().permissions().mode() & 0o777, 0o600);
}

/// The goauth helper, built against the Go checkout, or None (and the
/// Go-driven tests skip) without a Go toolchain or checkout.
fn goauth() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let repo = std::env::var_os("OSEWA_GO_DIR").map(PathBuf::from).or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|a| a.join("osewa")).find(|p| {
                fs::read_to_string(p.join("go.mod")).is_ok_and(|m| m.starts_with("module github.com/rjyo/osewa\n"))
            })
        });
        let Some(repo) = repo else {
            eprintln!("skipping Go interop: no Go osewa checkout (set OSEWA_GO_DIR)");
            return None;
        };
        let dir = std::env::temp_dir().join(format!("osewa-auth-goauth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(Path::new(TESTDATA).join("goauth/main.go"), dir.join("main.go")).unwrap();
        // Same go line as the checkout, so the toolchain doesn't want to
        // upgrade our go.mod.
        let repo_mod = fs::read_to_string(repo.join("go.mod")).unwrap();
        let go_line = repo_mod.lines().find(|l| l.starts_with("go ")).unwrap_or("go 1.24");
        let gomod = format!(
            "module goauth\n\n{go_line}\n\nrequire github.com/rjyo/osewa v0.0.0\n\nreplace github.com/rjyo/osewa => {}\n",
            repo.display()
        );
        fs::write(dir.join("go.mod"), gomod).unwrap();
        let bin = dir.join("goauth");
        let built = Command::new("go")
            .args(["build", "-o"])
            .arg(&bin)
            .arg(".")
            .current_dir(&dir)
            .env("GOFLAGS", "-mod=mod")
            .status();
        match built {
            Ok(s) if s.success() => Some(bin),
            Ok(s) => panic!("building goauth: {s}"),
            Err(e) => {
                eprintln!("skipping Go interop: go: {e}");
                None
            }
        }
    })
    .as_deref()
}

fn go(args: &[&dyn AsRef<std::ffi::OsStr>]) -> Option<String> {
    let mut cmd = Command::new(goauth()?);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.stderr(Stdio::inherit()).output().unwrap();
    assert!(out.status.success(), "goauth {:?}: {}", cmd.get_args().collect::<Vec<_>>(), out.status);
    Some(String::from_utf8(out.stdout).unwrap())
}

#[test]
fn go_reads_rust_files() {
    if goauth().is_none() {
        return;
    }
    let t = TempDir::new();
    let id = Identity::load_or_create(&t.0.join("server.key")).unwrap();
    let d = Devices::new(t.0.join("devices"));
    let phone = encode_key(&Identity::generate().public());
    d.add(Device { name: "phone <&>\u{2028}".into(), key: phone.clone() }).unwrap();
    let go_key = encode_key(&Identity::generate().public());
    fs::write(t.0.join("go.pub"), &go_key).unwrap();

    let out = go(&[&"check", &t.0]).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["key"], encode_key(&id.public()), "Go loaded another key from ours");
    assert_eq!(v["devices"], serde_json::json!([{ "name": "phone <&>\u{2028}", "key": phone }]));
    // And we read the file Go rewrote.
    let l = d.list().unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[1], Device { name: "go".into(), key: go_key });
}

/// What a Rust server saw of one connection.
type Seen = Result<PublicKey, io::Error>;

/// Serves n connections with a Rust server: handshake, send "hello\n", read
/// to EOF.
fn rust_server(srv: ServerConfig, n: usize) -> (String, std::thread::JoinHandle<Vec<Seen>>) {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap().to_string();
    let srv = Arc::new(srv);
    let t = std::thread::spawn(move || {
        (0..n)
            .map(|_| {
                let (sock, _) = ln.accept().unwrap();
                sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut s = StreamOwned::new(ServerConnection::new(srv.clone()).unwrap(), sock);
                let r = s.conn.complete_io(&mut s.sock).and_then(|_| {
                    s.write_all(b"hello\n")?;
                    s.flush()?;
                    let _ = s.read_to_end(&mut Vec::new());
                    peer_key(&s.conn).ok_or_else(|| io::Error::other("no peer key"))
                });
                s.conn.send_close_notify();
                let _ = s.conn.complete_io(&mut s.sock);
                r
            })
            .collect()
    });
    (addr, t)
}

fn reason(r: &Seen) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => auth_error(e).map_or_else(|| e.to_string(), |a| a.to_string()),
    }
}

#[test]
fn go_client_rust_server() {
    if goauth().is_none() {
        return;
    }
    let t = TempDir::new();
    let server = Identity::generate();
    let (dev_path, stranger_path) = (t.0.join("device.key"), t.0.join("stranger.key"));
    let device = Identity::load_or_create(&dev_path).unwrap();
    let stranger = Identity::load_or_create(&stranger_path).unwrap();
    let d = Devices::new(t.0.join("devices"));
    d.add(Device { name: "phone".into(), key: encode_key(&device.public()) }).unwrap();
    let (addr, seen) = rust_server(server_config(&server, &d), 4);
    let pin = encode_key(&server.public());
    let wrong = encode_key(&stranger.public());

    let client = |key: &Path, pin: &str, ver: &str| go(&[&"client", &key, &pin, &addr, &ver]).unwrap();
    let ok = client(&dev_path, &pin, "1.3");
    let unknown = client(&stranger_path, &pin, "1.3");
    let bad_pin = client(&dev_path, &wrong, "1.3");
    let tls12 = client(&dev_path, &pin, "1.2");
    let seen = seen.join().unwrap();

    assert_eq!(ok, "ok hello\n");
    assert_eq!(seen[0].as_ref().ok(), Some(&device.public()), "server saw {}", reason(&seen[0]));
    assert!(unknown.starts_with("err "), "unknown device: {unknown}");
    assert_eq!(reason(&seen[1]), "auth: device not enrolled");
    assert!(bad_pin.contains("auth: server key does not match pin"), "wrong pin: {bad_pin}");
    assert!(seen[2].is_err());
    assert!(tls12.starts_with("err "), "TLS 1.2: {tls12}");
    assert!(seen[3].is_err(), "TLS 1.2 accepted");
}

/// A running goauth server and its stdout.
struct GoServer {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    addr: String,
}

impl GoServer {
    fn next(&mut self) -> String {
        let mut l = String::new();
        self.out.read_line(&mut l).unwrap();
        l
    }
}

impl Drop for GoServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn dial(cli: ClientConfig, addr: &str) -> io::Result<String> {
    let sock = TcpStream::connect(addr)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut c = StreamOwned::new(ClientConnection::new(Arc::new(cli), "osewa".try_into().unwrap()).unwrap(), sock);
    let mut line = String::new();
    BufReader::new(&mut c).read_line(&mut line)?;
    if line.is_empty() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line)
}

#[test]
fn rust_client_go_server() {
    let Some(bin) = goauth() else { return };
    let t = TempDir::new();
    let (device, stranger) = (Identity::generate(), Identity::generate());
    let d = Devices::new(t.0.join("devices"));
    d.add(Device { name: "phone".into(), key: encode_key(&device.public()) }).unwrap();
    // Go creates the server key; we read it for the pin.
    let key_path = t.0.join("server.key");
    let mut child = Command::new(bin)
        .arg("server")
        .arg(&key_path)
        .arg(&d.path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut srv = GoServer { child, out, addr: String::new() };
    srv.addr = srv.next().trim().to_string();
    let pin = Identity::load_or_create(&key_path).unwrap().public();

    let line = dial(client_config(&device, pin), &srv.addr).unwrap();
    assert_eq!(line, "hello\n");
    assert_eq!(srv.next(), format!("ok {}\n", encode_key(&device.public())));

    assert!(dial(client_config(&stranger, pin), &srv.addr).is_err(), "unknown device accepted");
    let l = srv.next();
    assert!(l.starts_with("err ") && l.contains("auth: device not enrolled"), "{l}");

    let err = dial(client_config(&device, stranger.public()), &srv.addr).expect_err("wrong pin accepted");
    assert!(matches!(auth_error(&err), Some(Error::ServerKey)), "{err}");
    assert!(srv.next().starts_with("err "));
}
