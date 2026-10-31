//! The mirror terminal: following the stream, snapshots, visible text.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use osewa_client::block_on;
use osewa_ghostty::{self as ghostty, KeyAction, Terminal};
use osewa_wire as wire;

use super::*;

#[test]
fn visible_text_skips_scrollback() {
    let mut term = Terminal::new(20, 3, ghostty::Options::default()).unwrap();
    assert_eq!(visible_text(None), "", "None terminal");
    for i in 0..10 {
        term.write(format!("line {i}\r\n").as_bytes());
    }
    term.write("你好 tail  ".as_bytes());
    assert_eq!(visible_text(Some(&term)), "line 8\nline 9\n你好 tail");
    assert!(term.text().contains("line 0"), "Text lost scrollback");
    // Blank rows below the cursor are trimmed.
    term.write(b"\x1b[2J\x1b[Htop");
    assert_eq!(visible_text(Some(&term)), "top");
}

/// Sends term's snapshot through m as the transport would; with history
/// false it stops at READY, as a catch-up screen does.
fn feed(m: &Mirror, term: &Terminal, history: bool) {
    let snap = term.snapshot().unwrap();
    let ready = ghostty::ready_end(&snap);
    let b = wire::SnapshotBegin {
        history: (!history).then_some(false),
        ..Default::default()
    };
    m.snapshot_begin(&b).unwrap();
    m.snapshot_data(&snap[..ready], false).unwrap();
    m.snapshot_ready().unwrap();
    if history {
        m.snapshot_data(&snap[ready..], true).unwrap();
    }
    m.snapshot_end().unwrap();
}

fn new_term(cols: usize, rows: usize) -> Terminal {
    Terminal::new(
        cols,
        rows,
        ghostty::Options {
            continuation: true,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn mirror_follows_stream() {
    let mut host = new_term(30, 4);
    for i in 0..20 {
        host.write(format!("line {i}\r\n").as_bytes());
    }
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let (l1, l2, l3, l4, l5) = (log.clone(), log.clone(), log.clone(), log.clone(), log.clone());
    let m = Mirror::new(Options {
        on_ready: Some(Box::new(move |_| l1.lock().unwrap().push("ready".into()))),
        on_snapshot_end: Some(Box::new(move |_| l5.lock().unwrap().push("end".into()))),
        on_output: Some(Box::new(move |b| {
            l2.lock()
                .unwrap()
                .push(format!("out {}", String::from_utf8_lossy(b)))
        })),
        on_event: Some(Box::new(move |e, live| {
            l3.lock().unwrap().push(format!("event {} {live}", e.kind))
        })),
        on_applied: Some(Box::new(move |_| l4.lock().unwrap().push("applied".into()))),
    });
    assert!(
        m.text().is_empty() && m.visible_text().is_empty(),
        "text before a snapshot"
    );
    feed(&m, &host, true);
    assert!(
        m.text().contains("line 0") && !m.visible_text().contains("line 0"),
        "text {:?} visible {:?}",
        m.text(),
        m.visible_text()
    );
    m.output(b"tail").unwrap();
    block_on(m.wait_text("tail")).unwrap();
    m.event(
        &wire::EventMsg {
            kind: wire::EVENT_RESIZE.into(),
            cols: 50,
            rows: 6,
            width: 500,
            height: 120,
            ..Default::default()
        },
        false,
    )
    .unwrap();
    m.view(|t| {
        let t = t.unwrap();
        assert_eq!((t.size(), t.cell_size()), ((50, 6), (10, 20)), "size after resize")
    });
    m.event(&wire::EventMsg::kind(wire::EVENT_BELL), true)
        .unwrap();
    let want = "ready applied end out tail applied event resize false applied event bell true applied";
    assert_eq!(log.lock().unwrap().join(" "), want);
    m.close();
}

/// A BEGIN before READY drops the partial decode; the old terminal stays
/// until the new READY, and a screen snapshot brings no history.
#[test]
fn mirror_abandoned_and_screen_snapshots() {
    let mut host = new_term(30, 4);
    host.write(b"first");
    let m = Mirror::new(Options::default());
    feed(&m, &host, true);

    host.write(b"\x1b[2J\x1b[Habandoned");
    let snap = host.snapshot().unwrap();
    m.snapshot_begin(&wire::SnapshotBegin {
        generation: 1,
        history: Some(false),
        ..Default::default()
    })
    .unwrap();
    m.snapshot_data(&snap[..ghostty::ready_end(&snap) / 2], false)
        .unwrap();
    assert!(
        m.visible_text().contains("first"),
        "mirror replaced before READY"
    );
    host.write(b"\x1b[2J\x1b[Hscreen");
    feed(&m, &host, false);
    assert_eq!(m.visible_text(), "screen");
    m.close();
}

fn poll_once<F: Future>(f: F) -> Poll<F::Output> {
    let mut f = pin!(f);
    f.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn mirror_end_and_close() {
    let m = Mirror::new(Options::default());
    // A deadline: the wait doesn't finish on its own.
    let m1 = m.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(block_on(m1.wait_text("x"))).unwrap());
    assert!(
        rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "WaitText returned before End"
    );
    assert!(poll_once(m.wait_text("x")).is_pending());
    m.end();
    let err = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("WaitText after End")
        .unwrap_err();
    assert!(err.contains("ended"), "WaitText after End = {err}");
    // After close, late frames are ignored rather than failing.
    m.close();
    m.close();
    m.snapshot_data(b"x", false).unwrap();
    m.snapshot_ready().unwrap();
    m.output(b"x").unwrap();
    m.view(|t| assert!(t.is_none(), "terminal after close"));
}

#[test]
fn mirror_bad_snapshot() {
    let m = Mirror::new(Options::default());
    m.snapshot_begin(&wire::SnapshotBegin::default()).unwrap();
    m.snapshot_data(b"not a snapshot", false).unwrap();
    assert!(m.snapshot_ready().is_err(), "garbage decoded");
    m.close();
}

/// Through the Renderer trait, as the client drives it.
#[test]
fn mirror_as_renderer() {
    let mut host = new_term(20, 3);
    host.write(b"via trait");
    let m = Mirror::new(Options::default());
    let mut r: Box<dyn osewa_client::Renderer> = Box::new(m.clone());
    let snap = host.snapshot().unwrap();
    let ready = ghostty::ready_end(&snap);
    r.snapshot_begin(&wire::SnapshotBegin::default()).unwrap();
    r.snapshot_data(&snap[..ready], false).unwrap();
    r.snapshot_ready().unwrap();
    r.snapshot_data(&snap[ready..], true).unwrap();
    r.snapshot_end().unwrap();
    r.output(b"!").unwrap();
    assert_eq!(m.visible_text(), "via trait!");
    r.end();
    assert!(block_on(m.wait_text("never")).is_err());
}

#[test]
fn key_names() {
    let all = ghostty::parse_mods(&["ctrl", "shift", "alt", "super"]).unwrap();
    let (code, mods, text, action) = key(&ghostty::Key {
        code: "KeyC".into(),
        mods: all,
        text: "C".into(),
        action: KeyAction::Repeat,
    });
    assert!(
        code == "KeyC"
            && mods.join(",") == "shift,ctrl,alt,super"
            && text == "C"
            && action == "repeat",
        "{code:?} {mods:?} {text:?} {action:?}"
    );
    let (_, mods, _, action) = key(&ghostty::Key {
        code: "Enter".into(),
        action: KeyAction::Release,
        ..Default::default()
    });
    assert!(
        mods.is_empty() && action == "release",
        "{mods:?} {action:?}"
    );
    let (_, _, _, action) = key(&ghostty::Key {
        code: "Enter".into(),
        ..Default::default()
    });
    assert_eq!(action, "", "press");
}
