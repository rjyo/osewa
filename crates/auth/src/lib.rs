//! osewa's device auth (docs/PROTOCOL.md "Auth"): Ed25519 device keys, the
//! devices file, pinned mutual TLS and enrollment. Keys, the devices file and
//! the handshake are protocol v0's, interchangeable with any v0 peer.

mod devices;
mod enroll;
mod keys;
mod tls;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use devices::{Device, Devices};
pub use enroll::{devices_path, enroll, enroll_main, identity_path, EnrollRequest, EnrollResponse};
pub use keys::{decode_key, encode_key, Identity, PublicKey};
pub use tls::{auth_error, cert_key, client_config, peer_key, server_config};

/// Why auth refused something. The first three are the refusals v0 peers
/// tell apart (unknown device, server key, no peer key); their messages
/// are the protocol's.
#[derive(Debug)]
pub enum Error {
    UnknownDevice,
    ServerKey,
    NoPeerKey,
    Io(io::Error),
    Msg(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownDevice => f.write_str("auth: device not enrolled"),
            Error::ServerKey => f.write_str("auth: server key does not match pin"),
            Error::NoPeerKey => f.write_str("auth: peer did not present an Ed25519 certificate"),
            Error::Io(e) => e.fmt(f),
            Error::Msg(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

/// An I/O error naming the operation and path ("open <path>: ..."), the
/// wording users see in errors from every osewa.
fn io_at(op: &str, path: &Path, e: io::Error) -> Error {
    Error::Io(io::Error::new(e.kind(), format!("{op} {}: {e}", path.display())))
}

/// filepath.Dir: the parent, or "." for a bare name.
fn dir_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Creates a new file dir/<prefix><random> with mode 0600, as
/// os.CreateTemp(dir, prefix+"*") does.
fn create_temp(dir: &Path, prefix: &str) -> Result<(std::fs::File, TempPath), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        let mut b = [0u8; 8];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("system random");
        let p = dir.join(format!("{prefix}{}", u64::from_le_bytes(b)));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p) {
            Ok(f) => return Ok((f, TempPath(p))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_at("open", &p, e)),
        }
    }
}

/// Removes the file when dropped, so a failed write leaves no temp file.
struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod testutil;

/// Helper modes let tests run auth code in other processes, which is the
/// only way to exercise cross-process locking and process-wide stdio. The
/// test does nothing unless OSEWA_AUTH_HELPER names a mode; then it is the
/// whole process.
#[cfg(test)]
mod helper {
    use crate::*;

    #[test]
    fn run() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let code = match var("OSEWA_AUTH_HELPER").as_str() {
            "" => return,
            "enroll_main" => {
                let args: Vec<String> = var("OSEWA_ARGS").split_whitespace().map(String::from).collect();
                enroll_main(&args)
            }
            "add" => {
                let d = Devices::new(var("OSEWA_DEVICES"));
                let n: usize = var("OSEWA_N").parse().unwrap();
                let prefix = var("OSEWA_PREFIX");
                let mut code = 0;
                for i in 0..n {
                    let dev = Device { name: format!("{prefix}-{i}"), key: testutil::rand_key() };
                    if let Err(e) = d.add(dev) {
                        eprintln!("{e}");
                        code = 1;
                        break;
                    }
                }
                code
            }
            "identity" => match Identity::load_or_create(Path::new(&var("OSEWA_KEYPATH"))) {
                Ok(id) => {
                    println!("\n{}", encode_key(&id.public()));
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            },
            m => panic!("unknown helper mode {m}"),
        };
        std::process::exit(code);
    }
}

#[cfg(test)]
mod process_tests {
    use std::process::{Command, Stdio};

    use crate::testutil::TempDir;
    use crate::*;

    fn helper(mode: &str, env: &[(&str, String)]) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "helper::run", "--nocapture", "--test-threads=1"]).env("OSEWA_AUTH_HELPER", mode);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stderr(Stdio::inherit());
        cmd
    }

    // Writers in different processes (enroll over SSH, a revoke tool) must
    // not lose each other's updates.
    #[test]
    fn devices_cross_process() {
        let t = TempDir::new();
        let path = t.path().join("devices");
        let (procs, n) = (4, 25);
        let children: Vec<_> = (0..procs)
            .map(|p| {
                helper(
                    "add",
                    &[
                        ("OSEWA_DEVICES", path.display().to_string()),
                        ("OSEWA_N", n.to_string()),
                        ("OSEWA_PREFIX", format!("p{p}")),
                    ],
                )
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
            })
            .collect();
        for mut c in children {
            assert!(c.wait().unwrap().success());
        }
        let l = Devices::new(&path).list().unwrap();
        assert_eq!(l.len(), procs * n, "updates were lost");
    }

    // Processes racing to create the identity must all load the same,
    // complete key.
    #[test]
    fn identity_create_race() {
        for round in 0..10 {
            let t = TempDir::new();
            let path = t.path().join(round.to_string()).join("server.key");
            let children: Vec<_> = (0..8)
                .map(|_| {
                    helper("identity", &[("OSEWA_KEYPATH", path.display().to_string())])
                        .stdout(Stdio::piped())
                        .spawn()
                        .unwrap()
                })
                .collect();
            let want = encode_key(&Identity::load_or_create(&path).unwrap().public());
            for c in children {
                let out = c.wait_with_output().unwrap();
                assert!(out.status.success(), "round {round}");
                let stdout = String::from_utf8(out.stdout).unwrap();
                assert!(stdout.lines().any(|l| l == want), "round {round}: a process loaded another key: {stdout}");
            }
        }
    }
}
