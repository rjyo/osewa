//! Ported from holder_test.go.

use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::helpers::*;
use super::main::{holder_command, Proc};
use crate::{socket_path, Config};

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
}

fn own() -> wire::Request {
    wire::Request { own: true, ..Default::default() }
}

fn method(m: &str) -> wire::Request {
    wire::Request { method: m.into(), ..Default::default() }
}

#[test]
fn attach_output() {
    let h = start(cfg(sh("printf hello; sleep 30")));
    let mut c = h.dial("dev1");
    let resp = c.attach_size(50, 12, true);
    assert!(!resp.epoch.is_empty() && resp.owner == "dev1" && !resp.resumed, "{resp:?}");
    c.wait_text("hello");
    let head = resp.head.expect("attach without head");
    assert_eq!(c.gens.len(), 1);
    assert_eq!((c.gens[0].generation, c.gens[0].epoch.as_str(), c.gens[0].position), (0, resp.epoch.as_str(), head));
    let info = c.info();
    assert_eq!((info.id.as_str(), info.cols, info.rows, info.owner.as_str()), ("s1", 50, 12, "dev1"));
    assert!(!info.exited && info.created != 0);
    assert_eq!(c.term.as_ref().unwrap().size(), (50, 12));
    assert!(c.event_kinds().iter().any(|k| k == wire::EVENT_RESIZE) || c.gens[0].cols == 50, "no resize to the owner's size");
}

#[test]
fn bad_requests() {
    let h = start(cfg(sh("sleep 30")));
    let mut c = h.dial("dev1");
    let req = method;
    let cases = [
        (req("nope"), wire::ERR_BAD_REQUEST),
        (wire::Request { stream: 1, session: "other".into(), ..req(wire::METHOD_ATTACH) }, wire::ERR_NOT_FOUND),
        (req(wire::METHOD_ATTACH), wire::ERR_BAD_REQUEST),
        (wire::Request { stream: 1, epoch: "old".into(), ..req(wire::METHOD_ATTACH) }, wire::ERR_ENDED),
        (wire::Request { stream: 1, ..req(wire::METHOD_OWN) }, wire::ERR_BAD_REQUEST),
        (wire::Request { stream: 1, cols: 10, rows: 10, ..req(wire::METHOD_RESIZE) }, wire::ERR_BAD_REQUEST),
        (wire::Request { session: "other".into(), ..req(wire::METHOD_KILL) }, wire::ERR_NOT_FOUND),
    ];
    for (r, code) in cases {
        let resp = c.request(r.clone());
        assert_eq!(resp.error, code, "{r:?}");
    }
    c.send(&wire::Frame::new(wire::TYPE_REQUEST, 0, b"{".to_vec()));
    c.until(|f| f.ty == wire::TYPE_RESPONSE);
    c.attach(wire::Request::default());
    let resp = c.request(wire::Request { stream: 7, ..req(wire::METHOD_RESIZE) });
    assert_eq!(resp.error, wire::ERR_BAD_REQUEST, "zero resize");
    let resp = c.request(wire::Request { stream: 7, ..req(wire::METHOD_DETACH) });
    assert!(resp.error.is_empty(), "detach: {resp:?}");
    c.wait_for("stream end", |c| c.ended.is_some());
    assert_eq!(c.ended.as_ref().unwrap().reason, wire::END_DETACHED);
}

#[test]
fn resume_from_position() {
    let h = start(cfg(sh("for i in 1 2 3 4 5 6 7 8 9 10; do echo line$i; sleep 0.05; done; sleep 30")));
    let mut c = h.dial("dev1");
    let resp = c.attach(own());
    c.wait_text("line3");
    let (pos, before, prefix) = (c.end, c.raw.clone(), c.gens[0].position);
    c.close();
    std::thread::sleep(Duration::from_millis(200));

    let mut c2 = h.dial("dev1");
    let r2 = c2.attach(wire::Request { epoch: resp.epoch.clone(), position: Some(pos), ..Default::default() });
    assert!(r2.resumed, "{r2:?}");
    c2.end = pos;
    // A resumed client keeps its terminal; build one from what c had.
    c2.term = Some(new_ref(40, 10));
    c2.wait_for("line10", |c| String::from_utf8_lossy(&c.raw).contains("line10"));
    assert!(c2.gens.is_empty(), "resume sent a snapshot");
    assert_eq!((c.gaps, c2.gaps), (0, 0), "gaps before, after");
    assert_eq!(c2.starts[0], pos, "resume starts");
    let all = format!("{}{}", String::from_utf8_lossy(&before), String::from_utf8_lossy(&c2.raw));
    for i in 1..=10 {
        // Output before c's snapshot is in the snapshot, not in raw.
        let n = all.matches(&format!("line{i}\r\n")).count();
        assert!(n == 1 || (n == 0 && prefix > 0), "line{i} seen {n} times in {all:?}");
    }
}

#[test]
fn resume_evicted_gets_snapshot() {
    let h = start(Config { log_bytes: 4096, ..cfg(sh("seq 1 3000; sleep 30")) });
    let mut c = h.dial("dev1");
    let resp = c.attach(own());
    c.wait_text("3000");

    let mut c2 = h.dial("dev1");
    let r2 = c2.attach(wire::Request { epoch: resp.epoch.clone(), position: Some(0), ..Default::default() });
    assert!(!r2.resumed, "resumed from an evicted position");
    c2.wait_for("snapshot", |c| c.gens.len() == 1 && c.term.is_some());
    // The snapshot covers all c saw; events logged since (an input_mode
    // poll, say) may put the log end further.
    assert!(
        c2.text().contains("3000") && c2.gens[0].position >= c.end,
        "snapshot at {}, before {} that c saw: {:?}",
        c2.gens[0].position,
        c.end,
        c2.text()
    );

    // A position past the end, or not on a record boundary, also snapshots.
    for p in [c.end + 1, c.end - 1] {
        let mut c3 = h.dial("dev1");
        let r = c3.attach(wire::Request { epoch: resp.epoch.clone(), position: Some(p), ..Default::default() });
        assert!(!r.resumed, "resumed from {p}");
    }
}

#[test]
fn snapshot_mid_sequence() {
    let dir = TempDir::new();
    let p1 = payload(&dir, "p1", b"A\x1b[1;3");
    let p2 = payload(&dir, "p2", b"1mRED\x1b[0m\x1b]2;ti");
    let p3 = payload(&dir, "p3", b"tle\x1b\\B\x1b[?1049hALT");
    let h = start(Config {
        run_dir: dir.0.join("run"),
        ..cfg(sh(&format!("stty -echo; read a; cat {p1}; read b; cat {p2}; read c; cat {p3}; sleep 30")))
    });

    let mut c1 = h.dial("dev1");
    c1.attach(wire::Request { own: true, cols: 40, rows: 10, ..Default::default() });
    c1.wait_for("first snapshot", |c| c.term.is_some());
    std::thread::sleep(Duration::from_millis(200)); // let stty run before input arrives
    let mut seq = 0;
    let mut step = |c1: &mut Client, want: &[u8]| {
        seq += 1;
        c1.text_input(seq, "\r");
        c1.wait_for(&format!("raw {:?}", String::from_utf8_lossy(want)), |c| c.raw.ends_with(want));
    };
    step(&mut c1, b"A\x1b[1;3");
    let mut c2 = h.dial("dev2");
    c2.attach(wire::Request::default());
    c2.wait_for("snapshot", |c| c.term.is_some());
    step(&mut c1, b"\x1b]2;ti");
    let mut c3 = h.dial("dev3");
    c3.attach(wire::Request::default());
    c3.wait_for("snapshot", |c| c.term.is_some());
    step(&mut c1, b"ALT");
    c2.wait_text("ALT");
    c3.wait_text("ALT");

    let mut r = new_ref(40, 10);
    if c1.gens[0].position == 0 {
        r.write(&c1.raw);
    } else {
        r.write(b"A\x1b[1;31mRED\x1b[0m\x1b]2;title\x1b\\B\x1b[?1049hALT");
    }
    let want = state(&r);
    for (i, c) in [&c1, &c2, &c3].iter().enumerate() {
        let got = state(c.term.as_ref().unwrap());
        assert_eq!(got, want, "client {}", i + 1);
    }
    let t2 = c2.term.as_ref().unwrap();
    assert!(t2.title() == "title" && t2.alt_screen(), "c2 title {:?} alt {}", t2.title(), t2.alt_screen());
}

#[test]
fn input_dedupe_across_reconnect() {
    let h = start(cfg(sh(r#"stty -echo; echo READY; while read l; do echo "L:$l"; done"#)));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("READY");
    c.text_input(1, "one\r");
    c.text_input(2, "two\r");
    c.wait_for("ack 2", |c| c.acked == 2);
    c.close();

    let mut c2 = h.dial("dev1");
    c2.attach(own());
    c2.text_input(1, "one\r");
    c2.wait_for("ack", |c| c.acked != 0);
    assert_eq!(c2.acked, 2, "ack for a duplicate");
    c2.text_input(2, "two\r");
    c2.text_input(3, "three\r");
    c2.wait_for("ack 3", |c| c.acked == 3);
    c2.wait_text("L:three");
    let text = c2.text();
    for l in ["L:one", "L:two", "L:three"] {
        assert_eq!(text.matches(l).count(), 1, "{l} in:\n{text}");
    }
}

#[test]
fn non_owner_input_dropped() {
    let h = start(cfg(sh(r#"stty -echo; echo READY; while read l; do echo "L:$l"; done"#)));
    let mut a = h.dial("a");
    a.attach(own());
    let mut b = h.dial("b");
    b.attach(wire::Request::default());
    b.wait_text("READY");

    b.text_input(1, "bad\r");
    b.wait_for("ack", |c| c.acked == 1);
    a.text_input(1, "good\r");
    a.wait_text("L:good");
    assert!(!a.text().contains("L:bad"), "non-owner input applied:\n{}", a.text());
    let resp = b.request(wire::Request { stream: 7, cols: 20, rows: 5, ..method(wire::METHOD_RESIZE) });
    assert_eq!(resp.error, wire::ERR_NOT_OWNER, "non-owner resize: {resp:?}");

    // b takes over, at its own size, and its input applies.
    let resp = b.request(wire::Request { stream: 7, ..method(wire::METHOD_OWN) });
    assert_eq!(resp.owner, "b", "own: {resp:?}");
    b.text_input(2, "mine\r");
    b.wait_text("L:mine");
    a.text_input(2, "late\r");
    a.wait_for("ack", |c| c.acked == 2);
    let resp = b.request(wire::Request { stream: 7, cols: 30, rows: 8, ..method(wire::METHOD_RESIZE) });
    assert!(resp.error.is_empty(), "owner resize: {resp:?}");
    a.wait_for("resize event", |c| c.has_event(|e| e.kind == wire::EVENT_RESIZE && e.cols == 30));
    assert!(a.has_event(|e| e.kind == wire::EVENT_OWNER && e.owner == "b"), "events = {:?}", a.events);
    let info = a.info();
    assert!(info.cols == 30 && info.rows == 8 && info.owner == "b", "info = {info:?}");
    b.wait_text("L:mine");
    assert!(!b.text().contains("L:late"), "former owner's input applied");
}

#[test]
fn key_paste_focus_input() {
    // The child prints what it reads as one run of hex digits after "H:",
    // so neither od's lines nor the terminal's wrapping can split it.
    let h = start(cfg(sh(r#"stty raw -echo; printf '\033[?1004h\033[?2004hGO'; h=$(dd bs=1 count=19 2>/dev/null | od -An -v -tx1 | tr -d ' \n'); printf 'H:%s:END' "$h"; sleep 30"#)));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("GO");
    let key = |seq, key: &str, mods: &[&str], text: &str, action: &str| wire::Input {
        seq,
        ty: "key".into(),
        key: key.into(),
        mods: mods.iter().map(|m| m.to_string()).collect(),
        text: text.into(),
        action: action.into(),
        ..Default::default()
    };
    c.input(key(1, "KeyA", &[], "a", ""));
    c.input(key(2, "KeyB", &["ctrl"], "", "repeat"));
    c.input(key(3, "KeyA", &[], "", "release"));
    c.input(key(4, "NoSuchKey", &[], "", ""));
    c.input(key(5, "KeyA", &["hyper"], "", ""));
    c.input(wire::Input { seq: 6, ty: "paste".into(), text: "pp".into(), ..Default::default() });
    c.input(wire::Input { seq: 7, ty: "focus".into(), focused: true, ..Default::default() });
    c.wait_for("ack 7", |c| c.acked == 7);
    c.wait_for("the bytes read", |c| unwrapped(&c.text()).contains(":END"));
    // a, ^B, the bracketed paste and the focus report, in that order;
    // release, the unknown key and the bad modifier sent nothing.
    let want = format!("H:6102{}{}:END", hex::encode(b"\x1b[200~pp\x1b[201~"), hex::encode(b"\x1b[I"));
    assert!(unwrapped(&c.text()).contains(&want), "read {:?}, want {want:?}", unwrapped(&c.text()));
}

/// Joins the screen's lines, undoing soft wrapping.
fn unwrapped(s: &str) -> String {
    s.replace('\n', "")
}

#[test]
fn dsr_answered_without_client() {
    let h = start(cfg(sh(r#"stty raw -echo; printf '\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); printf 'GOT%s\r\n' "$r" | tr '\033' E; sleep 30"#)));
    std::thread::sleep(Duration::from_millis(500));
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    c.wait_text("GOTE[1;1R");
}

#[test]
fn stdin_flood_does_not_stall() {
    let _heavy = heavy(1);
    let h = start(cfg(sh("stty -echo; while :; do echo tick; sleep 0.05; done")));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("tick");

    const FRAMES: u64 = 200;
    let w = c.w.clone();
    let (sent_tx, sent) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let big = "x".repeat(4096);
        for i in 1..=FRAMES {
            let input = wire::Input { seq: i, ty: "text".into(), text: big.clone(), ..Default::default() };
            if wire::write(&mut *w.lock().unwrap(), &wire::json(wire::TYPE_INPUT, 7, &input)).is_err() {
                let _ = sent_tx.send(false);
                return;
            }
        }
        let _ = sent_tx.send(true);
    });

    // Output keeps flowing and the holder keeps answering while the child
    // ignores stdin.
    for _ in 0..5 {
        let n = c.raw.len();
        c.wait_for("more ticks", |c| String::from_utf8_lossy(&c.raw[n..]).matches("tick").count() >= 2);
    }
    let mut other = h.dial("dev2");
    let start = Instant::now();
    other.info();
    let d = start.elapsed();
    assert!(d <= Duration::from_secs(1), "info took {d:?}");
    // Linux PTYs block once full, so acks stop; macOS discards input past
    // its tty queue, so the cap itself is covered by pty_writer_cap.
    eprintln!("acked {} of {FRAMES}", c.acked);
    if sent.try_recv() == Ok(true) {
        // The sender may still be blocked; either way nothing deadlocked.
        eprintln!("all input written to the socket");
    }
    let resp = other.request(method(wire::METHOD_KILL));
    assert!(resp.error.is_empty(), "kill: {resp:?}");
    c.wait_for("stream end", |c| c.ended.is_some());
}

#[test]
fn lag_sends_fresh_snapshot() {
    let _heavy = heavy(1);
    let h = start(Config { lag_bytes: 64 << 10, ..cfg(sh("read go; seq 1 200000; echo END; sleep 30")) });
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_for("snapshot", |c| c.term.is_some());
    c.text_input(1, "\r");
    // Stop reading while the child floods.
    std::thread::sleep(Duration::from_secs(1));
    c.wait_text("END");
    assert!(c.gens.len() >= 2, "generations = {}, want a lag snapshot", c.gens.len());
    // A lag snapshot dropped before it went out leaves its generation
    // unseen, so generations only increase.
    for (i, g) in c.gens.iter().enumerate() {
        let bad = (i == 0 && g.generation != 0) || (i > 0 && g.generation <= c.gens[i - 1].generation);
        assert!(!bad, "generations {:?}", c.gens);
    }
    for i in 1..c.gens.len() {
        // Not two lag snapshots closer than the interval, as far as their
        // positions show: each one covers fresh output.
        assert!(
            c.gens[i].position > c.gens[i - 1].position,
            "snapshot positions {} then {}",
            c.gens[i - 1].position,
            c.gens[i].position
        );
    }
    assert!(c.text().contains("200000"), "text:\n{}", c.text());
}

#[test]
fn exit_code() {
    let tune = crate::Tunables { drain_wait: Duration::from_millis(200), ..Default::default() };
    let mut h = start(Config { exit_retention: Duration::from_millis(500), tune, ..cfg(sh("read x; echo bye; exit 7")) });
    let mut c = h.dial("dev1");
    c.attach(own());
    c.text_input(1, "\r");
    c.wait_for("stream end", |c| c.ended.is_some());
    let end = c.ended.clone().unwrap();
    assert!(end.reason == wire::END_EXITED && end.exit_code == Some(7), "end = {end:?}");
    let ev = c.events.iter().find(|e| e.kind == wire::EVENT_EXIT);
    assert!(ev.is_some_and(|e| e.code == Some(7)), "events = {:?}", c.events);
    assert!(c.text().contains("bye"), "text = {:?}", c.text());

    // The final screen stays readable until retention ends.
    let mut c2 = h.dial("dev2");
    let info = c2.info();
    assert!(info.exited && info.exit_code == 7, "info = {info:?}");
    c2.attach(own());
    c2.wait_for("stream end", |c| c.ended.is_some());
    assert!(c2.text().contains("bye") && c2.ended.as_ref().unwrap().exit_code == Some(7), "after exit: {:?} {:?}", c2.text(), c2.ended);
    let resp = c2.request(wire::Request { stream: 7, ..method(wire::METHOD_OWN) });
    assert_eq!(resp.error, wire::ERR_BAD_REQUEST, "own after end: {resp:?}");
    let resp = c2.request(method(wire::METHOD_KILL));
    assert!(resp.error.is_empty(), "kill after exit: {resp:?}");

    h.done(Duration::from_secs(5)).unwrap();
    assert!(!h.sock.exists(), "socket left behind");
}

#[test]
fn kill() {
    // trap '' HUP makes the shell survive SIGHUP, so SIGKILL must follow.
    let tune = crate::Tunables { kill_grace: Duration::from_millis(300), ..Default::default() };
    let mut h = start(Config {
        exit_retention: Duration::from_millis(300),
        tune,
        ..cfg(sh("trap '' HUP; echo up; while :; do sleep 0.1; done"))
    });
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("up");
    let mut k = h.dial("");
    let resp = k.request(wire::Request { session: "s1".into(), ..method(wire::METHOD_KILL) });
    assert!(resp.error.is_empty(), "kill: {resp:?}");
    k.request(method(wire::METHOD_KILL));
    c.wait_for("stream end", |c| c.ended.is_some());
    let end = c.ended.clone().unwrap();
    assert!(end.reason == wire::END_KILLED && end.exit_code.is_some_and(|c| c != 0), "end = {end:?}");
    h.done(Duration::from_secs(5)).unwrap();
}

#[test]
fn ownership_lapse() {
    let h = start(Config { owner_grace: Duration::from_millis(300), ..cfg(sh("sleep 30")) });
    let mut watch = h.dial("watcher");
    watch.attach(wire::Request::default());

    let mut a = h.dial("a");
    a.attach(own());
    a.close();
    std::thread::sleep(Duration::from_millis(100));
    let info = watch.info();
    assert_eq!(info.owner, "a", "owner lapsed early: {info:?}");
    // Coming back within the grace keeps ownership, even without own.
    let mut a2 = h.dial("a");
    a2.attach(wire::Request::default());
    std::thread::sleep(Duration::from_millis(500));
    let info = watch.info();
    assert_eq!(info.owner, "a", "owner lapsed while attached: {info:?}");
    a2.request(method(wire::METHOD_DETACH));
    watch.wait_for("owner event", |c| c.has_event(|e| e.kind == wire::EVENT_OWNER && e.owner.is_empty()));
    assert_eq!(watch.info().owner, "");
}

#[test]
fn events() {
    let dir = TempDir::new();
    let p = payload(
        &dir,
        "ev",
        b"\x07\x1b]2;work\x1b\\\x1b]52;c;aGVsbG8=\x1b\\\x1b]7501;state=working:app=claude-code:msg=QXBwbHk/\x07DONE",
    );
    let h = start(Config { run_dir: dir.0.join("run"), ..cfg(sh(&format!("stty -echo; read x; cat {p}; sleep 30"))) });
    let mut owner = h.dial("o");
    owner.attach(own());
    let mut other = h.dial("x");
    other.attach(wire::Request::default());
    std::thread::sleep(Duration::from_millis(200));
    owner.text_input(1, "\r");
    owner.wait_text("DONE");
    other.wait_text("DONE");
    // Events follow the output that raised them.
    other.wait_for("status", |c| c.has_event(|e| e.kind == wire::EVENT_STATUS));
    owner.wait_for("status", |c| c.has_event(|e| e.kind == wire::EVENT_STATUS));

    let mut clip = Vec::new();
    for e in &owner.events {
        if e.kind == wire::EVENT_CLIPBOARD {
            clip = e.data.clone();
        }
        assert!(e.kind != wire::EVENT_TITLE || e.title == "work", "title event {e:?}");
        assert!(
            e.kind != wire::EVENT_STATUS || (e.state == "working" && e.app == "claude-code" && e.msg == "Apply?"),
            "status event {e:?}"
        );
    }
    assert_eq!(clip, b"hello", "owner events {:?}", owner.events);
    for k in [wire::EVENT_BELL, wire::EVENT_TITLE] {
        assert!(other.event_kinds().iter().any(|e| e == k), "other missing {k}: {:?}", other.event_kinds());
    }
    assert!(!other.event_kinds().iter().any(|e| e == wire::EVENT_CLIPBOARD), "clipboard sent to a non-owner");
    let info = other.info();
    assert_eq!(info.title, "work", "info = {info:?}");
}

#[test]
fn ack_and_credit_accepted() {
    let h = start(cfg(sh("echo hi; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    c.wait_text("hi");
    c.send(&wire::ack(7, 1, c.end));
    c.send(&wire::credit(7, 1 << 20));
    c.send(&wire::Frame::new(wire::TYPE_ACK, 7, vec![1]));
    c.info();
}

#[test]
fn cancel_stops_child() {
    let dir = TempDir::new();
    let run_dir = dir.0.join("run");
    let cfg = Config { run_dir: run_dir.clone(), id: "s1".into(), cols: 20, rows: 5, argv: sh("sleep 30"), ..Default::default() };
    let (stop, stop_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let c = cfg.clone();
    let done = std::thread::spawn(move || crate::run_until(c, move || ready_tx.send(()).unwrap(), false, Some(stop_rx)));
    ready.recv_timeout(WAIT).unwrap();
    // A second holder for the same id refuses a live socket.
    let second = Config { argv: sh("true"), ..cfg.clone() };
    assert!(crate::run(second, || {}, false).is_err(), "second holder started");
    stop.send(()).unwrap();
    assert!(eventually(WAIT, || done.is_finished()), "serve did not stop");
    let r = done.join().unwrap();
    assert!(r.is_ok(), "serve = {r:?}");
    // A stale socket file is replaced.
    std::fs::write(socket_path(&run_dir, "s1"), b"").unwrap();
    let (stop, stop_rx) = tokio::sync::oneshot::channel();
    let r = crate::run_until(cfg, move || stop.send(()).unwrap(), false, Some(stop_rx));
    assert!(r.is_ok(), "serve over stale socket = {r:?}");
}

#[test]
fn config_errors() {
    let dir = TempDir::new();
    let d = dir.0.clone();
    let c = |id: &str, run_dir: &std::path::Path, cols, argv: &[&str]| Config {
        run_dir: run_dir.into(),
        id: id.into(),
        cols,
        rows: 1,
        argv: argv.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let none = std::path::Path::new("");
    for cfg in [
        c("", &d, 1, &["true"]),
        c("a/b", &d, 1, &["true"]),
        c("a", none, 1, &["true"]),
        c("a", &d, 1, &[]),
        c("a", &d, 0, &["true"]),
        c("a", &d, 1, &["/nonexistent/osewa"]),
    ] {
        assert!(crate::run(cfg.clone(), || {}, false).is_err(), "{cfg:?}: no error");
    }
}

#[test]
fn run() {
    let dir = TempDir::new();
    let d = dir.0.to_str().unwrap();
    let mut p = Proc::spawn(holder_command(&[
        "--run", d, "--id", "m1", "--cols", "30", "--rows", "6", "--cwd", d, "--", "/bin/sh", "-c", "pwd; sleep 30",
    ]));
    let line = p.line();
    assert_eq!(line, "ready\n", "stdout");
    let mut c = Client::dial(&socket_path(&dir.0, "m1"), "dev1");
    c.attach(wire::Request::default());
    let real = std::fs::canonicalize(&dir.0).unwrap().to_str().unwrap().to_string();
    let d2 = d.to_string();
    c.wait_for("cwd", move |c| c.text().contains(&d2) || c.text().contains(&real) || c.text().contains("/oh"));
    let info = c.info();
    assert!(info.cols == 30 && info.rows == 6, "info = {info:?}");
    // Go's test cancels Serve's context; a process gets SIGTERM.
    p.signal(libc::SIGTERM);
    assert_eq!(p.wait_exit(WAIT), 0, "code");

    let mut bad = Proc::spawn(holder_command(&["--bogus"]));
    assert_eq!(bad.wait_exit(WAIT), 2, "bad flag");
    let mut no_argv = Proc::spawn(holder_command(&["--run", d, "--id", "m2"]));
    assert_eq!(no_argv.wait_exit(WAIT), 1, "no argv");
}

#[test]
fn attach_head_and_input_ack() {
    let h = start(cfg(sh(r#"stty -echo; echo READY; while read l; do echo "L:$l"; done"#)));
    let mut c = h.dial("dev1");
    let resp = c.attach(own());
    c.want_input_ack(0);
    c.wait_for("snapshot", |c| c.gens.len() == 1);
    assert!(resp.head == Some(c.gens[0].position), "head {:?}, snapshot at {}", resp.head, c.gens[0].position);
    c.wait_text("READY");
    for (i, l) in ["one", "two", "three"].iter().enumerate() {
        c.text_input(i as u64 + 1, &format!("{l}\r"));
    }
    c.wait_for("ack 3", |c| c.acked == 3);
    c.wait_text("L:three");
    let pos = c.end;
    c.close();

    // A resume reports the head and the seq to continue above.
    let mut c2 = h.dial("dev1");
    let r2 = c2.attach(wire::Request { epoch: resp.epoch.clone(), position: Some(pos), ..Default::default() });
    c2.want_input_ack(3);
    assert!(r2.resumed && r2.head.is_some_and(|h| h >= pos), "resume = {r2:?}, from {pos}");

    // Another client id starts at 0.
    let mut c3 = h.dial("dev2");
    let r3 = c3.attach(wire::Request::default());
    c3.want_input_ack(0);
    assert!(r3.head.is_some_and(|h| h >= r2.head.unwrap()), "head went back: {:?} after {:?}", r3.head, r2.head);
}

/// Not in the Go tests: a holder for a live session refuses to start.
#[test]
fn stale_socket_replaced_live_refused() {
    let h = start(cfg(sh("sleep 30")));
    let cfg2 = h.cfg.clone();
    let err = crate::run(cfg2, || {}, false).unwrap_err();
    assert!(err.contains("already served"), "{err}");
}
