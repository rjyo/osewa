//! Flow control: the credit window, snapshots and control frames.

use std::fmt::Write;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::helpers::*;
use crate::queue::{INITIAL_WINDOW, MAX_FRAME_DATA};
use crate::Config;

const WINDOW: usize = INITIAL_WINDOW as usize;

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
}

fn own() -> wire::Request {
    wire::Request { own: true, ..Default::default() }
}

/// A client that never credits gets at most the initial window of counted
/// bytes; once its queue passes the lag threshold it is sent a new snapshot.
#[test]
fn no_credit_window_then_snapshot() {
    let _heavy = heavy(1);
    let h = start(Config { lag_bytes: 256 << 10, ..cfg(sh("read x; seq 1 400000; sleep 30")) });
    let mut c = h.dial("dev1");
    c.no_credit = true;
    c.attach(own());
    c.text_input(1, "\r");
    c.wait_for("a lag snapshot", |c| c.gens.len() >= 2);
    assert!(c.counted <= WINDOW, "got {} counted bytes without credit", c.counted);
    assert!(c.gens[1].generation != 0 && c.gens[1].position != 0, "snapshots {:?}", c.gens);
    // The new generation's data still waits for credit.
    std::thread::sleep(Duration::from_millis(100));
    c.drain_now();
    assert!(c.counted <= WINDOW, "got {} counted bytes without credit", c.counted);
}

/// RESPONSE and INPUT_ACK go out at once while data waits for credit.
#[test]
fn control_frames_bypass_credit() {
    let _heavy = heavy(1);
    let h = start(cfg(sh(r#"stty -echo; echo READY; read x; seq 1 400000; while read l; do echo "L:$l"; done"#)));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("READY");
    c.no_credit = true;
    let base = c.counted; // all credited
    c.text_input(1, "\r");
    // Let the flood fill the window.
    c.wait_for("a full window", |c| c.counted - base >= WINDOW - MAX_FRAME_DATA as usize);
    std::thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    let resp = c.request(wire::Request { method: wire::METHOD_INFO.into(), ..Default::default() });
    assert_eq!(resp.sessions.len(), 1, "info: {resp:?}");
    c.text_input(2, "x\r");
    c.wait_for("ack 2", |c| c.acked == 2);
    let d = start.elapsed();
    assert!(d <= Duration::from_millis(500), "control frames took {d:?} behind uncredited data");
    assert!(c.counted - base <= WINDOW, "got {} counted bytes without credit", c.counted - base);
}

/// A snapshot several windows long completes when the client credits on
/// receipt, before it can apply anything.
#[test]
fn snapshot_larger_than_window() {
    let h = start(Config { cols: 80, rows: 24, ..cfg(sh("seq 1 100000; echo DONE; sleep 30")) });
    let mut w = h.dial("w");
    w.attach(wire::Request::default());
    w.wait_text("DONE");
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    c.wait_for("snapshot end", |c| c.gens.len() == 1 && c.snap_ends.contains_key(&0) && c.term.is_some());
    assert!(c.snap.len() > WINDOW, "snapshot only {} bytes", c.snap.len());
    assert!(c.text().contains("DONE"), "text {:?}", c.text());
}

/// After the child exits the stream is over, but its final output and
/// STREAM_END still wait on credit, which must keep counting.
#[test]
fn credit_after_exit() {
    let tune = crate::Tunables { drain_wait: Duration::from_millis(200), ..Default::default() };
    let h = start(Config { tune, ..cfg(sh("read x; seq 1 100000; exit 3")) });
    let mut c = h.dial("dev1");
    c.no_credit = true;
    c.attach(own());
    c.text_input(1, "\r");
    let mut other = h.dial("other");
    assert!(eventually(WAIT, || other.info().exited), "child did not exit");
    c.no_credit = false;
    c.send(&wire::credit(c.stream, c.counted as u32));
    c.wait_for("stream end", |c| c.ended.is_some());
    let end = c.ended.clone().unwrap();
    assert!(end.exit_code == Some(3) && c.text().contains("100000"), "end {end:?}, text {:?}", c.text());
}

/// An LCG step: cheap deterministic noise.
fn lcg(r: &mut u32) -> u32 {
    *r = r.wrapping_mul(1103515245).wrapping_add(12345);
    *r
}

fn wide(b: &mut String, r: &mut u32) {
    let r = lcg(r);
    let ch = char::from_u32(0x4e00 + (r >> 4) % 2000).unwrap();
    write!(b, "\x1b[38;2;{};{};{}m{ch}", r >> 8 & 0xff, r >> 16 & 0xff, r >> 24).unwrap();
}

/// n lines of wide characters in many colors, scrolling.
pub fn busy_lines(cols: usize, n: usize) -> String {
    let mut b = String::new();
    let mut r = 3u32;
    for _ in 0..n {
        for _ in 0..cols / 2 {
            wide(&mut b, &mut r);
        }
        b.push_str("\r\n");
    }
    b
}

/// Fills cols x rows with wide characters in many colors, so the snapshot's
/// bytes through READY are large.
pub fn busy_screen(cols: usize, rows: usize) -> String {
    let mut b = String::from("\x1b[H");
    let mut r = 1u32;
    for y in 0..rows {
        if y > 0 {
            b.push_str("\r\n");
        }
        for _ in 0..cols / 2 {
            wide(&mut b, &mut r);
        }
    }
    b
}

/// The bytes through READY go out at once, uncounted, so a cold attach
/// renders without waiting for credit even when they exceed the window;
/// history still waits.
#[test]
fn ready_not_gated_by_credit() {
    let dir = TempDir::new();
    let p = payload(&dir, "screen", busy_screen(250, 80).as_bytes());
    // History: styled lines that scroll off, several windows' worth.
    let hist = payload(&dir, "history", busy_lines(250, 400).as_bytes());
    let h = start(Config {
        run_dir: dir.0.join("run"),
        cols: 250,
        rows: 80,
        ..cfg(sh(&format!("cat {hist}; cat {p}; printf '\\033[0mDONE'; sleep 30")))
    });
    let mut w = h.dial("w");
    w.attach(wire::Request::default());
    w.wait_text("DONE");

    let mut c = h.dial("dev1");
    c.no_credit = true;
    c.attach(wire::Request::default());
    c.wait_for("READY", |c| c.term.is_some());
    assert!(c.ready_at > WINDOW, "only {} bytes before READY; the test needs more than the window", c.ready_at);
    assert_eq!(c.counted, 0, "counted bytes before READY");
    assert!(c.text().contains("DONE"), "READY screen is not the current screen");
    // History is counted: without credit no more than a window of it.
    std::thread::sleep(Duration::from_millis(100));
    c.drain_now();
    let finished = c.snap_ends.contains_key(&0);
    assert!(c.counted <= WINDOW && !finished, "history: {} counted bytes, finished {finished}", c.counted);
    // Credit lets the rest through.
    c.no_credit = false;
    c.send(&wire::credit(c.stream, c.counted as u32));
    c.wait_for("SNAPSHOT_END", |c| c.snap_ends.contains_key(&0));
}
