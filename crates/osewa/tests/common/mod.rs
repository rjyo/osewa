//! Shared by the Go interop tests: the Go osewa checkout, its CLI and
//! client-library helper built from it, and a server with its own config
//! and run directories that a Go client knows as host "rs".

#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const RUST_OSEWA: &str = env!("CARGO_BIN_EXE_osewa");

/// The Go osewa checkout, or None with a note.
pub fn go_repo() -> Option<&'static Path> {
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
pub fn go_client_lib() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let repo = go_repo()?;
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(concat!("goclient-", env!("CARGO_CRATE_NAME")));
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
        let bin = dir.join(concat!("goclient-", env!("CARGO_CRATE_NAME")));
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
pub fn go_osewa() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let repo = go_repo()?;
        let bin = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(concat!("osewa-go-", env!("CARGO_CRATE_NAME")));
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

pub struct TempDir(pub PathBuf);

impl TempDir {
    /// Short, for Unix socket paths.
    pub fn new(tag: &str) -> TempDir {
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
pub struct Rig {
    pub server: Child,
    pub addr: String,
    pub server_key: String,
    pub client_dir: PathBuf,
    pub server_dir: PathBuf,
    pub run_dir: PathBuf,
    /// Set for every Go command run.
    pub env: Vec<(String, String)>,
    pub _dir: TempDir,
}

impl Rig {
    /// Starts the server; holder, if given, is the binary whose holders it
    /// spawns (OSEWA_HOLDER).
    pub fn start(holder: Option<&Path>, extra: &[&str]) -> Rig {
        Rig::start_bin(Path::new(RUST_OSEWA), holder, extra)
    }

    /// start, with bin's server: the Rust osewa's or the Go one's.
    pub fn start_bin(bin: &Path, holder: Option<&Path>, extra: &[&str]) -> Rig {
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
        let mut cmd = Command::new(bin);
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
        Rig { server, addr: listen, server_key, client_dir, server_dir, run_dir, env: Vec::new(), _dir: dir }
    }

    pub fn go(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(go_osewa().unwrap())
            .args(args)
            .env("OSEWA_CONFIG_DIR", &self.client_dir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
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

    pub fn go_ok(&self, args: &[&str]) -> String {
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
