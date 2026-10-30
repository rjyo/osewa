//! Ported from review3_test.go.

use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::catchup::{link_time, session_cfg};
use super::helpers::*;
use super::main::echo_flood_argv;
use super::round3::last_mode;
use super::unit::drain;
use crate::queue::Kind;
use crate::Config;

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
}

const LINK: usize = 1_000_000 / 8;

fn echo_flood(env: String, catch_up_delay: Duration) -> Config {
    Config { cols: 80, rows: 24, catch_up_delay, env: vec![env], ..cfg(echo_flood_argv()) }
}

/// Review 3 #1: a stream streaming again after catch-up gets its echo as
/// output: typing costs no screen per key and doesn't hold off the full
/// snapshot that restores scrollback.
#[test]
fn no_input_screens_while_streaming() {
    let _heavy = heavy(1);
    let h = start(echo_flood(format!("OSEWA_TEST_ECHOFLOOD={},{},0,80,24", 10 * LINK, 1500), Duration::from_secs(1)));
    let mut c = h.dial_via("dev1", slow_link(LINK));
    c.attach(wire::Request { own: true, cols: 80, rows: 24, ..Default::default() });
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "go\r");
    c.wait_for("catch-up", |c| c.gens.len() > 1 && is_screen(c.gens.last().unwrap()));
    // The flood stops; once a screen lands and no next one follows, the
    // stream is streaming, and the full snapshot is catch_up_delay away.
    quiet_screens(&mut c, Duration::from_millis(400));
    let n = c.gens.len();
    // Type faster than catch_up_delay for longer than it.
    for i in 0..8u64 {
        c.text_input(i + 2, &format!("s{i}\r"));
        c.wait_text(&format!("ECHO:s{i}"));
        c.drain(Duration::from_millis(250));
    }
    let screens = c.gens[n..].iter().filter(|b| is_screen(b)).count();
    let full = c.gens[n..].iter().any(|b| !is_screen(b));
    assert!(screens == 0 && full, "while typing: {screens} screens, full snapshot {full}");
}

/// Typing at two keys per screen: keystrokes may abandon screens part way
/// out (docs/PROTOCOL.md), but a screen that abandoned one is left to
/// finish, so no two screens in a row are dropped: fast typing never
/// starves the client. (Review 3 #2 asked that no follow-up be dropped; the
/// spec has since made abandoning the faster choice.)
#[test]
fn input_screens_not_starved() {
    let _heavy = heavy(1);
    let h = start(echo_flood(format!("OSEWA_TEST_ECHOFLOOD={},{},0,80,24", 10 * LINK, 60000), Duration::ZERO));
    let mut c = h.dial_via("dev1", slow_link(LINK));
    c.attach(wire::Request { own: true, cols: 80, rows: 24, ..Default::default() });
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "go\r");
    c.wait_for("catch-up", |c| c.gens.len() > 1 && is_screen(c.gens.last().unwrap()));
    c.wait_for("a whole screen", |c| {
        let i = c.gens.len() - 1;
        is_screen(&c.gens[i]) && c.snap_ends.contains_key(&i)
    });
    let screen = link_time(c.snap.len(), LINK);
    let first = c.gens.len();
    for i in 0..12u64 {
        c.text_input(i + 2, &format!("f{i}\r"));
        c.drain(screen / 2);
    }
    c.wait_text("ECHO:f11");
    let (mut dropped, mut run) = (0, 0);
    for i in first..c.gens.len() - 1 {
        if c.snap_ends.contains_key(&i) {
            run = 0;
            continue;
        }
        dropped += 1;
        // A screen that abandoned another always finishes, so drops never
        // come two in a row.
        run += 1;
        assert!(run <= 1, "screens {} and {i} both dropped part way", i - 1);
    }
    eprintln!("{} screens while typing (one per {screen:?}), {dropped} dropped part way", c.gens.len() - first);
}

/// Review 3 #3: a stream replacing another on a connection starts with no
/// catch-up state from it.
#[test]
fn reset_flow_clears_catch_up() {
    let clock = super::unit::FakeClock::new();
    let mut q = clock.queue();
    q.reset_flow(1);
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_BEGIN, 1, Vec::new()), 7);
    let data: Rc<[u8]> = Rc::from(vec![0u8; 100]);
    q.chunk(wire::TYPE_SNAPSHOT_DATA, 1, &data, 0, data.len());
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_END, 1, Vec::new()), 7);
    drain(&mut q);
    q.end_written();
    assert!(q.end_armed && q.snap_data != 0 && q.last_end == 7, "setup");
    q.reset_flow(2);
    assert!(
        !q.end_armed && q.end_at == 0 && q.end_gen == 0 && q.last_end == 0 && q.snap_data == 0 && q.end_sent_at.is_none(),
        "catch-up state survived"
    );
}

/// Handles frames until a screen has fully landed and quiet passes with no
/// new snapshot.
fn quiet_screens(c: &mut Client, quiet: Duration) {
    let deadline = Instant::now() + WAIT;
    let left = |d: Instant| d.saturating_duration_since(Instant::now());
    'outer: loop {
        let i = c.gens.len() - 1;
        if is_screen(&c.gens[i]) && c.snap_ends.contains_key(&i) {
            let calm = Instant::now() + quiet;
            loop {
                if Instant::now() >= calm {
                    return;
                }
                assert!(Instant::now() < deadline, "screens never settled");
                match c.next_frame(left(calm).min(left(deadline))) {
                    Ok(Some(f)) => {
                        c.handle(&f);
                        if f.ty == wire::TYPE_SNAPSHOT_BEGIN {
                            continue 'outer;
                        }
                    }
                    Ok(None) => panic!("connection closed"),
                    Err(()) => {}
                }
            }
        }
        match c.next_frame(left(deadline)) {
            Ok(Some(f)) => c.handle(&f),
            Ok(None) => panic!("connection closed"),
            Err(()) => panic!("screens never settled"),
        }
    }
}

/// A stream in catch-up skips output, but not the session state a screen
/// doesn't carry: an owner change and an input mode change during a flood
/// reach a slow viewer with the next screen.
#[test]
fn catch_up_keeps_owner_and_mode() {
    let _heavy = heavy(1);
    let h = start(Config { owner_grace: Duration::from_millis(200), ..cfg(sh("yes tick & sleep 1; stty -echo; sleep 30")) });
    let mut a = h.dial("a");
    a.attach(wire::Request { own: true, ..Default::default() });
    let mut watch = h.dial_via("watcher", slow_link(1_000_000 / 8));
    watch.attach(wire::Request::default());
    watch.wait_for("catch-up", |c| c.gens.iter().any(is_screen));
    a.close();
    let last_owner = |c: &Client| c.events.iter().rev().find(|e| e.kind == wire::EVENT_OWNER).map(|e| e.owner.clone());
    watch.wait_for("owner lapse", |c| last_owner(c).is_some_and(|o| o.is_empty()));
    watch.wait_for("echo off", |c| last_mode(c).is_some_and(|m| !m.echo));
    assert!(watch.gaps == 0 && watch.stale == 0, "positions: {} gaps, {} stale", watch.gaps, watch.stale);
}

/// The full snapshot that ends catch-up restores the owner too: an owner
/// change skipped in the last screens-only stretch isn't lost when the
/// stream streams again. An attach's snapshot restores nothing.
#[test]
fn full_snapshot_restores_owner() {
    local(async {
        let s = unit_session(session_cfg(80, 24, 1 << 20));
        let q = add_conn(&s, 1, "v", Some(unit_stream(1, 1, 1, true)));
        s.borrow_mut().core.owner = "a".into();
        s.borrow_mut().set_owner(""); // skipped: the stream is screens-only
        let owners = || {
            let (mut n, mut last) = (0, String::new());
            for e in &q.borrow().data {
                if e.kind == Kind::Record && e.rec().only == Some(1) {
                    let ev: wire::EventMsg = serde_json::from_slice(&e.rec().data).unwrap();
                    if ev.kind == wire::EVENT_OWNER {
                        (n, last) = (n + 1, ev.owner);
                    }
                }
            }
            (n, last)
        };
        assert_eq!(owners().0, 0, "owner event reached a screens-only stream");
        let send = |history, reason| {
            let mut b = s.borrow_mut();
            let (core, c) = b.split(1).unwrap();
            core.send_snapshot(c, history, reason);
        };
        s.borrow_mut().conns.get_mut(&1).unwrap().stream.as_mut().unwrap().catch_up = false;
        send(true, "full");
        let (n, last) = owners();
        assert!(n == 1 && last.is_empty(), "after the full snapshot: {n} owner records, last {last:?}");
        q.borrow_mut().drop_data();
        send(true, "attach");
        assert_eq!(owners().0, 0, "attach snapshot restored state");
    });
}
