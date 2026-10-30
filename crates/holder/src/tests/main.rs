//! Ported from main_test.go: the holder as a real process, and the test
//! binary as the PTY child that floods. Go's TestMain re-executes the test
//! binary; here the binary re-executes itself to run child_process alone,
//! which dispatches on the same environment variables.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::helpers::*;
use crate::{socket_path, Config};

/// Not a test of its own: the entry point when a test re-executes this
/// binary, as the holder (OSEWA_TEST_HOLDER=1, its arguments in
/// OSEWA_TEST_ARGS) or as a flood generator. Run normally, it does nothing.
#[test]
fn child_process() {
    if std::env::var("OSEWA_TEST_HOLDER").as_deref() == Ok("1") {
        let args: Vec<String> = std::env::var("OSEWA_TEST_ARGS")
            .unwrap_or_default()
            .split('\x1f')
            .filter(|a| !a.is_empty())
            .map(String::from)
            .collect();
        std::process::exit(crate::main(&args));
    }
    if let Ok(rate) = std::env::var("OSEWA_TEST_FLOOD").unwrap_or_default().parse::<usize>() {
        flood_main(rate);
    }
    if let Ok(v) = std::env::var("OSEWA_TEST_ECHOFLOOD") {
        // rate,ms: flood at rate for ms.
        // [,tail[,cols,rows[,raw]]]: then tail bytes a second, on a cols x
        // rows screen (default 200x60); raw=1 echoes keystrokes as typed.
        let f: Vec<usize> = v.split(',').map(|x| x.parse().unwrap_or(0)).collect();
        let opt = |i: usize, def: usize| f.get(i).copied().unwrap_or(def);
        echo_flood_main(f[0], Duration::from_millis(f[1] as u64), opt(2, 0), opt(3, 200), opt(4, 60), opt(5, 0) == 1);
    }
}

/// The libtest arguments that run child_process alone.
const SELF_ARGS: &str = "--exact tests::main::child_process --nocapture --test-threads=1 -q";

/// A command line running this binary as the PTY child: its stdout (where
/// libtest reports) goes to /dev/null, and the generator writes to fd 3,
/// the PTY.
pub fn self_child(prefix: &str) -> Vec<String> {
    let exe = std::env::current_exe().unwrap();
    sh(&format!("{prefix}exec '{}' {SELF_ARGS} 3>&1 >/dev/null", exe.display()))
}

/// echoFloodArgv: echoFloodMain with the tty's own echo off: the program
/// draws its echo, and a tty echo's newline on the last row would scroll the
/// flood over it.
pub fn echo_flood_argv() -> Vec<String> {
    self_child("stty -echo; ")
}

/// echoFloodRawArgv: echoFloodMain in raw mode.
pub fn echo_flood_raw_argv() -> Vec<String> {
    self_child("stty raw -echo; ")
}

/// The holder process, with args after "holder".
pub fn holder_command(args: &[&str]) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(SELF_ARGS.split(' '));
    cmd.env("OSEWA_TEST_HOLDER", "1").env("OSEWA_TEST_ARGS", args.join("\x1f"));
    cmd
}

/// A running holder process, killed on drop.
pub struct Proc {
    pub child: Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proc {
    pub fn spawn(mut cmd: Command) -> Proc {
        cmd.stdout(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Proc { child, out }
    }

    /// The first line the holder printed, past libtest's own.
    pub fn line(&mut self) -> String {
        loop {
            let mut l = String::new();
            if self.out.read_line(&mut l).unwrap_or(0) == 0 {
                return l;
            }
            if l.trim().is_empty() || l.starts_with("running ") {
                continue;
            }
            return l;
        }
    }

    pub fn signal(&self, sig: libc::c_int) {
        unsafe { libc::kill(self.child.id() as i32, sig) };
    }

    /// Waits for the process to exit within d and returns its code.
    pub fn wait_exit(&mut self, within: Duration) -> i32 {
        let deadline = Instant::now() + within;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "holder still running after {within:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn start_process(dir: &Path, id: &str, argv: &[&str]) -> Proc {
    let d = dir.to_str().unwrap();
    let mut args = vec!["--run", d, "--id", id, "--cols", "40", "--rows", "10", "--"];
    args.extend_from_slice(argv);
    let mut p = Proc::spawn(holder_command(&args));
    let line = p.line();
    assert_eq!(line.trim(), "ready", "holder said {line:?}");
    p
}

/// SIGTERM ends a holder promptly whether or not a session.kill is in
/// flight: the child's group dies, streams get STREAM_END, the socket goes.
#[test]
fn sigterm() {
    for i in 0..6 {
        let dir = TempDir::new();
        let mut p = start_process(&dir.0, "k1", &["/bin/sh", "-c", "echo up; while :; do sleep 0.1; done"]);
        let sock = socket_path(&dir.0, "k1");
        let mut c = Client::dial(&sock, "dev1");
        c.attach(wire::Request { own: true, ..Default::default() });
        c.wait_text("up");
        let kill = || wire::Request { method: wire::METHOD_KILL.into(), ..Default::default() };
        match i % 3 {
            // SIGTERM right after kill
            0 => {
                c.request(kill());
            }
            // kill, then SIGTERM a moment later
            1 => {
                c.request(kill());
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {}
        }
        let start = Instant::now();
        p.signal(libc::SIGTERM);
        c.wait_for("stream end", |c| c.ended.is_some());
        let end = c.ended.clone().unwrap();
        assert!(end.reason == wire::END_KILLED && end.exit_code.is_some(), "run {i}: end = {end:?}");
        assert_eq!(p.wait_exit(Duration::from_secs(2)), 0, "run {i}: exit code");
        let d = start.elapsed();
        assert!(d <= Duration::from_secs(2), "run {i}: exit took {d:?}");
        assert!(!sock.exists(), "run {i}: socket left behind");
        c.close();
    }
}

/// The holder's own SIGHUP handling must not leak into the child as an
/// ignored signal.
#[test]
fn child_gets_default_sighup() {
    let dir = TempDir::new();
    let _p = start_process(&dir.0, "h1", &["/bin/sh", "-c", "echo up; while :; do sleep 0.1; done"]);
    let mut c = Client::dial(&socket_path(&dir.0, "h1"), "dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
    c.wait_text("up");
    let start = Instant::now();
    c.request(wire::Request { method: wire::METHOD_KILL.into(), ..Default::default() });
    c.wait_for("stream end", |c| c.ended.is_some());
    // 129 is death by SIGHUP; SIGKILL after the grace would be 137.
    let code = c.ended.as_ref().unwrap().exit_code;
    let d = start.elapsed();
    assert!(code == Some(129) && d <= crate::Tunables::default().kill_grace, "end = {code:?} after {d:?}");
}

/// Sums the logical rows of a snapshot's PAGE records (tag 3 in libghostty's
/// record.zig; rows is the u16 at payload offset 2 in page.zig), which counts
/// history and active rows alike.
fn snapshot_rows(snap: &[u8]) -> usize {
    // envelope.zig: magic and version, 10 bytes; record.zig: tag u16,
    // len u32, crc u32.
    const TAG_PAGE: u16 = 3;
    const SNAP_ENVELOPE: usize = 10;
    const SNAP_HEADER: usize = 10;
    let mut rows = 0;
    let mut off = SNAP_ENVELOPE;
    while off + SNAP_HEADER <= snap.len() {
        let tag = u16::from_le_bytes(snap[off..off + 2].try_into().unwrap());
        let n = u32::from_le_bytes(snap[off + 2..off + 6].try_into().unwrap()) as usize;
        if tag == TAG_PAGE {
            let p = off + SNAP_HEADER + 2;
            rows += u16::from_le_bytes(snap[p..p + 2].try_into().unwrap()) as usize;
        }
        off += SNAP_HEADER + n;
    }
    rows
}

fn cold_rows(sock: &Path) -> (usize, String) {
    let mut c = Client::dial(sock, "dev1");
    c.attach(wire::Request::default());
    c.wait_for("snapshot end", |c| c.gens.len() == 1 && c.snap_ends.contains_key(&0) && c.term.is_some());
    (snapshot_rows(&c.snap), c.text())
}

/// A cold attach after 2000 lines carries all of them by default.
#[test]
fn scrollback_default() {
    let h = start(Config { cols: 80, rows: 24, argv: sh("seq 1 2000; echo DONE; sleep 30"), ..Default::default() });
    let mut w = h.dial("w");
    w.attach(wire::Request::default());
    w.wait_text("DONE");
    let (rows, text) = cold_rows(&h.sock);
    assert!(rows >= 2001 && text.contains("DONE"), "snapshot has {rows} rows");
}

/// --scrollback-bytes reaches the terminal: a small cap loses early lines.
#[test]
fn scrollback_flag() {
    let dir = TempDir::new();
    let d = dir.0.to_str().unwrap();
    let args = [
        "--run",
        d,
        "--id",
        "sb",
        "--cols",
        "80",
        "--rows",
        "24",
        "--scrollback-bytes",
        "1",
        "--",
        "/bin/sh",
        "-c",
        "seq 1 2000; echo DONE; sleep 30",
    ];
    let mut p = Proc::spawn(holder_command(&args));
    let line = p.line();
    assert_eq!(line, "ready\n", "holder said {line:?}");
    let sock = socket_path(&dir.0, "sb");
    let mut w = Client::dial(&sock, "w");
    w.attach(wire::Request::default());
    w.wait_text("DONE");
    let (rows, _) = cold_rows(&sock);
    assert!(rows < 2000, "scrollback cap ignored: {rows} rows");
}

// The generators (round3_test.go's floodMain, w2Loop and echoFloodMain).

/// Where a generator writes: fd 3, the PTY (see self_child).
static OUT: Mutex<Option<File>> = Mutex::new(None);

/// Writes b whole, so the flood's sequences and an echo never interleave.
/// A holder that has gone ends the generator.
fn echo_write(b: &[u8]) {
    let mut out = OUT.lock().unwrap();
    let f = out.get_or_insert_with(|| unsafe { File::from_raw_fd(3) });
    if f.write_all(b).is_err() {
        std::process::exit(0);
    }
}

/// Runs f every 10 ms, on schedule, forever.
fn every_tick(mut f: impl FnMut()) -> ! {
    let tick = Duration::from_millis(10);
    let mut t = Instant::now();
    loop {
        f();
        t += tick;
        std::thread::sleep(t.saturating_duration_since(Instant::now()));
    }
}

/// The test binary's W2-like generator: cursor moves, SGR and text at a
/// fixed rate, forever. Deterministic (an LCG).
fn flood_main(rate: usize) -> ! {
    let mut w = W2Loop::new(200, 60);
    every_tick(|| echo_write(w.next(rate / 100)))
}

/// Replays a precomputed W2 stream, so a flood costs a write, not
/// formatting: under a loaded machine a generator that formats every update
/// can't keep up its rate, and the tests that need output far above the
/// link rate stop getting it.
pub struct W2Loop {
    pub buf: Vec<u8>,
    /// Offsets where one update ends.
    cuts: Vec<usize>,
    at: usize,
    i: usize,
}

impl W2Loop {
    pub fn new(cols: u32, rows: u32) -> W2Loop {
        let mut r: u32 = 7;
        let mut next = move |n: u32| {
            r = r.wrapping_mul(1103515245).wrapping_add(12345);
            (r >> 8) % n
        };
        let mut w = W2Loop { buf: Vec::new(), cuts: Vec::new(), at: 0, i: 0 };
        while w.buf.len() < 2 << 20 {
            w2_in(&mut w.buf, &mut next, cols, rows);
            w.cuts.push(w.buf.len());
        }
        w
    }

    /// At least n bytes of whole updates.
    pub fn next(&mut self, n: usize) -> &[u8] {
        if n == 0 {
            return &[];
        }
        if self.buf.len() - self.at < n {
            // Wrap rather than come up short.
            (self.at, self.i) = (0, 0);
        }
        let start = self.at;
        while self.cuts[self.i] - start < n {
            self.i += 1;
        }
        self.at = self.cuts[self.i];
        self.i += 1;
        &self.buf[start..self.at]
    }
}

/// Appends one TUI-like update for a cols x rows screen: a cursor move into
/// rows 1 to rows-2, so the last row stays free for an echo line, colors and
/// a word.
fn w2_in(buf: &mut Vec<u8>, next: &mut impl FnMut(u32) -> u32, cols: u32, rows: u32) {
    let (y, x, fg, bg) = (next(rows - 2) + 1, next(cols - 20) + 1, next(256), next(256));
    write!(buf, "\x1b[{y};{x}H\x1b[38;5;{fg}m\x1b[48;5;{bg}m").unwrap();
    for _ in 0..4 + next(16) {
        buf.push(b'a' + next(26) as u8);
    }
}

/// Waits for a "go" line, floods rows 1 to rows-2 at rate for d, then
/// continues at tail bytes a second (0: quiet). Each line read is drawn on
/// the last row as ECHO:<line>, in a write of its own that never splits the
/// flood's: a stand-in for a keystroke's echo during the flood.
fn echo_flood_main(rate: usize, d: Duration, tail: usize, cols: usize, rows: usize, raw: bool) -> ! {
    if raw {
        // Raw mode: the flood starts at once, and every keystroke is drawn
        // as typed ("> " and the line so far; Enter clears it), like
        // osewa-bench gen -echo.
        std::thread::spawn(move || {
            let mut line = Vec::new();
            for b in std::io::stdin().lock().bytes() {
                let Ok(b) = b else { return };
                if b == b'\r' || b == b'\n' {
                    line.clear();
                } else {
                    line.push(b);
                }
                let mut out = format!("\x1b[0m\x1b[{rows};1H\x1b[2K> ").into_bytes();
                out.extend_from_slice(&line);
                echo_write(&out);
            }
        });
    } else {
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        // Each line is drawn the moment it's read, like a program echoing
        // what it got.
        std::thread::spawn(move || {
            let mut lines = std::io::stdin().lock().lines();
            for l in lines.by_ref() {
                if l.map_or(true, |l| l == "go") {
                    break;
                }
            }
            let _ = go_tx.send(());
            for l in lines {
                let Ok(l) = l else { return };
                echo_write(format!("\x1b[0m\x1b[{rows};1H\x1b[2KECHO:{l}").as_bytes());
            }
        });
        let _ = go_rx.recv();
    }
    let mut w = W2Loop::new(cols as u32, rows as u32);
    let end = Instant::now() + d;
    every_tick(|| {
        let per = if Instant::now() < end { rate / 100 } else { tail / 100 };
        let b = w.next(per);
        if !b.is_empty() {
            echo_write(b);
        }
    })
}
