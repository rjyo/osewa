//! `osewa server [--daemon]`. One server per user: it
//! holds an exclusive lock on <run dir>/server.pid for its life, however it
//! was started (by hand, --daemon, systemd, launchd), so "is one running?"
//! has one answer.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::Out;

/// Bounds how long --daemon waits for the server to listen.
pub(crate) const DAEMON_WAIT: Duration = Duration::from_secs(10);

pub(crate) fn pid_path() -> PathBuf {
    osewa_paths::run_dir().join("server.pid")
}

/// Holds the server log: $XDG_STATE_HOME/osewa, else ~/.local/state/osewa.
pub(crate) fn state_dir() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(d) if !d.is_empty() => Path::new(&d).join("osewa"),
        _ => osewa_paths::home().join(".local").join("state").join("osewa"),
    }
}

pub(crate) fn log_path() -> PathBuf {
    state_dir().join("server.log")
}

/// `osewa server [--daemon] [server flags]`.
pub(crate) fn server_main(args: &[String], mut stdout: Out, mut stderr: Out) -> i32 {
    // Before anything else (the running server, the lock): a test's server
    // must never start on the real home.
    if let Err(e) = osewa_paths::home_override().and_then(|h| osewa_paths::check_home(h.as_deref())) {
        let _ = writeln!(stderr, "osewa server: {e}");
        return 2;
    }
    let daemon = args.iter().any(|a| a == "--daemon" || a == "-daemon");
    let rest: Vec<String> = args.iter().filter(|a| *a != "--daemon" && *a != "-daemon").cloned().collect();
    if daemon {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                let _ = writeln!(stderr, "osewa server: {e}");
                return 1;
            }
        };
        return start_daemon(&exe, &rest, &mut stdout, &mut stderr);
    }
    let _lock = match lock_server() {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(stderr, "osewa server: {e}");
            return 1;
        }
    };
    crate::autoupdate::spawn_server_updater(&mut stderr);
    osewa_server::main(&rest)
}

/// The server lock, held while this lives.
pub(crate) struct Lock(#[allow(dead_code)] File);

fn flock(f: &File, how: libc::c_int) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), how | libc::LOCK_NB) == 0 }
}

/// Takes the server lock and records our pid, or reports the server that
/// holds it.
pub(crate) fn lock_server() -> Result<Lock, String> {
    osewa_paths::ensure(&osewa_paths::run_dir()).map_err(|e| e.to_string())?;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(pid_path())
        .map_err(|e| e.to_string())?;
    if !flock(&f, libc::LOCK_EX) {
        let mut b = String::new();
        let _ = f.read_to_string(&mut b);
        return Err(format!("already running (pid {})", b.trim()));
    }
    let _ = f.set_len(0);
    let _ = f.seek(SeekFrom::Start(0));
    let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
    Ok(Lock(f))
}

/// The pid of the server holding the lock, if any.
pub(crate) fn running_server() -> Option<i32> {
    let mut f = File::open(pid_path()).ok()?;
    if flock(&f, libc::LOCK_SH) {
        return None; // nobody holds it
    }
    let mut b = String::new();
    let _ = f.read_to_string(&mut b);
    Some(b.trim().parse().unwrap_or(0))
}

/// Where the server will listen, from its flags, made dialable on this
/// machine.
pub(crate) fn listen_addr(args: &[String]) -> String {
    let mut addr = format!(":{}", osewa_auth::DEFAULT_PORT);
    for (i, a) in args.iter().enumerate() {
        if (a == "--listen" || a == "-listen") && i + 1 < args.len() {
            addr = args[i + 1].clone();
        } else if let Some(v) = a.strip_prefix("--listen=").or_else(|| a.strip_prefix("-listen=")) {
            addr = v.into();
        }
    }
    let Some((host, port)) = split_host_port(&addr) else { return addr };
    let host = if host.is_empty() || host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
    crate::channels::join_host_port(host, port)
}

/// Splits host:port, [v6]:port included, for the forms a listen address
/// takes.
fn split_host_port(addr: &str) -> Option<(&str, &str)> {
    if let Some(r) = addr.strip_prefix('[') {
        let (h, p) = r.split_once("]:")?;
        return Some((h, p));
    }
    let (h, p) = addr.rsplit_once(':')?;
    if h.contains(':') {
        return None;
    }
    Some((h, p))
}

/// Starts the server (exe server args...) detached, logging to
/// server.log, unless one is already running, and returns once it listens.
pub(crate) fn start_daemon(exe: &Path, args: &[String], stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    if let Some(pid) = running_server() {
        let _ = writeln!(stdout, "osewa server: already running (pid {pid})");
        return 0;
    }
    let fail = |stderr: &mut dyn Write, e: &dyn std::fmt::Display| {
        let _ = writeln!(stderr, "osewa server: {e}");
        1
    };
    if let Err(e) = osewa_paths::ensure(&state_dir()) {
        return fail(stderr, &e);
    }
    let log = match OpenOptions::new().append(true).create(true).mode(0o600).open(log_path()) {
        Ok(f) => f,
        Err(e) => return fail(stderr, &e),
    };
    let start = log.metadata().map(|m| m.len()).unwrap_or(0);
    let (out, err) = match (log.try_clone(), log.try_clone()) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fail(stderr, &e),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("server").args(args).stdin(Stdio::null()).stdout(out).stderr(err);
    // Its own session: it outlives this ssh command and its terminal.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(stderr, &e),
    };
    let pid = child.id() as i32;
    let addr = listen_addr(args);
    let deadline = Instant::now() + DAEMON_WAIT;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            let _ = write!(stderr, "osewa server: exited at start; {} says:\n{}", log_path().display(), log_since(start));
            return 1;
        }
        if Instant::now() > deadline {
            let _ = writeln!(stderr, "osewa server: not listening on {addr} after {DAEMON_WAIT:?}; see {}", log_path().display());
            return 1;
        }
        std::thread::sleep(Duration::from_millis(50));
        if running_server() == Some(pid) && our_server_at(&addr) {
            let _ = writeln!(stdout, "osewa server: started (pid {pid}, {addr}, log {})", log_path().display());
            // Reaped by init once we exit; a long-lived caller (a test)
            // reaps it here when it ends.
            std::thread::spawn(move || child.wait());
            return 0;
        }
    }
}

/// Reports whether this user's server answers at addr: its TLS certificate
/// carries the server key. Anything else could hold the port: the child
/// locks the pidfile before it binds, and fails if it can't.
pub(crate) fn our_server_at(addr: &str) -> bool {
    // The server writes it before it listens.
    if !osewa_auth::identity_path().exists() {
        return false;
    }
    let Ok(id) = osewa_auth::Identity::load_or_create(&osewa_auth::identity_path()) else { return false };
    let seen = Arc::new(std::sync::Mutex::new(None));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Seen { seen: seen.clone(), provider }))
        .with_no_client_auth();
    use std::net::ToSocketAddrs;
    let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else { return false };
    let Ok(mut tcp) = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(1)) else { return false };
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(1)));
    let Ok(name) = rustls::pki_types::ServerName::try_from("osewa") else { return false };
    let Ok(mut conn) = rustls::ClientConnection::new(Arc::new(cfg), name) else { return false };
    // Without a client certificate the server ends the handshake, after its
    // own certificate went by.
    let _ = conn.complete_io(&mut tcp);
    let key = *seen.lock().unwrap();
    key == Some(id.public())
}

/// Accepts any certificate, keeping the key it is over.
#[derive(Debug)]
struct Seen {
    seen: Arc<std::sync::Mutex<Option<osewa_auth::PublicKey>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Seen {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = osewa_auth::cert_key(end_entity);
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// What the log got from offset on.
fn log_since(off: u64) -> String {
    let Ok(mut f) = File::open(log_path()) else { return String::new() };
    let _ = f.seek(SeekFrom::Start(off));
    let mut b = Vec::new();
    let _ = f.take(4096).read_to_end(&mut b);
    if !b.is_empty() && !b.ends_with(b"\n") {
        b.push(b'\n');
    }
    String::from_utf8_lossy(&b).into_owned()
}
