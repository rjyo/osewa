//! W4: what real full-screen programs send. Whole
//! frames on the alternate screen, each inside a synchronized-output
//! bracket, with few styles. It alternates every W4_SWITCH frames between
//!
//! - a process table like htop: a header with two colored meters, a
//!   reverse-video column header, rows of numbers that change every frame,
//!   one highlighted row, and a function-key bar;
//! - a code view like an editor: line numbers in dim, source code with
//!   keywords, strings and comments in three colors, scrolling one line per
//!   frame, and a reverse-video status line.
//!
//! The status line of both carries STAMP_PLACEHOLDER, which `osewa-bench
//! gen` replaces with the time each frame is written. Rows are drawn with
//! absolute cursor moves and erase-to-end-of-line, never by scrolling, as
//! curses does after a full repaint.

use std::fmt::Write as _;

use crate::workload::Rng;

/// How many frames W4 shows one view before the other.
pub const W4_SWITCH: usize = 50;

/// A stamp-sized slot that gen fills with the time.
pub const STAMP_PLACEHOLDER: &str = "@T0000000000000@";

/// n frames for a cols x rows screen.
pub fn w4_frames(n: usize, cols: usize, rows: usize) -> Vec<Vec<u8>> {
    let mut r = Rng(4);
    (0..n)
        .map(|i| {
            let mut b = String::from("\x1b[?2026h");
            if (i / W4_SWITCH).is_multiple_of(2) {
                w4_table(&mut b, &mut r, i, cols, rows);
            } else {
                w4_code(&mut b, i, cols, rows);
            }
            b.push_str("\x1b[?2026l");
            b.into_bytes()
        })
        .collect()
}

/// n frames as one stream, entering the alternate screen first.
pub fn w4(n: usize, cols: usize, rows: usize) -> Vec<u8> {
    let mut out = b"\x1b[?1049h\x1b[H\x1b[2J".to_vec();
    for f in w4_frames(n, cols, rows) {
        out.extend_from_slice(&f);
    }
    out
}

fn row(b: &mut String, y: usize) {
    let _ = write!(b, "\x1b[{y};1H");
}

/// Pads or cuts s to exactly w cells (ASCII only).
pub(crate) fn fit(s: &str, w: usize) -> String {
    if s.len() >= w {
        return s[..w].to_string();
    }
    format!("{s}{}", " ".repeat(w - s.len()))
}

/// str::repeat that panics on a negative count, as a bad workload size
/// should.
fn repeat(s: &str, n: i64) -> String {
    assert!(n >= 0, "strings: negative Repeat count");
    s.repeat(n as usize)
}

const COMMANDS: [&str; 10] = [
    "/usr/bin/osewa server",
    "bash",
    "vim main.go",
    "go test ./...",
    "/sbin/init",
    "sshd: dev@pts/0",
    "postgres: writer",
    "node server.js",
    "htop",
    "tmux",
];

fn w4_table(b: &mut String, r: &mut Rng, frame: usize, cols: usize, rows: usize) {
    let cpu = 20 + r.intn(60) as i64;
    let mem = 30 + r.intn(40) as i64;
    let bar = |b: &mut String, label: &str, pct: i64| {
        let w = (cols as i64 - 12) / 2;
        let n = w * pct / 100;
        let _ = write!(b, "{label}[\x1b[32m{}\x1b[0m{}{pct:3}%]", repeat("|", n), repeat(" ", w - n - 5));
    };
    row(b, 1);
    bar(b, "CPU", cpu);
    b.push_str("\x1b[K");
    row(b, 2);
    bar(b, "Mem", mem);
    b.push_str("\x1b[K");
    row(b, 3);
    let _ = write!(b, "\x1b[30;42m{}\x1b[0m", fit("  PID USER      PRI  NI  VIRT   RES  CPU% MEM%   TIME+  Command", cols));
    let sel = frame % (rows - 5);
    for y in 4..rows {
        let i = y - 4;
        let user = ["dev", "root", "postgres"][i % 3];
        let virt = 100 + r.intn(900);
        let res = 10 + r.intn(400);
        let cpu = r.intn(1000) as f64 / 10.0;
        let mem = r.intn(300) as f64 / 10.0;
        let (h, m, s) = (r.intn(60), r.intn(60), r.intn(100));
        let line = format!(
            "{:5} {user:<8}  20   0 {virt:5}M {res:4}M {cpu:5.1} {mem:4.1} {h:2}:{m:02}.{s:02} {}",
            1000 + i * 37,
            COMMANDS[i % COMMANDS.len()]
        );
        row(b, y);
        if i == sel {
            let _ = write!(b, "\x1b[30;46m{}\x1b[0m", fit(&line, cols));
        } else {
            b.push_str(&fit(&line, cols));
            b.push_str("\x1b[K");
        }
    }
    row(b, rows);
    let keys = "F1Help F2Setup F3Search F4Filter F5Tree F6Sort F9Kill F10Quit";
    let _ = write!(b, "\x1b[7m{}\x1b[0m", fit(&format!("{keys}  {STAMP_PLACEHOLDER}"), cols));
}

const GO_SOURCE: &str = r#"// Package holder keeps one terminal session alive.
package holder

import (
	"context"
	"fmt"
	"time"
)

// Config is one session's settings.
type Config struct {
	Cols, Rows int
	Idle       time.Duration // zero disables the idle timeout
}

func (s *Session) loop(ctx context.Context) error {
	buf := make([]byte, 64<<10)
	for {
		n, err := s.pty.Read(buf)
		if err != nil {
			return fmt.Errorf("holder: read: %w", err)
		}
		s.term.Write(buf[:n])
		s.log.Append(buf[:n])
		if s.idle > 0 && time.Since(s.last) > s.idle {
			return nil // "idle" ends the session
		}
	}
}"#;

fn go_source() -> Vec<&'static str> {
    GO_SOURCE.split('\n').collect()
}

fn go_keyword(w: &str) -> bool {
    matches!(w, "package" | "import" | "type" | "struct" | "func" | "for" | "if" | "return" | "nil")
}

/// Colors keywords blue, strings green and comments dim.
pub(crate) fn highlight(line: &str) -> String {
    if let Some(i) = line.find("//") {
        return format!("{}\x1b[2m{}\x1b[0m", highlight(&line[..i]), &line[i..]);
    }
    let mut b = String::new();
    for (i, part) in line.split('"').enumerate() {
        if i % 2 == 1 {
            let _ = write!(b, "\x1b[32m\"{part}\"\x1b[0m");
            continue;
        }
        for (j, w) in part.split(' ').enumerate() {
            if j > 0 {
                b.push(' ');
            }
            if go_keyword(w.trim_start_matches('\t')) {
                let _ = write!(b, "\x1b[34m{w}\x1b[0m");
            } else {
                b.push_str(w);
            }
        }
    }
    b
}

fn w4_code(b: &mut String, frame: usize, cols: usize, rows: usize) {
    let src = go_source();
    for y in 1..rows {
        let n = (frame + y - 1) % src.len();
        row(b, y);
        let line = src[n].replace('\t', "    ");
        let _ = write!(b, "\x1b[2m{:4}\x1b[0m {}\x1b[K", n + 1, highlight(&line));
    }
    row(b, rows);
    let status = format!(" NORMAL  holder/session.go  {}:1  utf-8  go  {STAMP_PLACEHOLDER}", frame % src.len() + 1);
    let _ = write!(b, "\x1b[7m{}\x1b[0m", fit(&status, cols));
}
