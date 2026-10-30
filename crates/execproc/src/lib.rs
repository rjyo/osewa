//! A process without a PTY for exec.start (docs/PROTOCOL.md, Channel
//! streams): its own process group, stdin, stdout and stderr as pipes.

use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// How long Stop waits between SIGHUP and SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// What to run.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub argv: Vec<String>,
    /// Default $HOME.
    pub dir: String,
    /// Added to the environment; later entries win.
    pub env: Vec<String>,
}

#[derive(Debug)]
pub enum Error {
    /// argv[0] doesn't resolve to a program.
    NotFound(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(m) => write!(f, "execproc: program not found: {m}"),
            Error::Other(m) => write!(f, "execproc: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// A started process.
pub struct Proc {
    child: Child,
    pid: i32,
    stdin: Option<PipeWriter>,
    stdout: Option<PipeReader>,
    stderr: Option<PipeReader>,
    closed: Arc<watch::Sender<bool>>,
    stopped: Arc<AtomicBool>,
    status: Option<(i32, bool)>,
}

impl Proc {
    /// Starts the process. Must run inside a tokio runtime.
    pub fn start(o: Options) -> Result<Proc, Error> {
        if o.argv.is_empty() {
            return Err(Error::Other("empty argv".into()));
        }
        let dir = if o.dir.is_empty() { std::env::var("HOME").unwrap_or_default() } else { o.dir.clone() };
        // A missing directory fails the spawn with ENOENT too; check it
        // first so that only a missing program is NotFound.
        if !dir.is_empty() {
            match std::fs::metadata(&dir) {
                Err(e) => return Err(Error::Other(format!("dir: stat {dir}: {}", go_error(&e)))),
                Ok(m) if !m.is_dir() => return Err(Error::Other(format!("dir {dir}: not a directory"))),
                Ok(_) => {}
            }
        }
        let env: Vec<(String, String)> = o
            .env
            .iter()
            .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect();
        let name = &o.argv[0];
        let prog: PathBuf = if name.contains('/') { name.into() } else { look_path(name, &env)? };
        let mut cmd = Command::new(&prog);
        cmd.arg0(name)
            .args(&o.argv[1..])
            .envs(env)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !dir.is_empty() {
            cmd.current_dir(&dir);
        }
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Error::NotFound(format!("fork/exec {}: {}", prog.display(), go_error(&e))),
            _ => Error::Other(format!("fork/exec {}: {}", prog.display(), go_error(&e))),
        })?;
        let pid = child.id().map_or(0, |p| p as i32);
        let (closed, _) = watch::channel(false);
        let closed = Arc::new(closed);
        // Taken now: tokio closes stdin when waiting on a child that still
        // owns it, and output still in a pipe after the exit stays readable
        // because the child no longer owns stdout and stderr either.
        let stdin = child.stdin.take().map(|w| PipeWriter { inner: Some(Box::new(w)), closed: closed.subscribe() });
        let stdout = child.stdout.take().map(|r| PipeReader { inner: Some(Box::new(r)), closed: closed.subscribe() });
        let stderr = child.stderr.take().map(|r| PipeReader { inner: Some(Box::new(r)), closed: closed.subscribe() });
        Ok(Proc { child, pid, stdin, stdout, stderr, closed, stopped: Arc::new(AtomicBool::new(false)), status: None })
    }

    /// The process id, which is also its process group id.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn take_stdin(&mut self) -> Option<PipeWriter> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<PipeReader> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<PipeReader> {
        self.stderr.take()
    }

    /// Waits for the process to exit and returns its exit code, 128+signal
    /// when a signal ended it, and whether one did. It may be called more
    /// than once. Output not yet read stays readable after it.
    pub async fn wait(&mut self) -> (i32, bool) {
        if let Some(s) = self.status {
            return s;
        }
        let s = match self.child.wait().await {
            Ok(st) => match st.signal() {
                Some(sig) => (128 + sig, true),
                None => (st.code().unwrap_or(-1), false),
            },
            Err(_) => (-1, false),
        };
        self.status = Some(s);
        s
    }

    /// A handle that stops the process from anywhere.
    pub fn stopper(&self) -> Stopper {
        Stopper { pgid: self.pid, once: self.stopped.clone() }
    }

    /// Stops the process with the default grace.
    pub fn stop(&self) {
        self.stopper().stop(KILL_GRACE)
    }

    /// A handle that releases the pipes from anywhere.
    pub fn closer(&self) -> Closer {
        Closer(self.closed.clone())
    }

    /// Releases the pipes: stdin, and stdout and stderr if not read to EOF,
    /// unblocking a reader waiting on them. Use it when abandoning a process
    /// whose output will not be read, after stop.
    pub fn close(&mut self) {
        self.closer().close();
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
    }
}

/// Signals a process group.
#[derive(Clone)]
pub struct Stopper {
    pgid: i32,
    once: Arc<AtomicBool>,
}

impl Stopper {
    /// Sends SIGHUP to the process group, and SIGKILL grace later if any of
    /// the group is still alive: a leader that exits on SIGHUP can leave
    /// children that ignore it. Only the first call does anything, and it
    /// doesn't wait.
    pub fn stop(&self, grace: Duration) {
        if self.pgid <= 0 || self.once.swap(true, Ordering::SeqCst) {
            return;
        }
        let pgid = self.pgid;
        unsafe {
            libc::kill(-pgid, libc::SIGHUP);
            // A stopped process doesn't act on SIGHUP until continued.
            libc::kill(-pgid, libc::SIGCONT);
        }
        // A thread rather than a task: the SIGKILL must not depend on a
        // runtime that may be shutting down.
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            // The group id can't be reused while any member, a zombie
            // leader included, exists, so this signal can't reach a
            // stranger unless the whole group is gone.
            if group_alive(pgid) {
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
        });
    }
}

/// Words an I/O error as Go does: a system error as Go's errno text (the C
/// library's, first letter lowercased), anything else as it is. Messages a
/// client shows read the same from either server.
pub fn go_error(e: &io::Error) -> String {
    let Some(n) = e.raw_os_error() else { return e.to_string() };
    let s = unsafe { std::ffi::CStr::from_ptr(libc::strerror(n)) }.to_string_lossy().into_owned();
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => s,
    }
}

/// Reports whether process group pgid has any member, zombies included.
pub fn group_alive(pgid: i32) -> bool {
    pgid > 0 && unsafe { libc::kill(-pgid, 0) } == 0
}

/// Releases a process's pipes.
#[derive(Clone)]
pub struct Closer(Arc<watch::Sender<bool>>);

impl Closer {
    pub fn close(&self) {
        self.0.send_replace(true);
    }
}

async fn closed(rx: &mut watch::Receiver<bool>) {
    // A dropped Proc releases nothing: wait forever then.
    if rx.wait_for(|c| *c).await.is_err() {
        std::future::pending::<()>().await
    }
}

/// Reads stdout or stderr. It closes its pipe at EOF, so a caller that reads
/// to the end leaks nothing, and reads EOF once the Proc is closed.
pub struct PipeReader {
    inner: Option<Box<dyn AsyncRead + Unpin + Send>>,
    closed: watch::Receiver<bool>,
}

impl PipeReader {
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(inner) = self.inner.as_mut() else { return Ok(0) };
        if *self.closed.borrow() {
            self.inner = None;
            return Ok(0);
        }
        let r = tokio::select! {
            r = inner.read(buf) => r,
            _ = closed(&mut self.closed) => Ok(0),
        };
        if matches!(r, Ok(0)) {
            self.inner = None;
        }
        r
    }

    /// Reads to EOF.
    pub async fn read_to_end(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 32 << 10];
        loop {
            match self.read(&mut buf).await? {
                0 => return Ok(out),
                n => out.extend_from_slice(&buf[..n]),
            }
        }
    }
}

/// Writes stdin. Writes fail once the Proc is closed.
pub struct PipeWriter {
    inner: Option<Box<dyn AsyncWrite + Unpin + Send>>,
    closed: watch::Receiver<bool>,
}

impl PipeWriter {
    pub async fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        let gone = || io::Error::new(io::ErrorKind::BrokenPipe, "execproc: stdin closed");
        if *self.closed.borrow() {
            self.inner = None;
        }
        let Some(inner) = self.inner.as_mut() else { return Err(gone()) };
        let r = tokio::select! {
            r = inner.write_all(b) => r,
            _ = closed(&mut self.closed) => Err(gone()),
        };
        if r.is_err() {
            self.inner = None;
        }
        r
    }

    /// Closes stdin: the process reads EOF.
    pub fn close(&mut self) {
        self.inner = None;
    }
}

/// Finds file in the PATH of env, the environment the process will get on
/// top of ours, so Options.env can choose which program runs. The last PATH
/// wins, as it does in the child. Relative entries are skipped: they would
/// resolve against the server's cwd here but the process's dir in the child.
fn look_path(file: &str, env: &[(String, String)]) -> Result<PathBuf, Error> {
    let path = env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    for d in path.split(':') {
        if !Path::new(d).is_absolute() {
            continue;
        }
        let p = Path::new(d).join(file);
        if let Ok(m) = std::fs::metadata(&p) {
            if !m.is_dir() && m.permissions().mode() & 0o111 != 0 {
                return Ok(p);
            }
        }
    }
    Err(Error::NotFound(format!("{file} not in PATH {path:?}")))
}

#[cfg(test)]
mod tests;
