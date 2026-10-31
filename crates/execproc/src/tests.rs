use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::*;

// The environment is process-wide; tests that change it take turns.
static ENV: Mutex<()> = Mutex::new(());

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn sh(script: &str) -> Options {
    Options { argv: vec!["sh".into(), "-c".into(), script.into()], ..Default::default() }
}

fn argv(a: &[&str]) -> Options {
    Options { argv: a.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

/// Stops the process when the test ends.
struct Started(Proc);

impl Drop for Started {
    fn drop(&mut self) {
        self.0.stop();
        self.0.close();
    }
}

fn start(o: Options) -> Started {
    Started(Proc::start(o).unwrap())
}

/// Reads both pipes to EOF, then waits, as the server does.
async fn run(p: &mut Proc) -> (String, String, i32, bool) {
    let mut out = p.take_stdout().unwrap();
    let mut err = p.take_stderr().unwrap();
    let (o, e) = tokio::join!(out.read_to_end(), err.read_to_end());
    let (code, signaled) = p.wait().await;
    (String::from_utf8(o.unwrap()).unwrap(), String::from_utf8(e.unwrap()).unwrap(), code, signaled)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("osewa-execproc-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
    fn real(&self) -> String {
        std::fs::canonicalize(&self.0).unwrap().to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn exit_codes() {
    rt().block_on(async {
        for (script, code, signaled) in [
            ("exit 0", 0, false),
            ("exit 3", 3, false),
            ("exit 255", 255, false),
            ("kill -TERM $$", 128 + libc::SIGTERM, true),
            ("kill -KILL $$", 128 + libc::SIGKILL, true),
        ] {
            let mut p = start(sh(script));
            let (_, _, c, s) = run(&mut p.0).await;
            assert_eq!((c, s), (code, signaled), "{script}");
        }
    });
}

#[test]
fn stdout_stderr() {
    rt().block_on(async {
        let mut p = start(sh("echo out; echo err >&2"));
        let (out, err, code, _) = run(&mut p.0).await;
        assert_eq!((out.as_str(), err.as_str(), code), ("out\n", "err\n", 0));
    });
}

#[test]
fn stdin_round_trip() {
    rt().block_on(async {
        let mut p = start(argv(&["cat"]));
        // 1 MiB, far past a pipe buffer.
        let input: Vec<u8> = b"0123456789abcdef".repeat(64 << 10);
        let mut w = p.0.take_stdin().unwrap();
        let sent = input.clone();
        let writer = async move {
            w.write_all(&sent).await.unwrap();
            w.close();
        };
        let ((out, _, code, _), ()) = tokio::join!(run(&mut p.0), writer);
        assert!(out.as_bytes() == input && code == 0, "got {} bytes back, code {code}", out.len());
    });
}

// Output still in the pipe when the process exits must survive wait.
#[test]
fn wait_keeps_output() {
    rt().block_on(async {
        let mut p = start(sh("echo left in the pipe; echo and stderr >&2"));
        assert_eq!(p.0.wait().await.0, 0);
        assert_eq!(p.0.wait().await.0, 0); // again: wait is idempotent
        let mut out = p.0.take_stdout().unwrap();
        let mut err = p.0.take_stderr().unwrap();
        assert_eq!(out.read_to_end().await.unwrap(), b"left in the pipe\n");
        assert_eq!(err.read_to_end().await.unwrap(), b"and stderr\n");
        // Read to EOF closed it: reading again is still EOF, not an error.
        assert_eq!(out.read(&mut [0u8; 1]).await.unwrap(), 0);
    });
}

#[test]
fn not_found() {
    rt().block_on(async {
        for a in [&["osewa-no-such-program"][..], &["/nonexistent/osewa-no-such-program"]] {
            assert!(matches!(Proc::start(argv(a)), Err(Error::NotFound(_))), "{a:?}");
        }
        assert!(matches!(Proc::start(Options::default()), Err(Error::Other(_))), "empty argv");
        // A missing directory is a different error, not a missing program.
        let o = Options { dir: "/nonexistent/osewa".into(), ..argv(&["true"]) };
        assert!(matches!(Proc::start(o), Err(Error::Other(_))), "missing dir");
        let d = TempDir::new();
        let f = d.0.join("file");
        std::fs::write(&f, b"").unwrap();
        let o = Options { dir: f.to_string_lossy().into(), ..argv(&["true"]) };
        assert!(matches!(Proc::start(o), Err(Error::Other(_))), "dir is a file");
    });
}

#[test]
fn env_and_dir() {
    let _g = ENV.lock().unwrap();
    let dir = TempDir::new();
    unsafe {
        std::env::set_var("OSEWA_EXECPROC_INHERITED", "parent");
        std::env::set_var("OSEWA_EXECPROC_OVERRIDE", "parent");
    }
    rt().block_on(async {
        let mut p = start(Options {
            argv: vec![
                "sh".into(),
                "-c".into(),
                r#"pwd -P; echo "$OSEWA_EXECPROC_INHERITED $OSEWA_EXECPROC_OVERRIDE $OSEWA_EXECPROC_ADDED""#.into(),
            ],
            dir: dir.0.to_string_lossy().into(),
            env: vec!["OSEWA_EXECPROC_OVERRIDE=child".into(), "OSEWA_EXECPROC_ADDED=new".into()],
        });
        let (out, _, _, _) = run(&mut p.0).await;
        assert_eq!(out, format!("{}\nparent child new\n", dir.real()));
    });
}

#[test]
fn dir_defaults_to_home() {
    let _g = ENV.lock().unwrap();
    // Kept: processes other tests start meanwhile run in it, and a shell
    // whose cwd vanished complains on stderr.
    let home = std::env::temp_dir().join("osewa-execproc-home");
    std::fs::create_dir_all(&home).unwrap();
    let real = std::fs::canonicalize(&home).unwrap().to_string_lossy().into_owned();
    let old = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", &home) };
    let got = rt().block_on(async {
        let mut p = start(argv(&["pwd", "-P"]));
        run(&mut p.0).await.0
    });
    if let Some(h) = old {
        unsafe { std::env::set_var("HOME", h) };
    }
    assert_eq!(got, format!("{real}\n"));
}

#[test]
fn own_process_group() {
    rt().block_on(async {
        let p = start(argv(&["sleep", "100"]));
        let pgid = unsafe { libc::getpgid(p.0.pid()) };
        assert_eq!(pgid, p.0.pid());
    });
}

#[test]
fn stop_hangs_up() {
    rt().block_on(async {
        let mut p = start(argv(&["sleep", "100"]));
        p.0.stop();
        p.0.stop(); // once only
        assert_eq!(p.0.wait().await, (128 + libc::SIGHUP, true));
    });
}

#[test]
fn stop_continues_stopped() {
    rt().block_on(async {
        let mut p = start(argv(&["sleep", "100"]));
        unsafe { libc::kill(p.0.pid(), libc::SIGSTOP) };
        p.0.stop();
        assert_eq!(p.0.wait().await.0, 128 + libc::SIGHUP);
    });
}

/// Starts script, which prints the pid of a grandchild that ignores SIGHUP,
/// stops it, and checks every process ends within the grace.
fn stop_group(script: &str, want_code: i32) {
    let grace = Duration::from_millis(300);
    rt().block_on(async {
        let mut p = start(sh(script));
        let mut out = p.0.take_stdout().unwrap();
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while out.read(&mut b).await.unwrap() == 1 && b[0] != b'\n' {
            line.push(b[0]);
        }
        let child: i32 = String::from_utf8(line).unwrap().trim().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await; // let the trap be set
        let t0 = Instant::now();
        p.0.stopper().stop(grace);
        assert_eq!(p.0.wait().await.0, want_code, "leader code");
        while alive(child) {
            assert!(t0.elapsed() < Duration::from_secs(5), "grandchild {child} still alive");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(t0.elapsed() >= grace, "grandchild died after {:?}, before the SIGKILL", t0.elapsed());
        // With the whole group dead, stdout reaches EOF.
        out.read_to_end().await.unwrap();
    });
}

#[test]
fn stop_kills_ignoring_group() {
    // Leader and grandchild both ignore SIGHUP: SIGKILL ends both.
    stop_group(r#"trap "" HUP; sleep 100 & echo $!; wait"#, 128 + libc::SIGKILL);
}

#[test]
fn stop_kills_orphaned_grandchild() {
    // The leader dies of SIGHUP; the grandchild that ignores it must still
    // get the SIGKILL.
    stop_group(r#"(trap "" HUP; exec sleep 100) & echo $!; wait"#, 128 + libc::SIGHUP);
}

#[test]
fn close_unblocks_reader() {
    rt().block_on(async {
        let mut p = start(argv(&["sleep", "100"]));
        let mut out = p.0.take_stdout().unwrap();
        let mut stdin = p.0.take_stdin().unwrap();
        let closer = p.0.closer();
        let reader = async {
            let mut b = [0u8; 1];
            out.read(&mut b).await
        };
        let close = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            closer.close();
        };
        let (r, ()) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(reader, close) })
            .await
            .expect("close did not unblock the reader");
        assert_eq!(r.unwrap(), 0);
        // stdin is closed too.
        assert!(stdin.write_all(b"x").await.is_err(), "write after close succeeded");
    });
}

/// Writes an executable that prints its name into a new directory.
fn script(name: &str) -> TempDir {
    let d = TempDir::new();
    write_exe(&d.0.join(name), &format!("#!/bin/sh\necho {name}\n"));
    d
}

fn write_exe(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn with_path<T>(path: &str, f: impl FnOnce() -> T) -> T {
    let old = std::env::var("PATH").ok();
    unsafe { std::env::set_var("PATH", path) };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Some(p) = old {
        unsafe { std::env::set_var("PATH", p) };
    }
    r.unwrap_or_else(|e| std::panic::resume_unwind(e))
}

// A bare argv[0] resolves against the PATH the process gets, not the
// server's.
#[test]
fn path_from_env() {
    let _g = ENV.lock().unwrap();
    let in_server = script("osewa-execproc-prog");
    let in_env = script("osewa-execproc-prog");
    write_exe(&in_env.0.join("osewa-execproc-prog"), "#!/bin/sh\necho from env\n");
    let s = in_server.0.to_string_lossy().into_owned();
    let e = in_env.0.to_string_lossy().into_owned();
    with_path(&format!("{s}:/usr/bin:/bin"), || {
        rt().block_on(async {
            let mut p = start(Options {
                env: vec![format!("PATH={e}:/usr/bin:/bin")],
                ..argv(&["osewa-execproc-prog"])
            });
            let (out, _, code, _) = run(&mut p.0).await;
            assert_eq!((out.as_str(), code), ("from env\n", 0));
            // The last PATH in env wins, as it does in the child's environment.
            let mut p = start(Options {
                env: vec!["PATH=/nonexistent".into(), format!("PATH={e}")],
                ..argv(&["osewa-execproc-prog"])
            });
            assert_eq!(run(&mut p.0).await.0, "from env\n");
            // Only in the server's PATH: not found for this process.
            let empty = TempDir::new();
            let r = Proc::start(Options {
                env: vec![format!("PATH={}", empty.0.display())],
                ..argv(&["osewa-execproc-prog"])
            });
            assert!(matches!(r, Err(Error::NotFound(_))), "server-only program");
            // Relative PATH entries are skipped: they would resolve against dir.
            let r = Proc::start(Options { dir: e.clone(), env: vec!["PATH=.:".into()], ..argv(&["osewa-execproc-prog"]) });
            assert!(matches!(r, Err(Error::NotFound(_))), "relative PATH entry");
            // A path with a slash is not looked up.
            let full = format!("{s}/osewa-execproc-prog");
            let mut p = start(Options { env: vec![format!("PATH={e}")], ..argv(&[&full]) });
            assert_eq!(run(&mut p.0).await.0, "osewa-execproc-prog\n");
        })
    });
}

#[test]
fn path_falls_back_to_server() {
    let _g = ENV.lock().unwrap();
    let dir = script("osewa-execproc-prog");
    let d = dir.0.to_string_lossy().into_owned();
    with_path(&format!("{d}:/usr/bin:/bin"), || {
        rt().block_on(async {
            let mut p = start(Options { env: vec!["OTHER=1".into()], ..argv(&["osewa-execproc-prog"]) });
            let (out, _, code, _) = run(&mut p.0).await;
            assert_eq!((out.as_str(), code), ("osewa-execproc-prog\n", 0));
            // A directory or a non-executable file on PATH doesn't count.
            std::fs::create_dir(dir.0.join("osewa-execproc-dir")).unwrap();
            std::fs::write(dir.0.join("osewa-execproc-noexec"), b"").unwrap();
            for name in ["osewa-execproc-dir", "osewa-execproc-noexec"] {
                assert!(matches!(Proc::start(argv(&[name])), Err(Error::NotFound(_))), "{name}");
            }
        })
    });
}

// Errors read as v0 servers word them.
#[test]
fn error_texts() {
    rt().block_on(async {
        let e = Proc::start(argv(&["/nonexistent/osewa-no-such-program"])).err().unwrap();
        assert_eq!(e.to_string(), "execproc: program not found: fork/exec /nonexistent/osewa-no-such-program: no such file or directory");
        let e = Proc::start(Options { dir: "/nonexistent/osewa".into(), ..argv(&["true"]) }).err().unwrap();
        assert_eq!(e.to_string(), "execproc: dir: stat /nonexistent/osewa: no such file or directory");
        let e = Proc::start(argv(&["osewa-no-such-program"])).err().unwrap();
        assert!(e.to_string().starts_with("execproc: program not found: osewa-no-such-program not in PATH \""), "{e}");
    });
    assert_eq!(error_text(&io::Error::from_raw_os_error(libc::ECONNREFUSED)), "connection refused");
    assert_eq!(error_text(&io::Error::other("plain")), "plain");
}
