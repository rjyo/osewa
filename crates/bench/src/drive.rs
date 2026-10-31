//! `osewa-bench drive`: runs a terminal client (mosh-client,
//! osewa attach) in a PTY, renders its output with libghostty like a real
//! terminal, and measures what a user sees: keystroke-to-glyph latency, or
//! screen age.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use osewa_ghostty::{self as ghostty, Callbacks, Terminal};

use crate::stamp::{newest_stamp, unix_ms};
use crate::{json_f64s, Error};

/// The display's terminal. Its one callback sends on a channel, libghostty
/// keeps no thread affinity, and every use is under the display's lock.
struct Term(Terminal);
unsafe impl Send for Term {}

struct Screen {
    term: Term,
    bytes: usize,
    /// Bumped on every update.
    gen: u64,
    done: bool,
}

/// A terminal client's PTY rendered by libghostty, standing in for the
/// user's terminal emulator.
pub struct Display {
    ptm: File,
    child: Child,
    st: Arc<(Mutex<Screen>, Condvar)>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

impl Display {
    pub fn start(argv: &[String], cols: u16, rows: u16) -> io::Result<Display> {
        // A real terminal answers the client's queries. The answers are
        // written from their own thread: writing to the PTY from inside
        // term.write would hold the lock while blocked on a client that
        // isn't reading its input, and every screen read would wait behind
        // it. A full queue drops the answer rather than block the reader.
        let (rtx, rrx) = mpsc::sync_channel::<Vec<u8>>(256);
        let term = Terminal::new(
            cols as usize,
            rows as usize,
            ghostty::Options {
                callbacks: Callbacks {
                    write_pty: Some(Box::new(move |b: &[u8]| {
                        let _ = SyncSender::try_send(&rtx, b.to_vec());
                    })),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
        let (mut m, mut s) = (0, 0);
        let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        cvt(unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) })?;
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        for fd in [m, s] {
            cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) })?;
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave.try_clone()?);
        unsafe {
            cmd.pre_exec(|| {
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);
        let ptm = File::from(master);
        let st = Arc::new((Mutex::new(Screen { term: Term(term), bytes: 0, gen: 0, done: false }), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        {
            let (st, stop, mut r) = (st.clone(), stop.clone(), ptm.try_clone()?);
            threads.push(std::thread::spawn(move || read_loop(&mut r, &st, &stop)));
        }
        {
            let (st, stop, mut w) = (st.clone(), stop.clone(), ptm.try_clone()?);
            threads.push(std::thread::spawn(move || loop {
                match rrx.recv_timeout(Duration::from_millis(50)) {
                    Ok(b) => {
                        let _ = w.write_all(&b);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if stop.load(Ordering::SeqCst) || st.0.lock().unwrap().done {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }));
        }
        Ok(Display { ptm, child, st, stop, threads })
    }

    fn lock(&self) -> MutexGuard<'_, Screen> {
        self.st.0.lock().unwrap()
    }

    /// Writes to the client as typed.
    pub fn send(&self, s: &str) {
        let _ = (&self.ptm).write_all(s.as_bytes());
    }

    /// The visible rows, the cursor row, and the update count.
    pub fn screen(&self) -> (Vec<String>, usize, u64) {
        let s = self.lock();
        let t = &s.term.0;
        let b = t.format(ghostty::Format::Plain, false, false).unwrap_or_default();
        let text = String::from_utf8_lossy(&b);
        let lines: Vec<&str> = text.split('\n').collect();
        let (_, h) = t.size();
        let skip = t.scrollback_rows().min(lines.len());
        let rows = lines[skip..].iter().take(h).map(|l| l.trim_end_matches(' ').to_string()).collect();
        (rows, t.cursor().1, s.gen)
    }

    /// Waits until cond holds on the screen, or timeout.
    pub fn wait_for(&self, timeout: Duration, mut cond: impl FnMut(&[String], usize) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let (rows, cy, gen) = self.screen();
            if cond(&rows, cy) {
                return true;
            }
            let mut s = self.lock();
            while s.gen == gen && !s.done {
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                s = self.st.1.wait_timeout(s, deadline - now).unwrap().0;
            }
            if s.gen == gen && s.done {
                return false;
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    pub fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn read_loop(r: &mut File, st: &(Mutex<Screen>, Condvar), stop: &AtomicBool) {
    let mut buf = vec![0u8; 64 << 10];
    let finish = || {
        st.0.lock().unwrap().done = true;
        st.1.notify_all();
    };
    loop {
        // Poll so close() can stop us while a grandchild still holds the
        // PTY open.
        let mut pfd = libc::pollfd { fd: r.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, 50) };
        if stop.load(Ordering::SeqCst) {
            return finish();
        }
        if n == 0 || (n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted) {
            continue;
        }
        match r.read(&mut buf) {
            Ok(0) | Err(_) => return finish(),
            Ok(n) => {
                let mut s = st.0.lock().unwrap();
                s.term.0.write(&buf[..n]);
                s.bytes += n;
                s.gen += 1;
                drop(s);
                st.1.notify_all();
            }
        }
    }
}

fn cursor_line(rows: &[String], cy: usize) -> &str {
    rows.get(cy).map(String::as_str).unwrap_or("")
}

pub fn drive_main(args: &[String], w: &mut dyn Write) -> Result<(), Error> {
    let mut fs = osewa_flag::FlagSet::new("drive");
    fs.value("mode", "type", "type: keystroke-to-glyph latency; watch: screen age")
        .value("cols", "80", "terminal width")
        .value("rows", "24", "terminal height")
        .value("ready", "$", "wait for this on screen before starting")
        .value("setup", "", "text to type after ready, e.g. \"cat\\r\"")
        .value("settle", "1s", "pause after setup")
        .value("keys", "200", "type: keystrokes to measure")
        .value("interval", "100ms", "type: pause between keystrokes")
        .value("line", "30", "type: press Enter after this many characters")
        .value("timeout", "5s", "type: give up on one glyph after this")
        .value("duration", "10s", "watch: how long to sample")
        .value("every", "20ms", "watch: sampling period");
    let p = crate::parse(&fs, args)?;
    let (cols, rows, keys, line) = (crate::int(&p, "cols")?, crate::int(&p, "rows")?, crate::int(&p, "keys")?, crate::int(&p, "line")?);
    let (settle, interval, timeout) = (crate::duration(&p, "settle")?, crate::duration(&p, "interval")?, crate::duration(&p, "timeout")?);
    let (dur, every) = (crate::duration(&p, "duration")?, crate::duration(&p, "every")?);
    if p.args.is_empty() {
        return Err(Error("drive: no command".into()));
    }
    let d = Display::start(&p.args, cols as u16, rows as u16).map_err(|e| Error(crate::exec_error_text(&p.args[0], &e)))?;
    let res = drive(&d, &p, settle, keys, line, interval, timeout, dur, every);
    d.close();
    let out = res?;
    w.write_all(out.as_bytes()).map_err(Error::io)
}

#[allow(clippy::too_many_arguments)]
fn drive(
    d: &Display,
    p: &osewa_flag::Parsed,
    settle: Duration,
    keys: i64,
    line: i64,
    interval: Duration,
    timeout: Duration,
    dur: Duration,
    every: Duration,
) -> Result<String, Error> {
    let ready = p.str("ready");
    if !d.wait_for(Duration::from_secs(30), |r, _| r.join("\n").contains(&ready)) {
        let (rs, _, _) = d.screen();
        return Err(Error(format!("drive: {} never appeared:\n{}", osewa_wire::quote(&ready), rs.join("\n"))));
    }
    let setup = p.str("setup");
    if !setup.is_empty() {
        d.send(&setup.replace("\\r", "\r"));
    }
    std::thread::sleep(settle);
    match p.str("mode").as_str() {
        "type" => Ok(type_keys(d, keys.max(0) as usize, line.max(1) as usize, interval, timeout)),
        "watch" => Ok(watch(d, dur, every)),
        m => Err(Error(format!("drive: unknown mode {}", osewa_wire::quote(m)))),
    }
}

/// Types letters into an echoing program and times each until the glyph is
/// on the cursor's line: predicted or echoed, whichever is first. Returns
/// the JSON line.
fn type_keys(d: &Display, n: usize, line_len: usize, interval: Duration, timeout: Duration) -> String {
    let (mut lat, mut timeouts) = (Vec::new(), 0);
    let mut line = String::new();
    for k in 0..n {
        if k > 0 && k % line_len == 0 {
            d.send("\r");
            line.clear();
            d.wait_for(timeout, |rs, cy| cursor_line(rs, cy).is_empty());
            std::thread::sleep(interval);
        }
        let c = (b'a' + (k % 26) as u8) as char;
        line.push(c);
        let start = Instant::now();
        d.send(&c.to_string());
        if d.wait_for(timeout, |rs, cy| cursor_line(rs, cy).ends_with(line.as_str())) {
            lat.push(start.elapsed().as_micros() as f64 / 1000.0);
        } else {
            timeouts += 1;
        }
        std::thread::sleep(interval);
    }
    format!("{{\"latencies_ms\":{},\"timeouts\":{timeouts}}}\n", json_f64s(&lat))
}

/// Samples how old the newest stamp on screen is.
fn watch(d: &Display, dur: Duration, every: Duration) -> String {
    let mut samples = Vec::new();
    let end = Instant::now() + dur;
    while Instant::now() < end {
        let now = SystemTime::now();
        let (rs, _, _) = d.screen();
        let age = match newest_stamp(&rs.join("\n")) {
            Some(s) => match now.duration_since(s) {
                Ok(x) => x.as_millis() as i64,
                Err(e) => -(e.duration().as_millis() as i64),
            },
            None => -1,
        };
        samples.push(format!("{{\"at_ms\":{},\"age_ms\":{age}}}", unix_ms(now)));
        std::thread::sleep(every);
    }
    let samples = if samples.is_empty() { "null".to_string() } else { format!("[{}]", samples.join(",")) };
    format!("{{\"samples\":{samples},\"bytes\":{}}}\n", d.bytes())
}
