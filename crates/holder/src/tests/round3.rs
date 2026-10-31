//! Under load: floods, W2 screens, echo floods. The generators and links
//! live in main.rs and helpers.rs.

use std::cell::Cell;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::helpers::*;
use super::main::{echo_flood_argv, self_child};
use crate::pty::InputMode;
use crate::Config;

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
}

fn own() -> wire::Request {
    wire::Request { own: true, ..Default::default() }
}

fn mode(echo: bool, canonical: bool) -> InputMode {
    InputMode { echo, canonical }
}

pub fn mode_events(c: &Client) -> Vec<InputMode> {
    c.events
        .iter()
        .filter(|e| e.kind == wire::EVENT_INPUT_MODE)
        .map(|e| mode(e.echo.unwrap(), e.canonical.unwrap()))
        .collect()
}

pub fn last_mode(c: &Client) -> Option<InputMode> {
    mode_events(c).last().copied()
}

#[test]
fn input_mode_events() {
    let h = start(cfg(sh("echo A; read x; stty -echo; echo B; read y; stty -icanon; echo C; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("A");
    c.wait_for("mode at attach", |c| last_mode(c) == Some(mode(true, true)));

    c.text_input(1, "\r");
    c.wait_text("B");
    c.wait_for("echo off", |c| last_mode(c) == Some(mode(false, true)));
    c.text_input(2, "\r");
    c.wait_text("C");
    c.wait_for("raw", |c| last_mode(c) == Some(mode(false, false)));
    // Only changes are logged while attached: no repeats.
    let ms = mode_events(&c);
    assert!(ms.windows(2).all(|w| w[0] != w[1]), "repeated mode event: {ms:?}");

    // A new stream learns the mode right after its snapshot.
    let mut c2 = h.dial("dev2");
    c2.attach(wire::Request::default());
    c2.wait_for("mode", |c| last_mode(c).is_some());
    let m = last_mode(&c2);
    assert!(m == Some(mode(false, false)) && c2.gens.len() == 1, "second client mode {m:?}");
}

/// A mode change with no output after it is still seen, by polling.
#[test]
fn input_mode_polled() {
    let h = start(cfg(sh("read x; stty -echo; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_for("mode at attach", |c| last_mode(c).is_some());
    c.text_input(1, "\r");
    // The echoed newline may come before stty runs; the poll catches up.
    c.wait_for("echo off", |c| last_mode(c).is_some_and(|m| !m.echo));
}

#[test]
fn idle_ends_session() {
    // Idleness counts from the session's start, before start returns.
    let started = Instant::now();
    let mut h = start(Config {
        idle: Duration::from_millis(300),
        exit_retention: Duration::from_millis(100),
        ..cfg(sh("sleep 30"))
    });
    h.done(Duration::from_secs(5)).unwrap();
    let d = started.elapsed();
    assert!(d >= Duration::from_millis(300), "ended after {d:?}");
}

/// An attached stream, or output, keeps a session from idling; idleness
/// counts from the last of either.
#[test]
fn idle_kept_alive() {
    let mut h = start(Config {
        idle: Duration::from_millis(400),
        exit_retention: Duration::from_millis(100),
        ..cfg(sh("sleep 30"))
    });
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    std::thread::sleep(Duration::from_millis(800));
    let info = c.info();
    assert!(!info.exited && info.attached == 1 && info.idle_seconds == 0, "attached session: {info:?}");
    c.request(wire::Request { method: wire::METHOD_DETACH.into(), ..Default::default() });
    std::thread::sleep(Duration::from_millis(200));
    let info = c.info();
    assert!(!info.exited && info.attached == 0, "just detached: {info:?}");
    h.done(Duration::from_secs(5)).unwrap();

    let h2 = start(Config { idle: Duration::from_millis(300), ..cfg(sh("while :; do echo x; sleep 0.05; done")) });
    std::thread::sleep(Duration::from_millis(900));
    let info = h2.dial("").info();
    assert!(!info.exited, "printing session idled: {info:?}");
}

#[test]
fn info_idle_seconds() {
    let h = start(cfg(sh("sleep 30")));
    std::thread::sleep(Duration::from_millis(1100));
    let mut k = h.dial("");
    let info = k.info();
    assert!(info.attached == 0 && info.idle_seconds >= 1, "info {info:?}");
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parse_flags() {
    let c = crate::parse_flags(&args(&["--run", "r", "--id", "i", "--", "sh"])).unwrap();
    assert!(
        c.idle == crate::DEFAULT_IDLE && c.scrollback_bytes == crate::DEFAULT_SCROLLBACK_BYTES && c.argv == ["sh"],
        "{c:?}"
    );
    let c = crate::parse_flags(&args(&["--idle-timeout", "0", "--", "sh"])).unwrap();
    assert!(c.idle.is_zero(), "{c:?}");
    assert!(crate::parse_flags(&args(&["--idle-timeout", "soon"])).is_err(), "bad duration accepted");

    // Every flag lands in its field.
    let c = crate::parse_flags(&args(&[
        "--run", "/r", "--id", "x", "--cols", "100", "--rows", "30", "--cwd", "/w", "--scrollback-bytes", "5", "--", "sh", "-l",
    ]))
    .unwrap();
    assert_eq!((c.run_dir.to_str().unwrap(), c.id.as_str(), c.cols, c.rows, c.cwd.as_str()), ("/r", "x", 100, 30, "/w"));
    assert_eq!((c.scrollback_bytes, c.argv.as_slice()), (5, &args(&["sh", "-l"])[..]));
    assert!(crate::parse_flags(&args(&["--nope"])).is_err());
}

/// Attaches a client behind a link of the given rate to a session flooding
/// at ten times it, and returns the times between fresh screens
/// (SNAPSHOT_READY of a lag snapshot) over d.
fn ready_gaps(rate: usize, d: Duration) -> Vec<Duration> {
    let h = start(Config {
        cols: 200,
        rows: 60,
        env: vec![format!("OSEWA_TEST_FLOOD={}", 10 * rate)],
        ..cfg(self_child(""))
    });
    let mut nc = UnixStream::connect(&h.sock).unwrap();
    wire::write(&mut nc, &wire::json(wire::TYPE_HELLO, 0, &wire::Hello { client_id: "slow".into(), ..Default::default() }))
        .unwrap();
    let attach = wire::Request { id: 1, method: wire::METHOD_ATTACH.into(), stream: 1, ..Default::default() };
    wire::write(&mut nc, &wire::json(wire::TYPE_REQUEST, 0, &attach)).unwrap();
    // A read timeout, so a holder that stops sending fails the test
    // instead of hanging it.
    nc.set_read_timeout(Some(d + Duration::from_secs(5))).unwrap();
    let mut link = SlowConn::new(nc.try_clone().unwrap(), rate);
    let mut readies = Vec::new();
    let (mut first, mut seen_begin) = (false, false);
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        let f = wire::read(&mut link).unwrap();
        let credit = |nc: &mut UnixStream, n: usize| wire::write(nc, &wire::credit(1, n as u32)).unwrap();
        match f.ty {
            wire::TYPE_SNAPSHOT_BEGIN => {
                first = !seen_begin;
                seen_begin = true;
            }
            // Only the first snapshot's DATA is uncounted.
            wire::TYPE_SNAPSHOT_DATA if !first => credit(&mut nc, f.payload.len()),
            wire::TYPE_SNAPSHOT_HISTORY | wire::TYPE_OUTPUT | wire::TYPE_EVENT => credit(&mut nc, f.payload.len()),
            wire::TYPE_SNAPSHOT_READY => readies.push(Instant::now()),
            _ => {}
        }
    }
    let mut gaps: Vec<Duration> = readies.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.push(readies.last().expect("no screens").elapsed());
    gaps
}

/// Under output at ten times the link rate, fresh screens land at least
/// every 3 s on a 10 Mbit/s link.
#[test]
fn flood_freshness_10mbit() {
    // 12.5 MB/s through the holder is the heaviest load here: it gets the
    // machine nearly to itself.
    let _heavy = heavy(3);
    let gaps = ready_gaps(10_000_000 / 8, Duration::from_secs(5));
    eprintln!("{} screens, max gap {:?}", gaps.len(), gaps.iter().max());
    assert!(gaps.len() >= 3, "only {} fresh screens", gaps.len());
    for (i, g) in gaps.iter().enumerate() {
        assert!(*g <= Duration::from_secs(3), "gap {i}: {g:?}");
    }
}

/// The same on 1 Mbit/s, where the target is every 12 s.
#[test]
fn flood_freshness_1mbit() {
    let _heavy = heavy(2);
    let gaps = ready_gaps(1_000_000 / 8, Duration::from_secs(6));
    eprintln!("{} screens, max gap {:?}", gaps.len(), gaps.iter().max());
    assert!(gaps.len() >= 2, "only {} fresh screens", gaps.len());
    for (i, g) in gaps.iter().enumerate() {
        assert!(*g <= Duration::from_secs(12), "gap {i}: {g:?}");
    }
}

/// Catch-up end to end: a slow reader under a flood gets screen-only
/// snapshots that carry a keystroke's echo, none with history, and one full
/// snapshot once the flood stops.
#[test]
fn catch_up() {
    let _heavy = heavy(1);
    const LINK: usize = 1_000_000 / 8;
    let flood = Duration::from_secs(3);
    let h = start(Config {
        cols: 200,
        rows: 60,
        env: vec![format!("OSEWA_TEST_ECHOFLOOD={},{}", 10 * LINK, flood.as_millis())],
        ..cfg(echo_flood_argv())
    });
    let mut c = h.dial_via("dev1", slow_link(LINK));
    c.attach(wire::Request { own: true, cols: 200, rows: 60, ..Default::default() });
    c.wait_for("first snapshot", |c| c.term.is_some() && c.snap_ends.contains_key(&0));
    c.text_input(1, "go\r");
    c.wait_for("a screen snapshot", |c| c.gens.iter().any(is_screen));

    // The echo shows up in a screen snapshot soon after it's typed.
    let seen_in = Rc::new(Cell::new(None::<usize>));
    let s = seen_in.clone();
    c.on_ready = Some(Box::new(move |c| {
        let i = c.gens.len() - 1;
        if s.get().is_none() && is_screen(&c.gens[i]) && c.text().contains("ECHO:Z") {
            s.set(Some(i));
        }
    }));
    let typed = Instant::now();
    let before = c.gens.len();
    c.text_input(2, "Z\r");
    c.wait_for("echo in a screen snapshot", |_| seen_in.get().is_some());
    let d = typed.elapsed();
    let seen = seen_in.get().unwrap();
    eprintln!("echo in screen {seen} (the {}. after typing) after {d:?}; screens are {} bytes", seen + 1 - before, c.ready_at);
    // The next screen, or the one after if one was already in flight. On
    // 1 Mbit/s each screen of this size takes most of a second.
    assert!(seen <= before + 1 && d <= Duration::from_secs(5), "echo only in screen {seen} of {before}..., after {d:?}");

    // After the flood: a full snapshot with history, then live records.
    c.wait_for("full snapshot", |c| {
        let i = c.gens.len() - 1;
        i > 0 && !is_screen(&c.gens[i]) && c.snap_ends.contains_key(&i) && c.hist_bytes.get(&i).is_some_and(|&n| n > 0)
    });
    for (i, g) in c.gens.iter().enumerate() {
        let hist = c.hist_bytes.get(&i).copied().unwrap_or(0);
        assert!(!is_screen(g) || hist == 0, "screen snapshot {i} carried {hist} history bytes");
    }
    assert!(c.text().contains("ECHO:Z"), "final screen lost the echo");
    c.text_input(3, "Y\r");
    let n = c.gens.len();
    c.wait_text("ECHO:Y");
    assert_eq!(c.gens.len(), n, "caught-up stream got another snapshot instead of records");
}

/// input_mode echo:false is in the log before the prompt printed after it.
#[test]
fn input_mode_before_prompt() {
    let h = start(cfg(sh("read x; stty -echo; printf 'Password: '; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_for("mode at attach", |c| last_mode(c).is_some_and(|m| m.echo));
    c.text_input(1, "\r");
    loop {
        let f = c.next_handled();
        if f.ty != wire::TYPE_OUTPUT || !String::from_utf8_lossy(&f.payload).contains("Password") {
            continue;
        }
        assert!(
            last_mode(&c).is_some_and(|m| !m.echo),
            "prompt arrived before echo:false; modes so far {:?}",
            mode_events(&c)
        );
        break;
    }
}
