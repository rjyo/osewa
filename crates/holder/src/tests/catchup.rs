//! Ported from catchup_test.go.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_ghostty as ghostty;
use osewa_wire as wire;

use super::helpers::*;
use super::main::{echo_flood_argv, echo_flood_raw_argv};
use crate::log::REC_OUTPUT;
use crate::queue::{Kind, SendQueue, INITIAL_WINDOW};
use crate::session::Session;
use crate::{Config, Tunables};

const WINDOW: usize = INITIAL_WINDOW as usize;

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
}

fn own() -> wire::Request {
    wire::Request { own: true, ..Default::default() }
}

/// How long n bytes take on a link of rate bytes a second.
pub fn link_time(n: usize, rate: usize) -> Duration {
    Duration::from_secs_f64(n as f64 / rate as f64)
}

/// After the attach's first snapshot, SNAPSHOT_DATA is counted: a catch-up
/// screen waits for credit like output does.
#[test]
fn catch_up_screen_data_counted() {
    let _heavy = heavy(1);
    let h = start(cfg(sh("read x; seq 1 400000; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.no_credit = true;
    let base = c.counted;
    let readies = Rc::new(RefCell::new(0));
    let r = readies.clone();
    c.on_ready = Some(Box::new(move |_| *r.borrow_mut() += 1));
    c.text_input(1, "\r");
    c.wait_for("a catch-up screen begins", |c| c.gens.iter().any(is_screen));
    std::thread::sleep(Duration::from_millis(300));
    c.drain_now();
    let n = *readies.borrow();
    assert!(c.counted - base <= WINDOW && n == 0, "without credit: {} counted bytes, {n} screens ready", c.counted - base);
    c.no_credit = false;
    c.send(&wire::credit(c.stream, (c.counted - base) as u32));
    c.wait_for("the screen once credited", |_| *readies.borrow() > 0);
}

/// The next catch-up screen goes out only once the previous one is
/// credited, however far behind the stream is.
#[test]
fn catch_up_paced_by_credit() {
    let _heavy = heavy(1);
    let h = start(cfg(sh("read x; while :; do seq 1 20000; done")));
    // A slow link, so the stream falls behind at all.
    let mut c = h.dial_via("dev1", slow_link(1_000_000 / 8));
    c.attach(own());
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "\r");
    let base = loop {
        let f = c.next_handled();
        if f.ty == wire::TYPE_SNAPSHOT_BEGIN && is_screen(c.gens.last().unwrap()) {
            // Stop crediting from this screen on.
            c.no_credit = true;
            break c.counted;
        }
    };
    let n = c.gens.len();
    c.drain(Duration::from_secs(1));
    assert_eq!(c.gens.len(), n, "more snapshots without credit");
    c.no_credit = false;
    c.send(&wire::credit(c.stream, (c.counted - base) as u32));
    c.wait_for("the next screen", |c| c.gens.len() > n);
}

/// A session without a holder loop, as Go's newSession builds.
pub fn session_cfg(cols: i64, rows: i64, lag_bytes: usize) -> Config {
    Config {
        id: "x".into(),
        cols,
        rows,
        log_bytes: 1 << 20,
        lag_bytes,
        catch_up_delay: Duration::from_secs(3600),
        ..Default::default()
    }
}

/// The generations of the SNAPSHOT_BEGINs queued on q.
fn begins(q: &RefCell<SendQueue>) -> Vec<u32> {
    q.borrow()
        .data
        .iter()
        .filter(|e| e.is_marker(wire::TYPE_SNAPSHOT_BEGIN))
        .map(|e| serde_json::from_slice::<wire::SnapshotBegin>(e.payload()).unwrap().generation)
        .collect()
}

/// The SNAPSHOT_DATA bytes queued on q.
pub fn queued_data(q: &RefCell<SendQueue>) -> Vec<u8> {
    let mut b = Vec::new();
    for e in &q.borrow().data {
        if e.ty == wire::TYPE_SNAPSHOT_DATA && e.kind != Kind::Record {
            b.extend_from_slice(e.payload());
        }
    }
    b
}

/// Takes the next frame q would send.
pub fn next(q: &RefCell<SendQueue>) {
    q.borrow_mut().next_into(&mut Vec::new());
}

fn stream<T>(s: &Rc<RefCell<Session>>, f: impl FnOnce(&crate::session::Stream) -> T) -> T {
    f(s.borrow().conns[&1].stream.as_ref().unwrap())
}

/// While catching up, screens only: output is skipped, not queued. An input
/// screen replaces a queued screen nothing of which has gone out; abandons
/// one part way out (docs/PROTOCOL.md: "may be abandoned"); but queues
/// behind one that itself abandoned another, so fast typing still finishes
/// screens; and a later keystroke replaces only that queued one.
#[test]
fn catch_up_screens_only_and_input_screens() {
    local(async {
        let s = unit_session(session_cfg(80, 24, 1000));
        let q = add_conn(&s, 1, "o", Some(unit_stream(1, 1, 1, false)));
        let out = || s.borrow_mut().handle_output(&[b'x'; 9000]);
        let records = || q.borrow().data.iter().filter(|e| e.kind == Kind::Record && e.rec().kind == REC_OUTPUT).count();
        let first_is = |ty| q.borrow().data.front().is_some_and(|e| e.is_marker(ty));
        out();
        let g = begins(&q);
        assert!(g == [1] && stream(&s, |st| st.screen_pending && st.catch_up), "first catch-up: {g:?}");
        out();
        let skipped = stream(&s, |st| st.skipped);
        assert!(records() == 0 && skipped != 0, "screens only: {} records queued, {skipped} skipped", records());
        // Unstarted: replaced.
        s.borrow_mut().input_screen();
        let g = begins(&q);
        assert!(g == [2] && stream(&s, |st| st.input_pending), "unstarted screen not replaced: {g:?}");
        // Part way out: abandoned, its READY and END never go.
        next(&q); // BEGIN 2
        q.borrow_mut().snap_data = 1;
        s.borrow_mut().input_screen();
        let g = begins(&q);
        assert!(g == [3] && first_is(wire::TYPE_SNAPSHOT_BEGIN), "partly sent screen not abandoned: {g:?}");
        // That one abandoned another, so it finishes, the new one queued
        // right behind it.
        next(&q); // BEGIN 3
        next(&q); // all its DATA
        s.borrow_mut().input_screen();
        let g = begins(&q);
        assert!(g == [4] && first_is(wire::TYPE_SNAPSHOT_READY), "mostly sent screen not left to finish: {g:?}");
        // Another keystroke replaces only the queued one.
        s.borrow_mut().input_screen();
        let g = begins(&q);
        assert!(g == [5] && first_is(wire::TYPE_SNAPSHOT_READY), "queued screen not replaced: {g:?}");
        // A flood screen part way out is abandoned for an input screen.
        q.borrow_mut().drop_data();
        {
            let mut b = s.borrow_mut();
            let (core, c) = b.split(1).unwrap();
            core.send_snapshot(c, false, "lag"); // gen 6
        }
        next(&q);
        next(&q);
        s.borrow_mut().input_screen();
        let g = begins(&q);
        assert!(g == [7] && first_is(wire::TYPE_SNAPSHOT_BEGIN), "flood screen not abandoned: {g:?}");
        // A late completion of an abandoned screen is ignored.
        s.borrow_mut().snapshot_done(1, 6);
        assert!(stream(&s, |st| st.screen_pending), "stale completion taken");
    });
}

/// Review 2 #1: behind a relay that buffers megabytes, credit still paces
/// catch-up screens, so a keystroke's echo shows up within a few screens'
/// link time rather than after the relay's whole backlog.
#[test]
fn catch_up_behind_buffering_relay() {
    let _heavy = heavy(1);
    const LINK: usize = 1_000_000 / 8;
    let h = start(Config {
        cols: 200,
        rows: 60,
        env: vec![format!("OSEWA_TEST_ECHOFLOOD={},{}", 10 * LINK, 8000)],
        ..cfg(echo_flood_argv())
    });
    let mut c = h.dial_via("dev1", buffered_link(LINK));
    c.attach(wire::Request { own: true, cols: 200, rows: 60, ..Default::default() });
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "go\r");
    c.wait_for("catch-up", |c| c.gens.iter().any(is_screen));
    std::thread::sleep(Duration::from_secs(1)); // let a relay without pacing fill up
    let typed = Instant::now();
    c.text_input(2, "Z\r");
    c.wait_text("ECHO:Z");
    let d = typed.elapsed();
    let screen = link_time(c.ready_at, LINK);
    eprintln!("echo after {d:?}; a {}-byte screen takes {screen:?} on the link", c.ready_at);
    assert!(d <= 4 * screen + Duration::from_secs(1), "echo took {d:?}, over 4 screens ({screen:?})");
}

/// Review 2 #2: a 2 MiB paste into a child that isn't reading, from a client
/// that keeps 512 KiB unacked at most, neither ends the stream nor loses
/// anything: it all lands once the child reads.
#[test]
fn big_paste_into_busy_child() {
    let h = start(cfg(sh("stty raw -echo; echo R; sleep 1; head -c 2097152 | wc -c; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(own());
    // Before stty runs, a canonical-mode tty would drop most of a paste.
    c.wait_text("R");
    let chunk = "x".repeat(32 << 10);
    const FRAMES: u64 = 64; // 2 MiB
    for seq in 1..=FRAMES {
        c.wait_for("room under 512 KiB unacked", |c| (seq - 1 - c.acked) * (chunk.len() as u64) < 512 << 10);
        c.text_input(seq, &chunk);
    }
    c.wait_text("2097152");
    assert!(c.ended.is_none() && c.acked == FRAMES, "ended {:?}, acked {}", c.ended, c.acked);
}

/// Review 2 #3: on a long link a burst bigger than the first window is not
/// a slow client: no catch-up, and the attach snapshot's history completes.
#[test]
fn long_rtt_burst_no_catch_up() {
    let _heavy = heavy(1);
    let h = start(cfg(sh("seq 1 20000; read x; seq 1 25000; echo DONE; sleep 30")));
    let mut w = h.dial("w");
    w.attach(wire::Request::default());
    w.wait_text("20000");
    let mut c = h.dial_via("dev1", delay_link(Duration::from_millis(300)));
    c.attach(own());
    c.wait_for("READY", |c| c.term.is_some());
    // Burst while history is still on its way.
    c.text_input(1, "\r");
    c.wait_text("DONE");
    c.wait_for("history", |c| c.snap_ends.contains_key(&0));
    let hist = c.hist_bytes.get(&0).copied().unwrap_or(0);
    assert!(c.gens.len() == 1 && hist != 0, "snapshots {:?}, history {hist} bytes", c.gens);
}

/// Review 2 #4: steady output the link carries doesn't keep a stream in
/// catch-up: it gets its full snapshot back.
#[test]
fn catch_up_exits_under_steady_output() {
    let _heavy = heavy(1);
    const LINK: usize = 1_000_000 / 8;
    let h = start(Config {
        cols: 200,
        rows: 60,
        env: vec![format!("OSEWA_TEST_ECHOFLOOD={},{},{}", 10 * LINK, 1500, LINK / 10)],
        ..cfg(echo_flood_argv())
    });
    let mut c = h.dial_via("dev1", slow_link(LINK));
    c.attach(wire::Request { own: true, cols: 200, rows: 60, ..Default::default() });
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "go\r");
    c.wait_for("catch-up", |c| c.gens.iter().any(is_screen));
    c.wait_for("a full snapshot after catch-up", |c| {
        let i = c.gens.len() - 1;
        i > 0 && !is_screen(&c.gens[i]) && c.snap_ends.contains_key(&i)
    });
}

/// Catch-up screens come from snapshot_screen: no full encode (it would
/// fail here), and their DATA doesn't grow with scrollback.
#[test]
fn catch_up_screen_skips_full_encode() {
    local(async {
        let s = unit_session(Config { scrollback_bytes: 10 << 20, ..session_cfg(80, 24, 1000) });
        for i in 0..20000 {
            s.borrow_mut().core.term.write(format!("scrollback line {i}\r\n").as_bytes());
        }
        let full = s.borrow().core.term.snapshot().unwrap();
        s.borrow_mut().core.cfg.tune = Tunables { fail_encode: true, ..Default::default() };

        let q = add_conn(&s, 1, "o", Some(unit_stream(1, 1, 1, false)));
        s.borrow_mut().handle_output(&[b'x'; 9000]);
        assert!(
            s.borrow().conns[&1].stream.as_ref().is_some_and(|st| st.screen_pending),
            "no catch-up screen"
        );
        let data = queued_data(&q).len();
        // Compact: without the history rows that share the screen's page.
        let want = s.borrow().core.term.snapshot_screen_compact().unwrap();
        let plain = s.borrow().core.term.snapshot_screen().unwrap();
        assert!(
            data == want.len() && 2 * data <= plain.len() && data * 10 <= full.len(),
            "screen DATA {data} bytes; compact {}, plain screen {}, full {}",
            want.len(),
            plain.len(),
            full.len()
        );
        // And it is the same screen.
        let mut dec = ghostty::Decoder::new().unwrap();
        dec.feed(&want);
        let mut restored = dec.ready(ghostty::Callbacks::default()).unwrap();
        let now = s.borrow().core.term.snapshot().unwrap();
        let mut r = ghostty::restore(&now, ghostty::Callbacks::default()).unwrap();
        assert_eq!(screen_state(&mut restored), screen_state(&mut r), "compact screen differs");
    });
}

/// While catching up, a keystroke's echo goes out in the very next screen,
/// cut just after the input reached the program, instead of waiting for the
/// next flood screen to be cut and credited.
#[test]
fn input_triggered_screen() {
    // It measures latency: no flood test may run beside it.
    let _heavy = heavy(HEAVY_UNITS);
    const LINK: usize = 1_000_000 / 8;
    let rtt = Duration::from_millis(100);
    let h = start(Config {
        cols: 80,
        rows: 24,
        env: vec![format!("OSEWA_TEST_ECHOFLOOD={},{},0,80,24", 10 * LINK, 60000)],
        ..cfg(echo_flood_argv())
    });
    let mut c = h.dial_via("dev1", slow_delay_link(LINK, rtt));
    c.attach(wire::Request { own: true, cols: 80, rows: 24, ..Default::default() });
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "go\r");
    c.wait_for("catch-up", |c| c.gens.iter().any(is_screen));

    let mut lat = Vec::new();
    for i in 0..30u64 {
        let key = format!("k{i}");
        let typed = Instant::now();
        c.text_input(i + 2, &format!("{key}\r"));
        c.wait_text(&format!("ECHO:{key}"));
        lat.push(typed.elapsed());
        // Typing pace, while frames keep flowing.
        c.drain(Duration::from_millis(30 + i * 37 % 50));
    }
    check_echo_latency(lat, c.ready_at, LINK, rtt);
}

/// Fails unless the 95th percentile echo is within one screen plus the
/// link's delay, after the settle: while the owner types, nothing else is
/// in flight ahead of the keystroke's screen.
fn check_echo_latency(mut lat: Vec<Duration>, ready_at: usize, link: usize, rtt: Duration) {
    lat.sort();
    let (p50, p95) = (lat[lat.len() / 2], lat[lat.len() * 95 / 100]);
    let screen = link_time(ready_at, link);
    eprintln!("echo p50 {p50:?} p95 {p95:?}; a {ready_at}-byte screen takes {screen:?}, RTT {rtt:?}");
    let bound = screen + rtt + Tunables::default().echo_settle + Duration::from_millis(60);
    assert!(p95 <= bound, "echo p95 {p95:?} over one screen + RTT ({bound:?})");
}

/// Typing like the e2e echo test: a token, then Enter once it's seen. Enter
/// makes an input screen of its own that is often still going out when the
/// next token arrives; that screen is abandoned (or, if mostly sent, the new
/// one follows right behind it), so the token's echo is still one screen
/// plus a round trip away, not two.
#[test]
fn input_screen_abandons_partial() {
    // It measures latency: no flood test may run beside it.
    let _heavy = heavy(HEAVY_UNITS);
    const LINK: usize = 1_000_000 / 8;
    let rtt = Duration::from_millis(100);
    let h = start(Config {
        cols: 80,
        rows: 24,
        env: vec![format!("OSEWA_TEST_ECHOFLOOD={},{},0,80,24,1", 10 * LINK, 60000)],
        ..cfg(echo_flood_raw_argv())
    });
    let mut c = h.dial_via("dev1", slow_delay_link(LINK, rtt));
    c.attach(wire::Request { own: true, cols: 80, rows: 24, ..Default::default() });
    c.wait_for("catch-up", |c| c.gens.len() > 1 && is_screen(c.gens.last().unwrap()));
    let mut seq = 0;
    let mut lat = Vec::new();
    for i in 0..30 {
        let token = format!("e{i:03}x");
        seq += 1;
        let typed = Instant::now();
        c.text_input(seq, &token);
        c.wait_text(&format!("> {token}"));
        lat.push(typed.elapsed());
        seq += 1;
        c.text_input(seq, "\r");
        // Like the e2e test: Enter's screen is still uncredited when the
        // next token comes.
        c.drain(Duration::from_millis(200));
    }
    check_echo_latency(lat, c.ready_at, LINK, rtt);
}

/// A catch-up screen cut in the middle of an escape sequence (where the
/// compact encoding falls back to the plain one) still continues exactly.
#[test]
fn catch_up_screen_mid_sequence() {
    local(async {
        let s = unit_session(session_cfg(40, 10, 1000));
        for i in 0..300 {
            s.borrow_mut().core.term.write(format!("history {i}\r\n").as_bytes());
        }
        let q = add_conn(&s, 1, "o", Some(unit_stream(1, 1, 1, true)));
        let (head, rest) = ("\x1b[1;31mred\x1b]2;tit", "le\x1b\\ tail\x1b[0m");
        for cut in ["", head] {
            q.borrow_mut().drop_data();
            s.borrow_mut().handle_output(cut.as_bytes());
            {
                let mut b = s.borrow_mut();
                let (core, c) = b.split(1).unwrap();
                core.send_snapshot(c, false, "lag");
            }
            let data = queued_data(&q);
            let mut dec = ghostty::Decoder::new().unwrap();
            dec.feed(&data);
            let mut got = dec.ready(ghostty::Callbacks::default()).unwrap_or_else(|e| panic!("cut {cut:?}: {e}"));
            got.write(rest.as_bytes());
            let want = s.borrow().core.term.snapshot_screen().unwrap();
            let mut wdec = ghostty::Decoder::new().unwrap();
            wdec.feed(&want);
            let mut wterm = wdec.ready(ghostty::Callbacks::default()).unwrap();
            wterm.write(rest.as_bytes());
            assert_eq!(screen_state(&mut got), screen_state(&mut wterm), "cut {cut:?}");
        }
    });
}
