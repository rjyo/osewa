//! The holder's requests: attach, output, resume, bad requests.

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

/// The owner's pixel size reaches the terminal, which answers size reports
/// (CSI 14 t) with it and places images by it, and follows the owner's
/// resizes, a change of font size alone included.
#[test]
fn pixel_size() {
    let report = r#"stty raw -echo; printf '\033[14t'; s=; while c=$(dd bs=1 count=1 2>/dev/null); [ "$c" != t ]; do s="$s$c"; done; printf 'R:%s:END' "$(printf %s "$s" | tr -d '\033[')""#;
    let h = start(Config { cols: 80, rows: 24, width: 800, height: 480, ..cfg(sh(&format!("{report}; dd bs=1 count=1 >/dev/null 2>&1; {report}; sleep 30"))) });
    let mut a = h.dial("a");
    a.attach(wire::Request { cols: 80, rows: 24, width: 800, height: 480, ..own() });
    a.wait_text("R:4;480;800:END");
    // Same cells, bigger font.
    let resp = a.request(wire::Request { stream: 7, cols: 80, rows: 24, width: 960, height: 720, ..method(wire::METHOD_RESIZE) });
    assert!(resp.error.is_empty(), "resize: {resp:?}");
    a.wait_for("resize event", |c| c.has_event(|e| e.kind == wire::EVENT_RESIZE && e.width == 960 && e.height == 720));
    a.text_input(1, "x");
    a.wait_text("R:4;720;960:END");
}

/// Notifications reach clients as events. The owner's appearance answers
/// DSR 996 and, once the program asked for changes (mode 2031), reaches it
/// when it changes; a viewer's doesn't count.
#[test]
fn notify_and_color_scheme() {
    let report = r#"s=; while c=$(dd bs=1 count=1 2>/dev/null); [ "$c" != n ]; do s="$s$c"; done; printf 'R:%s:END' "$(printf %s "$s" | tr -d '\033[')""#;
    let h = start(cfg(sh(&format!(
        r#"stty raw -echo; printf '\033]777;notify;CI;all green\033\\GO'; dd bs=1 count=1 >/dev/null 2>&1; printf '\033[?2031h\033[?996n'; {report}; {report}; sleep 30"#
    ))));
    let mut a = h.dial("a");
    a.attach(own());
    a.wait_text("GO");
    a.wait_for("notify event", |c| c.has_event(|e| e.kind == wire::EVENT_NOTIFY && e.title == "CI" && e.msg == "all green"));
    let scheme = |s: &str| wire::Request { stream: 7, color_scheme: s.into(), ..method(wire::METHOD_COLOR_SCHEME) };
    assert_eq!(a.request(scheme("blue")).error, wire::ERR_BAD_REQUEST);
    let mut b = h.dial("b");
    b.attach(wire::Request { stream: 7, cols: 80, rows: 24, ..method(wire::METHOD_ATTACH) });
    assert_eq!(b.request(scheme("light")).error, wire::ERR_NOT_OWNER);
    assert!(a.request(scheme("dark")).error.is_empty());
    a.text_input(1, "x");
    a.wait_text("R:?997;1:END");
    // Unchanged: nothing to report. Changed: reported unasked.
    assert!(a.request(scheme("dark")).error.is_empty());
    assert!(a.request(scheme("light")).error.is_empty());
    a.wait_text("R:?997;2:END");
    assert_eq!(unwrapped(&a.text()).matches("R:").count(), 2, "{:?}", a.text());
}

/// The owner's terminal colors answer programs' color queries; a viewer's
/// and bad ones don't count.
#[test]
fn colors_answer_queries() {
    let h = start(cfg(sh(
        r#"stty raw -echo; printf GO; dd bs=1 count=1 >/dev/null 2>&1; printf '\033]11;?\033\\\033]4;5;?\033\\'; s=; n=0; while [ $n -lt 2 ]; do c=$(dd bs=1 count=1 2>/dev/null); [ "$c" = '\' ] && n=$((n+1)); s="$s$c"; done; printf 'R:%s:END' "$(printf %s "$s" | tr -d '\033\\')"; sleep 30"#,
    )));
    let mut a = h.dial("a");
    a.attach(own());
    a.wait_text("GO");
    let colors = |bg: &str, p5: &str| {
        let mut palette = vec![String::new(); 256];
        palette[5] = p5.into();
        wire::Request { stream: 7, background: bg.into(), palette, ..method(wire::METHOD_COLORS) }
    };
    assert_eq!(a.request(colors("blue", "")).error, wire::ERR_BAD_REQUEST);
    assert_eq!(a.request(wire::Request { palette: vec!["#000000".into()], ..colors("", "") }).error, wire::ERR_BAD_REQUEST);
    let mut b = h.dial("b");
    b.attach(wire::Request { stream: 7, cols: 80, rows: 24, ..method(wire::METHOD_ATTACH) });
    assert_eq!(b.request(colors("#ffffff", "")).error, wire::ERR_NOT_OWNER);
    assert!(a.request(colors("#102030", "#abcdef")).error.is_empty());
    // Unsaid: stays.
    assert!(a.request(colors("", "")).error.is_empty());
    a.text_input(1, "x");
    a.wait_for("the replies", |c| unwrapped(&c.text()).contains("R:]11;rgb:1010/2020/3030]4;5;rgb:abab/cdcd/efef:END"));
}

/// An OSC 52 read goes to the owner alone, and its answer to the program;
/// an answer with no read waiting goes nowhere.
#[test]
fn clipboard_reads() {
    let h = start(cfg(sh(
        r#"stty raw -echo; printf GO; dd bs=1 count=1 >/dev/null 2>&1; printf '\033]52;c;?\033\\'; s=; while c=$(dd bs=1 count=1 2>/dev/null); [ "$c" != '\' ]; do s="$s$c"; done; printf 'R:%s:END' "$(printf %s "$s" | tr -d '\033')"; sleep 30"#,
    )));
    let mut a = h.dial("a");
    a.attach(own());
    a.wait_text("GO");
    let clip = |d: &str| wire::Request { stream: 7, data: d.as_bytes().to_vec(), ..method(wire::METHOD_CLIPBOARD) };
    assert!(a.request(clip("early")).error.is_empty(), "with nothing to answer");
    let mut b = h.dial("b");
    b.attach(wire::Request { stream: 7, cols: 80, rows: 24, ..method(wire::METHOD_ATTACH) });
    a.text_input(1, "x");
    a.wait_for("clipboard_read event", |c| c.has_event(|e| e.kind == wire::EVENT_CLIPBOARD_READ && e.selection == "c"));
    assert_eq!(b.request(clip("viewer")).error, wire::ERR_NOT_OWNER);
    assert!(!b.has_event(|e| e.kind == wire::EVENT_CLIPBOARD_READ), "a viewer saw the read");
    assert!(a.request(clip("hi!")).error.is_empty());
    a.wait_text("R:]52;c;aGkh:END");
}

/// A Kitty clipboard protocol read waits for the owner's answer too, and
/// gets it in its own protocol, id and all.
#[test]
fn kitty_clipboard_reads() {
    // type=read, id=k, for text/plain.
    let h = start(cfg(sh(
        r#"stty raw -echo; printf GO; dd bs=1 count=1 >/dev/null 2>&1; printf '\033]5522;type=read:id=k;dGV4dC9wbGFpbg==\033\\'; s=; while c=$(dd bs=1 count=1 2>/dev/null); do s="$s$c"; case "$s" in *DONE*) break;; esac; done; printf 'R:%s:END' "$(printf %s "$s" | tr -d '\033\\')"; sleep 30"#,
    )));
    let mut a = h.dial("a");
    a.attach(own());
    a.wait_text("GO");
    a.text_input(1, "x");
    a.wait_for("clipboard_read event", |c| c.has_event(|e| e.kind == wire::EVENT_CLIPBOARD_READ && e.selection == "c"));
    assert!(a.request(wire::Request { stream: 7, data: b"hi!".to_vec(), ..method(wire::METHOD_CLIPBOARD) }).error.is_empty());
    let want = "R:]5522;type=read:status=OK:id=k]5522;type=read:status=DATA:id=k:mime=dGV4dC9wbGFpbg==;aGkh]5522;type=read:status=DONE:END";
    a.wait_for("the answer", |c| unwrapped(&c.text()).contains(want));
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
    // A holder process stops on SIGTERM.
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

/// A holder for a live session refuses to start.
#[test]
fn stale_socket_replaced_live_refused() {
    let h = start(cfg(sh("sleep 30")));
    let cfg2 = h.cfg.clone();
    let err = crate::run(cfg2, || {}, false).unwrap_err();
    assert!(err.contains("already served"), "{err}");
}

// A holder started with signals ignored (nohup, a background job) must
// still give its child default dispositions: an ignored signal survives
// exec, and the child would then shrug off Ctrl-C or session.kill.
#[test]
fn child_gets_default_signals() {
    let old = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let h = start(Config { exit_retention: Duration::from_secs(5), ..cfg(sh("sleep 0.2; kill -INT $$; echo alive")) });
    unsafe { libc::signal(libc::SIGINT, old) };
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    c.wait_for("end", |c| c.ended.is_some());
    assert_eq!(c.ended.as_ref().unwrap().exit_code, Some(130), "{}", c.text());
    assert!(!c.text().contains("alive"));
}

#[test]
fn negative_idle_timeout_disables() {
    let args: Vec<String> = ["--id", "x", "--idle-timeout", "-1s"].iter().map(|s| s.to_string()).collect();
    assert!(crate::parse_flags(&args).unwrap().idle.is_zero());
}

// After the session stops, the socket refuses connections at once.
#[test]
fn socket_refuses_after_stop() {
    let mut h = start(Config { exit_retention: Duration::from_millis(200), ..cfg(sh("exit 0")) });
    h.done(Duration::from_secs(5)).unwrap();
    assert!(std::os::unix::net::UnixStream::connect(&h.sock).is_err());
}

/// A session's environment is the account's with the client's env on top,
/// never the holder's (whatever started the server): its nesting guards,
/// tokens, $SHELL and PATH stay out, and it starts in the account's home.
#[test]
fn session_env_is_the_accounts() {
    let h = start(Config { env: vec!["EDITOR=vi".into(), "LANG=C.UTF-8".into()], cols: 120, rows: 40, ..cfg(sh("env | sort | tr '\\n' ' '; echo; echo cwd=$(pwd -P); sleep 30")) });
    let a = osewa_paths::account(h.cfg.home.as_deref());
    let home = a.home.display().to_string();
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("cwd=");
    let t = c.text().replace('\n', "");
    assert_eq!(a.home, osewa_paths::sandbox_home(), "tests run in the sandbox home");
    for want in [format!("HOME={home}"), format!("SHELL={}", a.shell), "EDITOR=vi".into(), "LANG=C.UTF-8".into(), "TERM=xterm-256color".into(), format!("cwd={home}")] {
        assert!(t.contains(&want), "missing {want:?} in {t:?}");
    }
    // This test process's own variables (cargo's, the terminal's) stay out.
    for gone in ["CARGO_PKG_NAME=", "RUST_TEST_THREADS=", "CARGO_MANIFEST_DIR="] {
        assert!(!t.contains(gone), "{gone} leaked: {t:?}");
    }
}

/// The holder CLI: the client's env comes on stdin (--env-stdin), and the
/// session gets none of the holder process's own environment; with no
/// --cwd it starts in the account's home (here OSEWA_HOME's).
#[test]
fn cli_session_env() {
    let dir = TempDir::new();
    let d = dir.0.to_str().unwrap();
    let mut cmd = holder_command(&["--run", d, "--id", "e1", "--cols", "200", "--rows", "20", "--env-stdin", "--", "/bin/sh", "-c", "echo $HERDR_ENV/$FOO/$SHELL/$EDITOR/$OSEWA_HOME/$HOME; pwd -P; sleep 30"]);
    cmd.env("HERDR_ENV", "1").env("FOO", "bar").env("SHELL", "/bin/false").stdin(std::process::Stdio::piped());
    let mut p = Proc::spawn(cmd);
    {
        use std::io::Write;
        let mut stdin = p.child.stdin.take().unwrap();
        stdin.write_all(br#"["EDITOR=vi","OSEWA_HOME=/tmp/no"]"#).unwrap();
    }
    assert_eq!(p.line(), "ready\n", "stdout");
    let mut c = Client::dial(&socket_path(&dir.0, "e1"), "dev1");
    c.attach(wire::Request::default());
    // holder_command gave it the sandbox home (OSEWA_HOME), which the
    // session doesn't see; the client's OSEWA_* is dropped.
    let a = osewa_paths::account(Some(&osewa_paths::sandbox_home()));
    let home = a.home.display().to_string();
    let want = format!("//{}/vi//{home}", a.shell);
    c.wait_for("env", move |c| c.text().contains(&want));
    let real = std::fs::canonicalize(&a.home).unwrap().display().to_string();
    c.wait_for("cwd", move |c| c.text().lines().any(|l| l == real));
    p.signal(libc::SIGTERM);
    assert_eq!(p.wait_exit(WAIT), 0, "code");

    let mut cmd = holder_command(&["--run", d, "--id", "e2", "--env-stdin", "--", "/bin/true"]);
    cmd.stdin(std::process::Stdio::piped());
    let mut bad = Proc::spawn(cmd);
    {
        use std::io::Write;
        bad.child.stdin.take().unwrap().write_all(b"not json").unwrap();
    }
    assert_eq!(bad.wait_exit(WAIT), 2, "a bad --env-stdin");
    let mut cmd = holder_command(&["--run", d, "--id", "e3", "--", "/bin/true"]);
    cmd.env(osewa_paths::HOME_OVERRIDE, "relative/home");
    assert_eq!(Proc::spawn(cmd).wait_exit(WAIT), 2, "a relative OSEWA_HOME");
}

/// --env-stdin takes more than one environment string may hold (Linux's
/// 128 KiB): the whole of what a create request can carry.
#[test]
fn cli_env_stdin_is_large() {
    let dir = TempDir::new();
    let d = dir.0.to_str().unwrap();
    let mut cmd = holder_command(&["--run", d, "--id", "b1", "--env-stdin", "--", "/bin/sh", "-c", "echo len=${#BIG}; sleep 30"]);
    cmd.stdin(std::process::Stdio::piped());
    let mut p = Proc::spawn(cmd);
    let big = "x".repeat(300 << 10);
    {
        use std::io::Write;
        let env = serde_json::to_vec(&[format!("BIG={big}")]).unwrap();
        p.child.stdin.take().unwrap().write_all(&env).unwrap();
    }
    assert_eq!(p.line(), "ready\n", "stdout");
    let mut c = Client::dial(&socket_path(&dir.0, "b1"), "dev1");
    c.attach(wire::Request::default());
    let want = format!("len={}", big.len());
    c.wait_for("len", move |c| c.text().contains(&want));
    p.signal(libc::SIGTERM);
    assert_eq!(p.wait_exit(WAIT), 0, "code");
}
