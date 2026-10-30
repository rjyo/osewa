//! `osewa-bench gen` (Go's gen.go): workload lines with a stamp line before
//! every block, paced to a rate, optionally with an application echo row.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, SystemTime};

use crate::stamp::stamp;
use crate::w4::{w4_frames, STAMP_PLACEHOLDER, W4_SWITCH};
use crate::workload::{w1, w2, w3};
use crate::Error;

/// How many workload lines go between stamps: few enough that a stamp is
/// always on a 24-row screen.
pub const BLOCK_LINES: usize = 8;

/// Splits a workload into the pieces gen emits whole: lines, each with any
/// redraw that precedes it, so an alternate-screen excursion is never cut.
pub fn units(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        match data.windows(2).position(|w| w == b"\r\n") {
            None => {
                out.push(data.to_vec());
                break;
            }
            Some(i) => {
                out.push(data[..i + 2].to_vec());
                data = &data[i + 2..];
            }
        }
    }
    out
}

/// Marks where a repaint gets its stamp at write time; it is as long as a
/// stamp without the line end.
pub const PLACEHOLDER: &[u8] = STAMP_PLACEHOLDER.as_bytes();

fn replace_once(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    match find(hay, from) {
        Some(i) => [&hay[..i], to, &hay[i + from.len()..]].concat(),
        None => hay.to_vec(),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Go's bytes.ReplaceAll for a non-empty `from`.
pub(crate) fn replace_all(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    let mut rest = hay;
    while let Some(i) = find(rest, from) {
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(to);
        rest = &rest[i + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Stamps every alternate-screen repaint (W2's "\x1b[?1049h\x1b[H\x1b[2J"
/// ... "\x1b[?1049l"): on the last row as it starts, since the repaint draws
/// top down, and on the top row once it is done, so a stamp is on screen
/// wherever a snapshot cuts it.
fn stamp_repaints(mut u: Vec<Vec<u8>>, rows: usize) -> Vec<Vec<u8>> {
    let enter: &[u8] = b"\x1b[?1049h\x1b[H\x1b[2J";
    let leave: &[u8] = b"\x1b[?1049l";
    for x in u.iter_mut() {
        if find(x, enter).is_none() {
            continue;
        }
        let at_start = [enter, format!("\x1b[{rows};1H").as_bytes(), PLACEHOLDER].concat();
        let v = replace_once(x, enter, &at_start);
        let at_end = [b"\x1b[1;1H", PLACEHOLDER, leave].concat();
        *x = replace_once(&v, leave, &at_end);
    }
    u
}

/// Starts a framed workload: alternate screen, cleared.
pub const FRAMED_PREFIX: &str = "\x1b[?1049h\x1b[H\x1b[2J";

pub fn workload(kind: &str, cols: usize, rows: usize) -> Result<Vec<Vec<u8>>, Error> {
    Ok(match kind {
        "w1" => units(&w1(2000)),
        "w2" => stamp_repaints(units(&w2(2000, cols, rows)), rows),
        "w3" => units(&w3(2000)),
        // Whole frames, each stamped in its status line. The generator
        // enters the alternate screen once (FRAMED_PREFIX): repeating it as
        // the frames cycle would clear the echo row.
        "w4" => w4_frames(2 * W4_SWITCH, cols, rows),
        _ => return Err(Error(format!("unknown workload {}", osewa_wire::go_quote(kind)))),
    })
}

pub fn gen_main(args: &[String], w: &mut dyn Write) -> Result<(), Error> {
    gen_with(args, io::stdin(), w)
}

/// gen with stdin as given.
pub fn gen_with(args: &[String], stdin: impl Read + Send + 'static, w: &mut dyn Write) -> Result<(), Error> {
    let mut fs = osewa_flag::FlagSet::new("gen");
    fs.value("kind", "w2", "workload: w1, w2, w3 or w4")
        .value("rate", "0", "bytes per second, 0 for as fast as the terminal takes it")
        .value("cols", "80", "W2 and W4 width")
        .value("rows", "24", "terminal height")
        .value("duration", "0", "stop after this long, 0 for never")
        .bool(
            "echo",
            false,
            "flood rows 1..rows-1 only and echo typed input on the last row; run under `stty raw -echo`, ^C stops",
        );
    let p = crate::parse(&fs, args)?;
    let (rate, cols, rows) = (crate::int(&p, "rate")?, crate::int(&p, "cols")?, crate::int(&p, "rows")?);
    let dur = crate::duration(&p, "duration")?;
    let echo = p.bool("echo");
    // With -echo the workload gets one row less, leaving the echo row.
    let h = if echo { rows - 1 } else { rows };
    let kind = p.str("kind");
    let units = workload(&kind, cols.max(0) as usize, h.max(0) as usize)?;
    let mut g = Generator::new(w, units, rate, dur, Box::new(SystemTime::now));
    // Framed workloads stamp every frame themselves; a stamp line would land
    // in the middle of the frame.
    g.framed = kind == "w4";
    if echo {
        g.rows = rows;
        let (tx, rx) = mpsc::sync_channel(1024);
        std::thread::spawn(move || {
            let mut stdin = stdin;
            let mut b = [0u8; 256];
            loop {
                match stdin.read(&mut b) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        for &c in &b[..n] {
                            if tx.send(c).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        g.input = Some(rx);
    }
    g.run().map_err(|e| Error(e.to_string()))
}

pub type Clock = Box<dyn FnMut() -> SystemTime + Send>;

/// Writes stamped blocks of units, cycling, paced to rate bytes/s. With
/// input set it is a flood plus an application echo: output scrolls in rows
/// 1..rows-1 and every typed byte is drawn on the last row at once, so the
/// echo stays on screen however fast the flood scrolls. That makes
/// keystroke-to-echo measurable during a flood.
pub struct Generator<'a> {
    w: &'a mut dyn Write,
    units: Vec<Vec<u8>>,
    rate: i64,
    dur: Duration,
    now: Clock,
    pub rows: i64,
    pub input: Option<Receiver<u8>>,
    typed: Vec<u8>,
    pub framed: bool,
}

/// Go's time.Time.Sub for the times gen compares: negative when b is later.
fn sub(a: SystemTime, b: SystemTime) -> i128 {
    match a.duration_since(b) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

impl<'a> Generator<'a> {
    pub fn new(w: &'a mut dyn Write, units: Vec<Vec<u8>>, rate: i64, dur: Duration, now: Clock) -> Generator<'a> {
        Generator { w, units, rate, dur, now, rows: 0, input: None, typed: Vec::new(), framed: false }
    }

    pub fn run(&mut self) -> io::Result<()> {
        let start = (self.now)();
        if self.framed {
            self.w.write_all(FRAMED_PREFIX.as_bytes())?;
        }
        if self.input.is_some() {
            // Scroll region above the echo row; the cursor starts inside it.
            write!(self.w, "\x1b[1;{}r\x1b[{};1H", self.rows - 1, self.rows - 1)?;
        }
        let mut sent: i64 = 0;
        let mut body = Vec::new();
        let mut i = 0;
        loop {
            body.clear();
            let mut n = 0;
            while n < BLOCK_LINES && (n == 0 || !self.framed) {
                body.extend_from_slice(&self.units[i % self.units.len()]);
                i += 1;
                n += 1;
            }
            let stop = if self.rate > 0 {
                let ns = ((sent + body.len() as i64) as f64 / self.rate as f64 * 1e9) as u64;
                self.wait_until(start + Duration::from_nanos(ns))?
            } else {
                self.drain()?
            };
            if stop {
                return Ok(());
            }
            let t = (self.now)();
            if !self.dur.is_zero() && sub(t, start) >= self.dur.as_nanos() as i128 {
                return Ok(());
            }
            let mut buf = Vec::with_capacity(body.len() + 20);
            if !self.framed {
                buf.extend_from_slice(&stamp(t));
            }
            buf.extend_from_slice(&body);
            // Repaints carry a stamp too, so an alternate screen has an age.
            if find(&buf, PLACEHOLDER).is_some() {
                let st = stamp(t);
                buf = replace_all(&buf, PLACEHOLDER, &st[..PLACEHOLDER.len()]);
            }
            self.w.write_all(&buf)?;
            sent += buf.len() as i64;
        }
    }

    /// Sleeps until due, echoing input meanwhile; true after ^C or the end
    /// of input.
    fn wait_until(&mut self, due: SystemTime) -> io::Result<bool> {
        loop {
            let d = sub(due, (self.now)());
            if d <= 0 {
                return self.drain();
            }
            let d = Duration::from_nanos(d as u64);
            let Some(input) = &self.input else {
                std::thread::sleep(d);
                return Ok(false);
            };
            let r = input.recv_timeout(d);
            match r {
                Ok(c) => {
                    if self.key(Some(c))? {
                        return Ok(true);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if self.key(None)? {
                        return Ok(true);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Echoes whatever input is waiting.
    fn drain(&mut self) -> io::Result<bool> {
        loop {
            let Some(input) = &self.input else { return Ok(false) };
            let c = match input.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Disconnected) => None,
                Err(TryRecvError::Empty) => return Ok(false),
            };
            if self.key(c)? {
                return Ok(true);
            }
        }
    }

    fn key(&mut self, c: Option<u8>) -> io::Result<bool> {
        match c {
            None | Some(0x03) => return Ok(true),
            Some(b'\r') | Some(0x15) => self.typed.clear(),
            Some(0x7f) => {
                self.typed.pop();
            }
            Some(c) => self.typed.push(c),
        }
        // Save and restore the cursor so the flood continues where it was.
        let mut b = format!("\x1b7\x1b[{};1H\x1b[2K> ", self.rows).into_bytes();
        b.extend_from_slice(&self.typed);
        b.extend_from_slice(b"\x1b8");
        self.w.write_all(&b)?;
        Ok(false)
    }
}
