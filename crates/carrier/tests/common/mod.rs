//! Shared by the integration tests: a byte-counting stream and
//! terminal-like data.

#![allow(dead_code)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Counts the bytes read through it.
pub struct Counting<S> {
    pub inner: S,
    pub read: Rc<Cell<u64>>,
}

impl<S> Counting<S> {
    pub fn new(inner: S) -> (Counting<S>, Rc<Cell<u64>>) {
        let read = Rc::new(Cell::new(0));
        (Counting { inner, read: read.clone() }, read)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Counting<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        self.read.set(self.read.get() + n as u64);
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counting<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Cuts terminal output into OUTPUT frames of varying size, from a
/// keystroke's echo to a full read buffer.
pub fn frames(data: &[u8]) -> Vec<Vec<u8>> {
    const SIZES: [usize; 5] = [64, 300, 1500, 4096, 16384];
    let mut out = Vec::new();
    let mut rest = data;
    let mut i = 0;
    while !rest.is_empty() {
        let n = SIZES[i % SIZES.len()].min(rest.len());
        let mut f = vec![0xA5, 1, 0, 0, 0];
        f.extend_from_slice(&rest[..n]);
        out.push(f);
        rest = &rest[n..];
        i += 1;
    }
    out
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const SOURCES: [(&str, &str); 6] = [
    ("docs/PROTOCOL.md", include_str!("../../../../docs/PROTOCOL.md")),
    ("docs/QUIC.md", include_str!("../../../../docs/QUIC.md")),
    ("README.md", include_str!("../../../../README.md")),
    ("src/ws/conn.rs", include_str!("../../src/ws/conn.rs")),
    ("src/ws/handshake.rs", include_str!("../../src/ws/handshake.rs")),
    ("src/ws/mod.rs", include_str!("../../src/ws/mod.rs")),
];

/// Text files paged with line numbers, as a pager or `cat -n` shows them.
pub fn paged_text() -> Vec<u8> {
    let mut s = String::new();
    for (name, text) in SOURCES {
        let _ = write!(s, "\x1b[1m── {name} ──\x1b[0m\r\n");
        for (i, line) in text.lines().enumerate() {
            let _ = write!(s, "\x1b[38;5;238m{:5}\x1b[0m │ {line}\r\n", i + 1);
        }
    }
    s.into_bytes()
}

/// About 1 MB of what a terminal shows: files paged with line numbers,
/// directory listings, a build log and a top-like screen redrawn, with
/// the escape sequences programs use. Deterministic.
pub fn terminal_data() -> Vec<u8> {
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut s = String::from_utf8(paged_text()).unwrap();
    let names = ["src", "target", "docs", "Cargo.toml", "README.md", "main.rs", "lib.rs", "build.rs", ".git", "tests"];
    let users = ["osewa", "root", "www-data"];
    for d in 0..150 {
        let _ = write!(s, "\x1b[32mosewa@host\x1b[0m:\x1b[34m~/src/project{d}\x1b[0m$ ls -la\r\ntotal {}\r\n", rng.below(400));
        for _ in 0..25 {
            let dir = rng.below(3) == 0;
            let name = names[rng.below(names.len() as u64) as usize];
            let _ = write!(
                s,
                "{}rw-r--r-- {:2} {:<8} staff {:8} Oct {:2} {:02}:{:02} {}{}{}\r\n",
                if dir { 'd' } else { '-' },
                rng.below(9) + 1,
                users[rng.below(3) as usize],
                rng.below(1 << 20),
                rng.below(30) + 1,
                rng.below(24),
                rng.below(60),
                if dir { "\x1b[1;34m" } else { "" },
                name,
                if dir { "\x1b[0m" } else { "" },
            );
        }
    }
    for i in 0..4000 {
        let crate_name = format!("{}-{}", names[i % names.len()].trim_start_matches('.'), rng.below(50));
        let _ = write!(
            s,
            "   \x1b[1;32mCompiling\x1b[0m {crate_name} v0.{}.{} (/home/osewa/src/{crate_name})\r\n",
            rng.below(20),
            rng.below(10)
        );
        if i % 97 == 0 {
            let _ = write!(
                s,
                "\x1b[1;33mwarning\x1b[0m\x1b[1m: unused variable: `x{i}`\x1b[0m\r\n  \x1b[1;34m-->\x1b[0m src/lib.rs:{}:9\r\n",
                rng.below(900)
            );
        }
    }
    for _ in 0..300 {
        let _ = write!(
            s,
            "\x1b[H\x1b[2Jtop - 12:{:02}:{:02} up 3 days, load average: {}.{:02}, {}.{:02}\r\n\x1b[7m  PID USER      %CPU %MEM COMMAND\x1b[0m\r\n",
            rng.below(60),
            rng.below(60),
            rng.below(4),
            rng.below(100),
            rng.below(4),
            rng.below(100)
        );
        for _ in 0..30 {
            let _ = write!(
                s,
                "{:5} {:<8} {:4}.{} {:4}.{} {}\x1b[K\r\n",
                rng.below(60000),
                users[rng.below(3) as usize],
                rng.below(100),
                rng.below(10),
                rng.below(20),
                rng.below(10),
                names[rng.below(names.len() as u64) as usize]
            );
        }
    }
    s.into_bytes()
}
