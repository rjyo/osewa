//! The osewa-bench binary as the e2e rig runs it: exit codes, usage,
//! pipes, drive watching gen's stamps in a PTY, and cold resumes against a
//! real Rust server.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BENCH: &str = env!("CARGO_BIN_EXE_osewa-bench");

#[derive(serde::Deserialize, Debug)]
struct WatchResult {
    samples: Option<Vec<Sample>>,
    bytes: usize,
}

#[derive(serde::Deserialize, Debug)]
struct Sample {
    at_ms: i64,
    age_ms: i64,
}

#[derive(serde::Deserialize, Debug)]
struct ResumeResult {
    total_ms: Option<Vec<f64>>,
    dial_ms: Option<Vec<f64>>,
    mismatch: usize,
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// drive renders gen's stamped output and samples its age.
#[test]
fn drive_watch() {
    let mut out = Vec::new();
    let code = osewa_bench::run(
        &args(&["drive", "-mode", "watch", "-ready", "@T", "-settle", "0", "-duration", "500ms", "--", BENCH, "gen", "-kind", "w1", "-rate", "20000", "-duration", "3s"]),
        &mut out,
        &mut std::io::stderr(),
    );
    assert_eq!(code, 0, "drive failed");
    let r: WatchResult = serde_json::from_slice(&out).unwrap();
    let samples = r.samples.unwrap_or_default();
    assert!(samples.len() >= 10 && r.bytes > 0, "{} samples, {} bytes", samples.len(), r.bytes);
    for s in &samples {
        assert!(s.age_ms >= 0 && s.age_ms <= 1000 && s.at_ms > 0, "screen age {} ms", s.age_ms);
    }
}

fn bench(a: &[&str], stdin: &[u8]) -> std::process::Output {
    let mut c = Command::new(BENCH).args(a).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(stdin).unwrap();
    c.wait_with_output().unwrap()
}

#[test]
fn binary_exit_codes_and_pipes() {
    let o = bench(&[], b"");
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&o.stderr), "usage: osewa-bench gen|drive|clock|resume|serve|bytes|vs [flags]\n");
    let o = bench(&["bogus"], b"");
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&o.stderr), "osewa-bench: unknown command \"bogus\"\n");

    let o = bench(&["bytes", "-n", "100003"], b"");
    assert!(o.status.success() && o.stdout.len() == 100003);
    let c = bench(&["bytes", "-check", "-n", "100003"], &o.stdout);
    assert!(c.status.success() && c.stdout == b"ok\n", "{:?}", String::from_utf8_lossy(&c.stderr));
    let c = bench(&["bytes", "-check", "-n", "100004"], &o.stdout);
    assert_eq!(c.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&c.stderr), "osewa-bench: pattern: short: 100003 of 100004 bytes\n");

    let o = bench(&["clock"], b"a\nb\nc");
    assert!(o.status.success());
    assert_eq!(String::from_utf8_lossy(&o.stdout).lines().count(), 3);

    // gen's stamps leave as they are written: unbuffered.
    let o = bench(&["gen", "-kind", "w3", "-rate", "100000", "-duration", "300ms"], b"");
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.starts_with("@T") && osewa_bench::stamp::stamp_indexes(&s).len() >= 3, "gen wrote {} bytes", s.len());
}

/// The Rust osewa next to this binary, built by a workspace `cargo test`,
/// or None with a note.
fn rust_osewa() -> Option<PathBuf> {
    let p = Path::new(BENCH).with_file_name("osewa");
    if p.exists() {
        Some(p)
    } else {
        eprintln!("skipping resume against a server: no {} (run the workspace's cargo test)", p.display());
        None
    }
}

/// `resume` against a real server: JSON with one total and dial per run,
/// and the screen compared with -want.
#[test]
fn resume_against_rust_server() {
    let Some(osewa) = rust_osewa() else { return };
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = PathBuf::from(format!("/tmp/osb-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&dir);
    let (server_dir, client_dir, run_dir) = (dir.join("server"), dir.join("client"), dir.join("run"));
    for d in [&server_dir, &client_dir] {
        fs::create_dir_all(d).unwrap();
        fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let server_id = osewa_auth::Identity::load_or_create(&server_dir.join("server.key")).unwrap();
    let key_path = client_dir.join("client.key");
    let client_id = osewa_auth::Identity::load_or_create(&key_path).unwrap();
    osewa_auth::Devices::new(server_dir.join("devices"))
        .add(osewa_auth::Device { name: "bench".into(), key: osewa_auth::encode_key(&client_id.public()) })
        .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let listen = format!("127.0.0.1:{port}");
    let mut server = Command::new(&osewa)
        .args(["server", "--listen", &listen, "--name", "bench", "--no-quic"])
        .env("OSEWA_CONFIG_DIR", &server_dir)
        .env("OSEWA_RUN_DIR", &run_dir)
        .env_remove("OSEWA_HOLDER")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let cleanup = || {
        let _ = Command::new("pkill").arg("-KILL").arg("-f").arg(run_dir.as_os_str()).status();
        let _ = fs::remove_dir_all(&dir);
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "the server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let server_key = osewa_auth::encode_key(&server_id.public());

    // A session with a known screen.
    let mut cfg = osewa_client::Config::default();
    cfg.url = format!("wss://{listen}/osewa");
    cfg.tls = Some(std::sync::Arc::new(osewa_auth::client_config(&client_id, server_id.public())));
    cfg.client_id = "bench-setup".into();
    let id = osewa_client::block_on(osewa_client::timeout(Duration::from_secs(10), async {
        let c = osewa_client::Conn::dial(cfg).await.unwrap();
        let id = c
            .create(osewa_client::CreateOptions {
                cols: 80,
                rows: 24,
                argv: args(&["/bin/sh", "-c", "printf 'resume target\\n'; exec cat"]),
                ..Default::default()
            })
            .await
            .unwrap();
        c.close().await;
        id
    }))
    .expect("create timed out");
    std::thread::sleep(Duration::from_millis(300));

    let want = dir.join("want");
    for (screen, mismatch) in [("resume target", 0), ("another screen", 3)] {
        fs::write(&want, screen).unwrap();
        let o = bench(
            &[
                "resume", "-addr", &listen, "-server-key", &server_key, "-key", key_path.to_str().unwrap(), "-session", &id, "-runs", "3",
                "-want", want.to_str().unwrap(),
            ],
            b"",
        );
        assert!(o.status.success(), "resume: {}", String::from_utf8_lossy(&o.stderr));
        assert!(o.stdout.ends_with(b"}\n"), "not one JSON line: {:?}", String::from_utf8_lossy(&o.stdout));
        let r: ResumeResult = serde_json::from_slice(&o.stdout).unwrap();
        let (total, dial) = (r.total_ms.unwrap(), r.dial_ms.unwrap());
        assert!(total.len() == 3 && dial.len() == 3, "{total:?} {dial:?}");
        assert!(total.iter().zip(&dial).all(|(t, d)| t >= d && *d > 0.0), "total {total:?} dial {dial:?}");
        assert_eq!(r.mismatch, mismatch, "screen {screen:?}");
    }
    unsafe { libc::kill(server.id() as i32, libc::SIGTERM) };
    let _ = server.wait();
    cleanup();
}
