//! flow_test.go

use super::*;
use crate::conn::CREDIT_DELAY;
use crate::session::MAX_UNACKED;

#[test]
fn credit_covers_received_bytes() {
    let f = Fake::new();
    let id = f.create(80, 24);
    for _ in 0..500 {
        f.output(&id, &format!("{}\r\n", "h".repeat(70))); // history: a multi-chunk snapshot
    }
    let s = attach(&f.dial("a"), &id, opts(80, 24));
    for _ in 0..30 {
        f.output(&id, &"o".repeat(4000));
    }
    f.output(&id, "\r\ndone");
    s.wait_text("done");
    // Everything is credited, the small tail by the timer.
    eventually("full credit", || {
        let (sent, credited) = f.flow();
        sent > 100 << 10 && credited == sent
    });
}

/// An echo's ACK goes out with its CREDIT, in the same write, rather than
/// on a timer of its own ACK_DELAY later: one wakeup and write fewer per
/// keystroke on both ends.
#[test]
fn ack_rides_with_credit() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, Default::default());
    s.wait_text("");
    for i in 0..3 {
        let acked = f.with(&id, |fs| fs.acked);
        f.output(&id, "x");
        let credited = || {
            let (sent, credited) = f.credited(&id);
            sent == credited
        };
        let start = Instant::now();
        while credited() {
            assert!(start.elapsed() < WAIT, "echo {i} never sent");
            std::thread::sleep(Duration::from_millis(1));
        }
        while !credited() {
            assert!(start.elapsed() < WAIT, "echo {i} never credited");
            std::thread::sleep(Duration::from_millis(1));
        }
        let at = Instant::now();
        while f.with(&id, |fs| fs.acked) == acked {
            assert!(at.elapsed() < CREDIT_DELAY / 5, "echo {i}: ACK {:?} after its CREDIT", at.elapsed());
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Credit is sent on receipt: an apply that is stuck (here a blocked
/// on_output) must not hold back the window.
#[test]
fn credit_does_not_wait_on_apply() {
    let f = Fake::new();
    let id = f.create(80, 24);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = Mutex::new(rx);
    let _s = attach(
        &f.dial("a"),
        &id,
        AttachOptions {
            cols: 80,
            rows: 24,
            on_output: Some(Box::new(move |_| {
                let _ = rx.lock().unwrap().recv();
            })),
            ..Default::default()
        },
    );
    for _ in 0..20 {
        f.output(&id, &"x".repeat(4000));
    }
    eventually("credit while apply is stuck", || {
        let (sent, credited) = f.flow();
        credited == sent
    });
    drop(tx);
}

#[test]
fn credit_held_while_apply_backlogged() {
    let f = Fake::new();
    let id = f.create(80, 24);
    let mut cfg = f.config("a");
    cfg.knobs.apply_max = Some(32 << 10);
    let c = run(Conn::dial(cfg)).unwrap();
    run(c.welcome()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = Mutex::new(rx);
    let once = Mutex::new(false);
    let s = attach(
        &c,
        &id,
        AttachOptions {
            cols: 80,
            rows: 24,
            on_output: Some(Box::new(move |_| {
                if !std::mem::replace(&mut *once.lock().unwrap(), true) {
                    let _ = rx.lock().unwrap().recv();
                }
            })),
            ..Default::default()
        },
    );
    eventually("snapshot", || s.connected() && s.pending() == 0 && s.text().is_empty() && s.ready());
    for _ in 0..40 {
        f.output(&id, &"y".repeat(4000));
    }
    sleep(Duration::from_millis(200));
    let (sent, credited) = f.flow();
    assert!(sent - credited >= 100 << 10, "credited {credited} of {sent} while the apply queue was full");
    drop(tx);
    eventually("credit after draining", || {
        let (sent, credited) = f.flow();
        credited == sent
    });
}

#[test]
fn incompatible_server() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, Default::default());
    f.set(|st| st.commit = "deadbeef".into());

    // Dial doesn't wait for WELCOME; the first request fails instead.
    let c = run(Conn::dial(f.config("a"))).unwrap();
    let err = run(c.list()).unwrap_err();
    assert!(err.is_incompatible(), "err = {err}");
    match &err {
        Error::Incompatible { server_commit, client_commit, .. } => {
            assert!(server_commit == "deadbeef" && client_commit == ghostty::COMMIT, "err = {err:?}")
        }
        e => panic!("err = {e:?}"),
    }
    assert!(err.to_string().contains("deadbeef") && err.to_string().contains(ghostty::COMMIT), "{err}");
    assert!(run(c.welcome()).unwrap_err().is_incompatible(), "welcome");

    // A resume onto such a server ends the session: no retry can work.
    f.drop_all();
    wait_for("lost", s.lost());
    let c2 = run(Conn::dial(f.config("a"))).unwrap();
    let err = run(s.resume(&c2)).unwrap_err();
    assert!(err.is_incompatible(), "resume: {err}");
    wait_for("done", s.done());
    assert!(s.err().is_some_and(|e| e.is_incompatible()), "{:?}", s.err());
    c.close_now();
    c2.close_now();
}

/// HELLO and the first request go out in one flight (PROTOCOL.md "Frames").
#[test]
fn first_request_does_not_wait_for_welcome() {
    let f = Fake::new();
    let id = f.create(40, 10);
    f.set(|st| st.late_welcome = true);
    let c = f.dial("a");
    run(c.list()).unwrap();
    let s = attach(&c, &id, Default::default());
    f.output(&id, "x");
    s.wait_text("x");

    // Cold resume: the attach rides with HELLO.
    f.drop_all();
    wait_for("lost", s.lost());
    run(s.resume(&f.dial("a"))).unwrap();
    f.output(&id, "y");
    s.wait_text("xy");
    assert_eq!(f.lock().pipelined, 2);
}

#[test]
fn snapshot_data_is_not_credited() {
    let f = Fake::new();
    let id = f.create(200, 50);
    f.output(&id, &format!("\x1b[H{}", format!("{}\r\n", "s".repeat(199)).repeat(49))); // a full screen, no history
    let s = attach(&f.dial("a"), &id, opts(200, 50));
    s.wait_text("sss");
    // Only the snapshot's history records (here just the empty manifest and
    // FINISH) count; the screen itself, about 10 KB, doesn't.
    eventually("credit", || {
        let (sent, credited) = f.flow();
        credited == sent
    });
    let (_, credited) = f.flow();
    assert!(credited <= 1024, "credited {credited} bytes for the first screen");
}

/// Catch-up screen snapshots count their SNAPSHOT_DATA: the holder paces
/// them on credit. A resumed stream's first snapshot is uncounted again.
#[test]
fn later_snapshot_data_is_credited() {
    let f = Fake::new();
    let id = f.create(200, 50);
    f.output(&id, &format!("\x1b[H{}", format!("{}\r\n", "s".repeat(199)).repeat(49)));
    let s = attach(&f.dial("a"), &id, opts(200, 50));
    s.wait_text("sss");
    let settled = || {
        let (sent, credited) = f.flow();
        credited == sent
    };
    eventually("first credit", settled);
    let (_, before) = f.flow();

    f.screen_snapshot(&id);
    eventually("screen credit", settled);
    let (_, after) = f.flow();
    assert!(after - before >= 5000, "a catch-up screen credited {} bytes", after - before);

    // A new stream starts over: its first snapshot is free again.
    f.drop_all();
    wait_for("lost", s.lost());
    f.with(&id, |fs| {
        fs.evicted = fs.next + 1;
        fs.log.clear();
    });
    let c2 = f.dial("a");
    run(s.resume(&c2)).unwrap();
    sleep(3 * CREDIT_DELAY);
    let (_, credited) = f.flow();
    assert!(credited <= 1024, "a resumed stream's first snapshot credited {credited} bytes");
}

/// A client keeps at most 512 KiB of input unacked; the rest waits for
/// INPUT_ACKs (PROTOCOL.md "Input").
#[test]
fn unacked_input_bounded() {
    let f = Fake::new();
    let id = f.create(40, 5);
    let s = ready(&f.dial("a"), &id, own());
    f.set(|st| st.no_ack = true);
    let chunk = "p".repeat(8 << 10);
    for _ in 0..200 {
        // 1.6 MB
        s.paste(&chunk).unwrap();
    }
    sleep(Duration::from_millis(200));
    let n = f.input_bytes(&id);
    assert!(n <= MAX_UNACKED && n >= MAX_UNACKED - 2 * chunk.len(), "{n} bytes in flight without acks, want about {MAX_UNACKED}");
    // Each ack lets more out, until everything is through.
    f.set(|st| st.no_ack = false);
    f.ack_inputs(&id);
    eventually("all input", || f.input_bytes(&id) == 200 * chunk.len());
    run(s.flush()).unwrap();

    // One event over the bound still goes when nothing else is in flight.
    let big = "b".repeat(MAX_UNACKED + 1);
    s.paste(&big).unwrap();
    run(s.flush()).unwrap();
    assert_eq!(f.input_bytes(&id), 200 * chunk.len() + big.len(), "oversized paste lost");
}
