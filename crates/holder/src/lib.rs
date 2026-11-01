//! One osewa session: the PTY, the child, the authoritative libghostty-vt
//! terminal, the output log, input, the owner and the exit status, served
//! over a Unix socket (docs/PROTOCOL.md, Processes).
//!
//! Everything runs on one thread: the PTY, the socket's connections and the
//! timers are tasks of a current-thread runtime sharing the session through
//! a RefCell, never borrowed across an await.

use std::cell::RefCell;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use osewa_wire as wire;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

mod input;
mod log;
mod pty;
mod queue;
mod session;
mod trace;

use queue::{Clock, SendQueue};
use session::{Conn, Session};

/// Defaults from docs/PROTOCOL.md.
pub const DEFAULT_LOG_BYTES: usize = 8 << 20;
pub const DEFAULT_LAG_BYTES: usize = 4 << 20;
pub const DEFAULT_EXIT_RETENTION: Duration = Duration::from_secs(24 * 3600);
pub const DEFAULT_OWNER_GRACE: Duration = Duration::from_secs(30);
pub const DEFAULT_CATCH_UP_DELAY: Duration = Duration::from_millis(200);
pub const DEFAULT_IDLE: Duration = Duration::from_secs(24 * 3600);
/// Ghostty the app's default. libghostty's own is far smaller, around 850
/// rows at 80 columns.
pub const DEFAULT_SCROLLBACK_BYTES: usize = 10 << 20;

/// Bounds one PTY read.
const READ_SIZE: usize = 32 << 10;
/// Bounds one output record, the smallest OUTPUT frame.
pub(crate) const MAX_RECORD_DATA: usize = queue::MIN_FRAME_DATA as usize;
/// The connection writer's buffer: frames collect here while the socket
/// takes the previous write.
const WRITE_BUFFER: usize = 64 << 10;

/// Timings that tests shorten.
#[derive(Clone, Debug)]
pub struct Tunables {
    /// How long after input reaches the PTY an input-triggered screen waits
    /// before it is cut at the next read. Under a flood the next read is
    /// output already buffered, from before the program drew its echo; a
    /// program that echoes within this of reading its input is in the
    /// screen. A tty's own echo is immediate either way.
    pub echo_settle: Duration,
    /// While the owner has typed within typing_window, the flood screen
    /// after a completed screen waits up to typing_hold, so the next
    /// keystroke's screen finds the link clear instead of queueing behind
    /// it.
    pub typing_window: Duration,
    pub typing_hold: Duration,
    /// How long after echo_settle the screen is cut without a read.
    pub echo_quiet: Duration,
    /// How often an attached session rereads the PTY's termios; every PTY
    /// read also triggers one.
    pub mode_poll: Duration,
    /// How long session.kill waits between SIGHUP and SIGKILL.
    pub kill_grace: Duration,
    /// Bounds how long exit waits for the PTY's last output when a
    /// background process keeps the PTY open.
    pub drain_wait: Duration,
    /// Bounds how long shutdown lets clients drain their queues.
    pub flush_wait: Duration,
    /// Makes every full snapshot's encode fail, for tests; screens still
    /// encode.
    pub fail_encode: bool,
}

impl Default for Tunables {
    fn default() -> Tunables {
        Tunables {
            echo_settle: Duration::from_millis(20),
            typing_window: Duration::from_secs(1),
            typing_hold: Duration::from_millis(200),
            echo_quiet: Duration::from_millis(10),
            mode_poll: Duration::from_millis(250),
            kill_grace: Duration::from_secs(2),
            drain_wait: Duration::from_secs(1),
            flush_wait: Duration::from_millis(500),
            fail_encode: false,
        }
    }
}

/// One session.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub run_dir: PathBuf,
    pub id: String,
    pub cols: i64,
    pub rows: i64,
    /// The text area in pixels, 0 when unknown.
    pub width: i64,
    pub height: i64,
    /// Required.
    pub argv: Vec<String>,
    pub cwd: String,
    /// KEY=value entries over the session's environment, which is the
    /// account's (osewa_paths::session_env), not the holder's; TERM
    /// defaults to xterm-256color. The CLI reads them from stdin with
    /// --env-stdin.
    pub env: Vec<String>,
    /// The session's home in place of the account's (OSEWA_HOME, which
    /// the CLI reads at start).
    pub home: Option<PathBuf>,
    /// Ring cap, default 8 MiB.
    pub log_bytes: usize,
    /// Per-client unsent threshold, default 4 MiB.
    pub lag_bytes: usize,
    /// Puts a stream into catch-up once its oldest unsent record has waited
    /// this long, and takes it out after it has kept up this long.
    pub catch_up_delay: Duration,
    /// Ends the session, as if killed, after this long with no attached
    /// stream and no PTY output. Zero disables it; main defaults it to 24h.
    pub idle: Duration,
    pub scrollback_bytes: usize,
    /// How long to linger after the child exits.
    pub exit_retention: Duration,
    pub owner_grace: Duration,
    /// If set, receives OSEWA_TRACE events in holder-<pid>.jsonl; main takes
    /// it from $OSEWA_TRACE.
    pub trace_dir: String,
    pub tune: Tunables,
}

/// Where the holder for session id listens.
pub fn socket_path(run_dir: &Path, id: &str) -> PathBuf {
    run_dir.join(format!("{id}.sock"))
}

/// Bounds terminal sizes: a client must not make the holder allocate an
/// enormous terminal.
pub(crate) fn valid_size(cols: i64, rows: i64) -> bool {
    cols > 0 && rows > 0 && cols <= wire::MAX_COLS && rows <= wire::MAX_ROWS
}

/// A pixel size as given, or 0 (unknown) when out of range: it only places
/// images, so a bad one is no reason to refuse a size.
pub(crate) fn pixels(v: i64) -> i64 {
    if (0..=wire::MAX_PIXELS).contains(&v) {
        v
    } else {
        0
    }
}

/// One cell's pixel size for a text area of width x height pixels.
pub(crate) fn cell_px(cols: i64, rows: i64, width: i64, height: i64) -> (u32, u32) {
    ((width / cols.max(1)) as u32, (height / rows.max(1)) as u32)
}

impl Config {
    fn defaults(&mut self) -> Result<(), String> {
        if self.id.is_empty() || self.id.contains(['/', '\0']) || self.id == "." || self.id == ".." {
            return Err(format!("holder: bad session id {:?}", self.id));
        }
        if self.run_dir.as_os_str().is_empty() {
            return Err("holder: no run dir".into());
        }
        if self.argv.is_empty() {
            return Err("holder: no argv".into());
        }
        if !valid_size(self.cols, self.rows) {
            return Err(format!("holder: bad size {}x{}", self.cols, self.rows));
        }
        if self.log_bytes == 0 {
            self.log_bytes = DEFAULT_LOG_BYTES;
        }
        if self.lag_bytes == 0 {
            self.lag_bytes = DEFAULT_LAG_BYTES;
        }
        if self.catch_up_delay.is_zero() {
            self.catch_up_delay = DEFAULT_CATCH_UP_DELAY;
        }
        if self.exit_retention.is_zero() {
            self.exit_retention = DEFAULT_EXIT_RETENTION;
        }
        if self.scrollback_bytes == 0 {
            self.scrollback_bytes = DEFAULT_SCROLLBACK_BYTES;
        }
        if self.owner_grace.is_zero() {
            self.owner_grace = DEFAULT_OWNER_GRACE;
        }
        Ok(())
    }
}

/// Runs `osewa holder --run <dir> --id <id> --cols <c> --rows <r> [--cwd
/// <dir>] [--env-stdin] [--scrollback-bytes <n>] [--idle-timeout <d>] --
/// argv...`; args are
/// the arguments after "holder". Prints "ready" once the socket accepts
/// connections and returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    let cfg = match parse_flags(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if let Err(e) = osewa_paths::check_home(cfg.home.as_deref()) {
        eprintln!("osewa holder: {e}");
        return 2;
    }
    if let Some(h) = &cfg.home {
        eprintln!("osewa holder: {}={}: the session's home instead of the account's", osewa_paths::HOME_OVERRIDE, h.display());
    }
    let ready = || {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "ready");
        let _ = out.flush();
    };
    match run(cfg, ready, true) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("osewa holder: {e}");
            1
        }
    }
}

/// Bounds the session env --env-stdin reads: a REQUEST frame's.
const ENV_STDIN_MAX: u64 = wire::MAX_FRAME as u64;

/// The client's session env, a JSON array of KEY=value, from stdin to EOF:
/// not on the command line, which every user on the host can read, nor in
/// one environment string, which Linux caps at 128 KiB.
fn env_from_stdin() -> Result<Vec<String>, String> {
    use std::io::Read;
    let mut b = Vec::new();
    io::stdin().take(ENV_STDIN_MAX + 1).read_to_end(&mut b).map_err(|e| format!("holder: --env-stdin: {e}"))?;
    if b.len() as u64 > ENV_STDIN_MAX {
        return Err(format!("holder: --env-stdin: more than {ENV_STDIN_MAX} bytes"));
    }
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&b).map_err(|e| format!("holder: --env-stdin: {e}"))
}

fn parse_flags(args: &[String]) -> Result<Config, String> {
    let mut fs = osewa_flag::FlagSet::new("holder");
    fs.value("run", "", "run directory for the socket")
        .value("id", "", "session id")
        .value("cols", "80", "columns")
        .value("rows", "24", "rows")
        .value("width", "0", "width in pixels, 0 if unknown")
        .value("height", "0", "height in pixels, 0 if unknown")
        .value("cwd", "", "working directory")
        .bool("env-stdin", false, "read the session env from stdin: a JSON array of KEY=value")
        .value("scrollback-bytes", &DEFAULT_SCROLLBACK_BYTES.to_string(), "scrollback cap in bytes")
        .value("idle-timeout", "24h0m0s", "end the session after this long unattached and silent; 0 disables");
    let p = fs.parse(args).map_err(|e| format!("{e}\n{}", fs.usage()))?;
    let int = |n: &str| p.int(n).map_err(|e| e.to_string());
    Ok(Config {
        run_dir: p.str("run").into(),
        id: p.str("id"),
        cols: int("cols")?,
        rows: int("rows")?,
        width: pixels(int("width")?),
        height: pixels(int("height")?),
        cwd: p.str("cwd"),
        scrollback_bytes: int("scrollback-bytes")?.max(0) as usize,
        // A negative duration is accepted and disables the timeout as zero
        // does: v0 holders take one.
        idle: match p.str("idle-timeout").strip_prefix('-') {
            Some(neg) => osewa_flag::parse_duration(neg).map(|_| Duration::ZERO).map_err(|e| e.to_string())?,
            None => p.duration("idle-timeout").map_err(|e| e.to_string())?,
        },
        argv: p.args.clone(),
        env: if p.bool("env-stdin") { env_from_stdin()? } else { Vec::new() },
        home: osewa_paths::home_override().map_err(|e| format!("holder: {e}"))?,
        trace_dir: std::env::var("OSEWA_TRACE").unwrap_or_default(),
        ..Default::default()
    })
}

/// Serves the session on its own current-thread runtime until the child has
/// exited and the exit retention has passed, or (with signals) SIGTERM or
/// SIGINT stops it.
pub fn run(cfg: Config, ready: impl FnOnce(), signals: bool) -> Result<(), String> {
    run_until(cfg, ready, signals, None)
}

/// run, also stopped (as by SIGTERM) when stop fires.
pub fn run_until(
    cfg: Config,
    ready: impl FnOnce(),
    signals: bool,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), String> {
    osewa_paths::check_home(cfg.home.as_deref()).map_err(|e| format!("holder: {e}"))?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, serve(cfg, ready, signals, stop))
}

fn real_clock() -> Clock {
    Rc::new(Instant::now)
}

/// Serves the session; stop, when given, ends it as SIGTERM does.
pub async fn serve(
    mut cfg: Config,
    ready: impl FnOnce(),
    signals: bool,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), String> {
    cfg.defaults()?;
    osewa_paths::ensure(&cfg.run_dir).map_err(|e| e.to_string())?;
    let sock = socket_path(&cfg.run_dir, &cfg.id);
    clear_stale(&sock).await?;
    let ln = UnixListener::bind(&sock).map_err(|e| e.to_string())?;
    let _remove = RemoveOnDrop(sock.clone());
    std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o600)).map_err(|e| e.to_string())?;

    let tune = cfg.tune.clone();
    let (master, slave) = pty::open(cfg.cols as u16, cfg.rows as u16, cfg.width as u16, cfg.height as u16).map_err(|e| e.to_string())?;
    let acct = osewa_paths::account(cfg.home.as_deref());
    let mut cmd = pty::command(&cfg.argv, &cfg.cwd, &cfg.env, &acct, &slave).map_err(|e| e.to_string())?;
    let mode_poll = if cfg.idle.is_zero() { tune.mode_poll } else { tune.mode_poll.min((cfg.idle / 4).max(Duration::from_millis(10))) };
    let s = Session::new(cfg, master.as_raw_fd(), real_clock())?;
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    drop(slave);
    drop(cmd);
    s.borrow_mut().core.pid = child.id().map_or(0, |p| p as i32);
    // Sound: the AsyncFd owns the master, and nothing else closes it.
    let master = Rc::new(unsafe { AsyncFd::register(master) }.map_err(|e| format!("{e:?}"))?);

    // The PTY's output.
    let reader_done = Rc::new(Notify::new());
    let reader_flag = Rc::new(std::cell::Cell::new(false));
    {
        let (s, master, done, flag) = (Rc::downgrade(&s), master.clone(), reader_done.clone(), reader_flag.clone());
        tokio::task::spawn_local(async move {
            read_pty(&s, &master).await;
            flag.set(true);
            done.notify_one();
        });
    }
    // The PTY's input.
    {
        let (weak, master) = (Rc::downgrade(&s), master.clone());
        let (input, wake) = {
            let b = s.borrow();
            (b.core.input.clone(), b.core.input_wake.clone())
        };
        tokio::task::spawn_local(async move { write_pty(weak, master, input, wake).await });
    }
    // The child's exit, once the PTY has given up its last output.
    {
        let weak = Rc::downgrade(&s);
        let reaped = s.borrow().core.reaped.clone();
        tokio::task::spawn_local(async move {
            let code = match child.wait().await {
                Ok(st) => pty::exit_code(st),
                Err(_) => -1,
            };
            reaped.set(true);
            if !reader_flag.get() {
                tokio::select! {
                    _ = reader_done.notified() => {}
                    _ = tokio::time::sleep(tune.drain_wait) => {}
                }
            }
            if let Some(s) = weak.upgrade() {
                let mut s = s.borrow_mut();
                if !s.core.done {
                    s.handle_exit(code);
                }
            }
        });
    }
    // The periodic tick.
    {
        let weak = Rc::downgrade(&s);
        tokio::task::spawn_local(async move {
            let mut iv = tokio::time::interval(mode_poll);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            iv.tick().await;
            loop {
                iv.tick().await;
                let Some(s) = weak.upgrade() else { return };
                let mut s = s.borrow_mut();
                if s.core.done {
                    return;
                }
                s.tick();
            }
        });
    }
    // Connections. The task owns the listener: aborting it closes the
    // socket, so a dial during shutdown is refused (unavailable), as when
    // no holder is there.
    let acceptor = {
        let weak = Rc::downgrade(&s);
        tokio::task::spawn_local(async move {
            let mut next = 0u64;
            while let Ok((stream, _)) = ln.accept().await {
                if !peer_allowed(&stream) {
                    continue;
                }
                next += 1;
                if !accept(&weak, next, stream) {
                    return;
                }
            }
        })
    };
    ready();

    let result = wait_stop(&s, signals, stop).await;
    acceptor.abort();
    let _ = acceptor.await;
    shutdown(&s, &master).await;
    result
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Waits until the exit retention has passed, or a stop request has seen the
/// child's exit through, so streams get STREAM_END with its code.
async fn wait_stop(
    s: &Rc<RefCell<Session>>,
    signals: bool,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let (retention, exited) = {
        let b = s.borrow();
        (b.core.retention.clone(), b.core.exited_notify.clone())
    };
    let tune = s.borrow().core.cfg.tune.clone();
    let mut term = if signals { Some(signal(SignalKind::terminate()).map_err(|e| e.to_string())?) } else { None };
    let mut int = if signals { Some(signal(SignalKind::interrupt()).map_err(|e| e.to_string())?) } else { None };
    // A holder started by hand must outlive its terminal too. Catch SIGHUP
    // rather than ignore it: an ignored signal survives exec, and the child
    // must still die from the SIGHUP that session.kill sends.
    let _hup = if signals { Some(signal(SignalKind::hangup()).map_err(|e| e.to_string())?) } else { None };
    let stopped = async {
        tokio::select! {
            Some(_) = async { term.as_mut()?.recv().await } => {}
            Some(_) = async { int.as_mut()?.recv().await } => {}
            Some(_) = async { stop?.await.ok() } => {}
            else => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = retention.notified() => return Ok(()),
        _ = stopped => {}
    }
    if s.borrow().core.exited {
        return Ok(());
    }
    {
        let mut b = s.borrow_mut();
        b.core.terminate();
        b.finish();
    }
    tokio::select! {
        _ = exited.notified() => {}
        _ = tokio::time::sleep(tune.kill_grace + tune.drain_wait + Duration::from_secs(1)) => {}
    }
    Ok(())
}

async fn shutdown(s: &Rc<RefCell<Session>>, master: &Rc<AsyncFd<OwnedFd>>) {
    let (writers, tune, reaped) = {
        let mut b = s.borrow_mut();
        b.core.done = true;
        let mut writers = Vec::new();
        for c in b.conns.values_mut() {
            c.q.borrow_mut().close();
            c.wake.notify_one();
            if let Some(w) = c.writer.take() {
                writers.push(w);
            }
        }
        (writers, b.core.cfg.tune.clone(), b.core.reaped.clone())
    };
    // Let writers flush what's queued, STREAM_END included, but don't wait
    // on a client that has stopped reading: one deadline for all of them.
    let deadline = tokio::time::Instant::now() + tune.flush_wait;
    for w in writers {
        let abort = w.abort_handle();
        if tokio::time::timeout_at(deadline, w).await.is_err() {
            abort.abort();
        }
    }
    let conns: Vec<Conn> = std::mem::take(&mut s.borrow_mut().conns).into_values().collect();
    for c in conns {
        if let Some(r) = c.reader {
            r.abort();
        }
    }
    let core_pid_signal = |sig| s.borrow().core.signal_group(sig);
    core_pid_signal(libc::SIGHUP);
    let wait_reaped = async {
        while !reaped.get() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    if tokio::time::timeout(tune.kill_grace, wait_reaped).await.is_err() {
        core_pid_signal(libc::SIGKILL);
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while !reaped.get() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }
    s.borrow().core.input.borrow_mut().close();
    let _ = master;
}

/// Removes a socket left by a holder that died, and refuses to replace a
/// live one.
async fn clear_stale(sock: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(sock).is_err() {
        return Ok(());
    }
    if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(sock)).await {
        return Err(format!("holder: {} is already served", sock.display()));
    }
    std::fs::remove_file(sock).map_err(|e| e.to_string())
}

/// Admits only connections from our own uid. The run directory is 0700 as
/// well, but the peer's credentials don't depend on how it was created.
fn peer_allowed(c: &UnixStream) -> bool {
    match c.peer_cred() {
        Ok(cred) => cred.uid() == unsafe { libc::getuid() },
        Err(_) => false,
    }
}

async fn read_pty(s: &Weak<RefCell<Session>>, master: &AsyncFd<OwnedFd>) {
    // A PTY read returns a few KiB at most, and what is kept (the output
    // log) is copied at its size, never this buffer.
    let mut buf = vec![0u8; READ_SIZE];
    loop {
        let Ok(mut guard) = master.readable().await else { return };
        let r = guard.try_io(|fd| {
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match r {
            Ok(Ok(0)) | Ok(Err(_)) => return,
            Ok(Ok(n)) => {
                {
                    let Some(s) = s.upgrade() else { return };
                    let mut s = s.borrow_mut();
                    if s.core.done {
                        return;
                    }
                    s.handle_output(&buf[..n]);
                }
                // Yield after each read: a PTY that
                // stays readable under a flood must not starve a keystroke
                // arriving on a connection, the writers or the echo timers.
                tokio::task::yield_now().await;
            }
            Err(_) => continue,
        }
    }
}

async fn write_pty(s: Weak<RefCell<Session>>, master: Rc<AsyncFd<OwnedFd>>, input: Rc<RefCell<input::PtyInput>>, wake: Rc<Notify>) {
    let mut chunk = Vec::with_capacity(input::WRITE_CHUNK);
    loop {
        {
            let inp = input.borrow();
            if inp.closed {
                return;
            }
            inp.chunk(&mut chunk);
        }
        if chunk.is_empty() {
            wake.notified().await;
            continue;
        }
        let res = loop {
            let Ok(mut guard) = master.writable().await else { break Err(()) };
            match guard.try_io(|fd| {
                let n = unsafe { libc::write(fd.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => break Ok(n),
                Ok(Err(_)) => break Err(()),
                Err(_) => continue,
            }
        };
        let room = match res {
            Ok(n) => input.borrow_mut().wrote(n),
            Err(()) => {
                input.borrow_mut().close();
                true
            }
        };
        if room {
            if let Some(s) = s.upgrade() {
                let mut s = s.borrow_mut();
                if !s.core.done {
                    s.on_room();
                }
            }
        }
    }
}

/// Registers a connection and starts its reader and writer; false once the
/// session has stopped.
fn accept(weak: &Weak<RefCell<Session>>, cid: u64, stream: UnixStream) -> bool {
    let Some(s) = weak.upgrade() else { return false };
    if s.borrow().core.done {
        return false;
    }
    let (rd, wr) = stream.into_split();
    let q = Rc::new(RefCell::new(SendQueue::new(s.borrow().core.clock.clone())));
    let mut c = Conn::new(cid, q.clone());
    let wake = c.wake.clone();
    let writer_done = Rc::new(Notify::new());
    let tr = s.borrow().core.tr.clone();
    let wd = writer_done.clone();
    let w = weak.clone();
    c.writer = Some(tokio::task::spawn_local(async move {
        let on_end = |gen: u32| {
            if let Some(s) = w.upgrade() {
                let mut s = s.borrow_mut();
                if !s.core.done {
                    s.snapshot_done(cid, gen);
                }
            }
        };
        let _ = run_writer(&q, &wake, wr, tr.as_deref(), on_end).await;
        wd.notify_one();
    }));
    let w = weak.clone();
    c.reader = Some(tokio::task::spawn_local(async move {
        let mut rd = BufReader::with_capacity(64 << 10, rd);
        loop {
            tokio::select! {
                f = read_frame(&mut rd) => {
                    let Ok(f) = f else { break };
                    let Some(s) = w.upgrade() else { return };
                    let mut s = s.borrow_mut();
                    if s.core.done {
                        return;
                    }
                    s.handle_frame(cid, f);
                }
                _ = writer_done.notified() => break,
            }
        }
        if let Some(s) = w.upgrade() {
            let mut s = s.borrow_mut();
            if !s.core.done {
                s.drop_conn(cid);
            }
        }
    }));
    s.borrow_mut().conns.insert(cid, c);
    true
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> io::Result<wire::Frame> {
    let mut lb = [0u8; 4];
    r.read_exact(&mut lb).await?;
    let n = u32::from_le_bytes(lb) as usize;
    wire::check_len(n)?;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await?;
    Ok(wire::Frame::unmarshal(&b)?)
}

/// Writes frames as they become sendable, until the queue is closed and
/// nothing more can go, or a write fails.
async fn run_writer<W: tokio::io::AsyncWrite + Unpin>(
    q: &RefCell<SendQueue>,
    wake: &Notify,
    mut w: W,
    tr: Option<&trace::Tracer>,
    on_end: impl Fn(u32),
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(WRITE_BUFFER);
    loop {
        let (sent, closed) = {
            let mut q = q.borrow_mut();
            let before = buf.len();
            let sent = q.next_into(&mut buf);
            // wire::write's bound: an oversized frame (one huge event) ends
            // the connection here, at the sender, rather
            // than at the peer's reader.
            if sent.is_some() && buf.len() - before - 4 > wire::MAX_FRAME {
                return Err(wire::Error::TooLarge.into());
            }
            (sent, q.closed)
        };
        if let Some(f) = sent {
            if let Some(tr) = tr {
                if f.ty == wire::TYPE_SNAPSHOT_BEGIN || f.ty == wire::TYPE_SNAPSHOT_READY {
                    // Flushed, so "written" means it left for the server.
                    w.write_all(&buf).await?;
                    buf.clear();
                    let ev = if f.ty == wire::TYPE_SNAPSHOT_READY { "ready_written" } else { "begin_written" };
                    tr.emit(ev, &[("stream", f.stream.into()), ("generation", f.gen.into())]);
                }
            }
            if f.ty == wire::TYPE_SNAPSHOT_END {
                let due = q.borrow_mut().end_written();
                if let Some(gen) = due {
                    on_end(gen);
                }
            }
            if buf.len() >= WRITE_BUFFER {
                w.write_all(&buf).await?;
                buf.clear();
            }
            continue;
        }
        if !buf.is_empty() {
            w.write_all(&buf).await?;
            buf.clear();
        }
        if closed {
            return Ok(());
        }
        wake.notified().await;
    }
}

#[cfg(test)]
mod tests;
