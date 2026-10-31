//! osewa's fixed benchmark workloads. Every workload is a pure function
//! of its size, with fixed bytes, so runs on any build, machine or day feed
//! the terminal the same bytes:
//!
//! - W1, plain ASCII logs: timestamped log lines, printable ASCII and CRLF
//!   only.
//! - W2, TUI-like output: about 40% escape bytes. Mostly styled status lines
//!   (SGR colors, cursor column moves, erase-in-line), and every 200 lines a
//!   full-screen redraw on the alternate screen inside a synchronized-output
//!   bracket (mode 2026), the way TUIs repaint.
//! - W3, CJK and emoji: wide characters, ZWJ sequences, skin tones, flags
//!   and combining marks mixed with ASCII.
//! - W4, real full-screen programs (w4.rs).
//!
//! The pseudo-random choices come from a fixed-seed xorshift generator.

use std::fmt::Write as _;

/// xorshift64*: tiny, deterministic and stable.
#[derive(Clone, Copy)]
pub(crate) struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(2685821657736338717)
    }

    pub fn intn(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

pub(crate) const WORDS: [&str; 34] = [
    "request", "handled", "cache", "miss", "upstream", "retry", "timeout", "connected", "session", "holder", "snapshot",
    "output", "credit", "window", "stream", "attach", "resume", "epoch", "position", "latency", "bytes", "frame", "decode",
    "encode", "worker", "queue", "flush", "commit", "ready", "finish", "client", "server", "device", "enroll",
];

fn word(r: &mut Rng) -> &'static str {
    WORDS[r.intn(WORDS.len())]
}

/// About `lines` lines of plain ASCII log output.
pub fn w1(lines: usize) -> Vec<u8> {
    let mut r = Rng(1);
    let mut b = String::new();
    for i in 0..lines {
        let (a, c) = (i / 60 % 60, i % 60);
        let (ms, w) = (r.intn(1000), r.intn(16));
        let _ = write!(b, "2026-10-28T10:{a:02}:{c:02}.{ms:03}Z INFO  [w{w:02}] ");
        let mut n = 4 + r.intn(10);
        while n > 0 {
            b.push_str(word(&mut r));
            b.push(' ');
            n -= 1;
        }
        let (id, dur) = (r.next() % 1_000_000_000, r.intn(5000));
        let _ = write!(b, "id={id} dur={dur}ms\r\n");
    }
    b.into_bytes()
}

/// How many lines W2 writes between alternate-screen redraws.
pub const W2_REDRAW: usize = 200;

const SGR: [&str; 11] = [
    "\x1b[1m",
    "\x1b[2m",
    "\x1b[31m",
    "\x1b[32m",
    "\x1b[33m",
    "\x1b[34m",
    "\x1b[38;5;208m",
    "\x1b[38;2;120;200;255m",
    "\x1b[48;5;236m",
    "\x1b[4m",
    "\x1b[7m",
];

fn sgr(r: &mut Rng) -> &'static str {
    SGR[r.intn(SGR.len())]
}

/// TUI-like output that leaves `lines` lines on the primary screen, for a
/// cols x rows terminal.
pub fn w2(lines: usize, cols: usize, rows: usize) -> Vec<u8> {
    let mut r = Rng(2);
    let mut b = String::new();
    for i in 0..lines {
        if i > 0 && i % W2_REDRAW == 0 {
            w2_redraw(&mut b, &mut r, cols, rows);
        }
        w2_line(&mut b, &mut r, i, cols);
    }
    b.into_bytes()
}

/// One styled status line: label, colored fields placed with cursor column
/// moves, then erase-to-end.
fn w2_line(b: &mut String, r: &mut Rng, i: usize, cols: usize) {
    let s = sgr(r);
    let _ = write!(b, "\x1b[2K{s}{i:5}\x1b[0m ");
    let mut col = 8i64;
    while col < cols as i64 - 20 {
        let w = word(r);
        if r.intn(3) == 0 {
            b.push_str(sgr(r));
            b.push_str(w);
            b.push_str("\x1b[0m");
        } else {
            b.push_str(w);
        }
        col += w.len() as i64 + 1;
        if r.intn(4) == 0 {
            col += 1 + r.intn(6) as i64;
            let _ = write!(b, "\x1b[{col}G");
        } else {
            b.push(' ');
        }
    }
    b.push_str("\x1b[K\r\n");
}

/// Paints a full alternate screen inside synchronized output, then returns
/// to the primary screen.
fn w2_redraw(b: &mut String, r: &mut Rng, cols: usize, rows: usize) {
    b.push_str("\x1b[?2026h\x1b[?1049h\x1b[H\x1b[2J");
    for y in 1..=rows {
        let s = sgr(r);
        let _ = write!(b, "\x1b[{y};1H{s}");
        for _ in 0..cols / 10 {
            b.push_str(word(r));
            b.push_str("  ");
        }
        b.push_str("\x1b[0m");
    }
    b.push_str("\x1b[?1049l\x1b[?2026l");
}

const WIDE: [&str; 17] = [
    "\u{65e5}\u{672c}\u{8a9e}",
    "\u{30c6}\u{30ad}\u{30b9}\u{30c8}",
    "\u{4e2d}\u{6587}\u{5b57}\u{7b26}",
    "\u{d55c}\u{ad6d}\u{c5b4}",
    "\u{6f22}\u{5b57}",
    "\u{6771}\u{4eac}\u{90fd}",
    "\u{3042}\u{308a}\u{304c}\u{3068}\u{3046}",
    "\u{1f44d}",
    "\u{1f44d}\u{1f3fd}",
    "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
    "\u{1f1ef}\u{1f1f5}",
    "\u{1f389}",
    "e\u{301}",
    "\u{f1}",
    "\u{3a9}",
    "\u{2192}",
    "\u{2713}",
];

/// About `lines` lines mixing CJK, emoji and ASCII.
pub fn w3(lines: usize) -> Vec<u8> {
    let mut r = Rng(3);
    let mut b = String::new();
    for i in 0..lines {
        let _ = write!(b, "{i:05} ");
        let mut n = 4 + r.intn(8);
        while n > 0 {
            if r.intn(3) == 0 {
                b.push_str(word(&mut r));
            } else {
                b.push_str(WIDE[r.intn(WIDE.len())]);
            }
            b.push(' ');
            n -= 1;
        }
        b.push_str("\r\n");
    }
    b.into_bytes()
}

/// The fraction of p's bytes that belong to escape sequences (ESC through
/// the CSI final byte).
pub fn escape_share(p: &[u8]) -> f64 {
    let mut esc = 0;
    let mut i = 0;
    while i < p.len() {
        if p[i] != 0x1b {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < p.len() && p[j] == b'[' {
            j += 1;
            while j < p.len() && (p[j] < 0x40 || p[j] > 0x7e) {
                j += 1;
            }
        }
        j += 1;
        esc += j.min(p.len()) - i;
        i = j;
    }
    if p.is_empty() {
        return 0.0;
    }
    esc as f64 / p.len() as f64
}

/// The resume fixture: a 200x60 terminal holding 10,000 lines of W2
/// scrollback.
pub const R1_COLS: usize = 200;
pub const R1_ROWS: usize = 60;
pub const R1_LINES: usize = 10000;

/// The bytes that build the R1 fixture.
pub fn r1() -> Vec<u8> {
    w2(R1_LINES, R1_COLS, R1_ROWS)
}
