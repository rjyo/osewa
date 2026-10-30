//! The Go osewa CLI against the Rust `osewa server`: ls, new, kill, exec
//! (stdin, exit codes, a large output under flow control) and forward,
//! over pinned mutual TLS and WebSocket, with Rust holders spawned by the
//! Rust server. Skips without a Go toolchain or the Go checkout (set
//! OSEWA_GO_DIR to point at one).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::*;

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
