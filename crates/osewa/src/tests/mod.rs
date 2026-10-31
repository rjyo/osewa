//! The osewa binary's tests.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::cli::{self, Ctx, Env};

mod attach;
mod channels;
mod daemon;
mod incompat;
mod live;
mod main_test;
mod render;
mod security;
mod status;
mod vtfilter;

/// A private temporary directory, short enough for Unix socket paths.
pub(crate) struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(format!("/tmp/osc{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        TempDir(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A buffer commands write to and the test reads, from any thread.
#[derive(Clone, Default)]
pub(crate) struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    pub fn string(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    pub fn reset(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// Waits for substr in buf.
#[track_caller]
pub(crate) fn wait_for(buf: &Buf, substr: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !buf.string().contains(substr) {
        assert!(Instant::now() < deadline, "timed out waiting for {substr:?} in {:?}", buf.string());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[track_caller]
pub(crate) fn eventually(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An Env writing to buffers, reading stdin, with config dir dir.
pub(crate) fn env(dir: &Path, stdin: &str) -> (Env, Buf, Buf) {
    let (out, err) = (Buf::default(), Buf::default());
    let e = Env::new(std::io::Cursor::new(stdin.as_bytes().to_vec()), out.clone(), err.clone(), dir.into());
    (e, out, err)
}

/// Runs osewa with args and an Env on dir.
pub(crate) fn run(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let (e, out, err) = env(dir, "");
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = cli::run(&args, e);
    (code, out.string(), err.string())
}

/// Runs a client command on a fresh runtime.
pub(crate) fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

pub(crate) fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn ctx() -> Ctx {
    Ctx::new()
}

/// The process environment is shared; tests that change it take turns,
/// and put it back when done.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

static ENV: Mutex<()> = Mutex::new(());

impl EnvGuard {
    pub fn new() -> EnvGuard {
        EnvGuard { _lock: ENV.lock().unwrap_or_else(|e| e.into_inner()), saved: Vec::new() }
    }

    pub fn set(&mut self, k: &str, v: impl AsRef<std::ffi::OsStr>) {
        if !self.saved.iter().any(|(n, _)| n == k) {
            self.saved.push((k.into(), std::env::var_os(k)));
        }
        unsafe { std::env::set_var(k, v) };
    }

    /// Points run, config and state dirs at fresh private directories.
    pub fn host_dirs(&mut self, keep: &mut Vec<TempDir>) {
        for (k, d) in [("OSEWA_RUN_DIR", TempDir::new()), ("OSEWA_CONFIG_DIR", TempDir::new()), ("XDG_STATE_HOME", TempDir::new())] {
            self.set(k, d.path());
            keep.push(d);
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..).rev() {
            match v {
                Some(v) => unsafe { std::env::set_var(&k, v) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
    }
}

/// The osewa binary cargo built beside this test binary.
pub(crate) fn osewa_bin() -> PathBuf {
    let me = std::env::current_exe().unwrap();
    let bin = me.parent().unwrap().parent().unwrap().join("osewa");
    assert!(bin.exists(), "{} not built (cargo test builds it for the integration tests)", bin.display());
    bin
}
