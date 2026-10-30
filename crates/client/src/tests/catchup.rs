//! catchup_test.go

use super::*;

/// A catch-up screen snapshot replaces the mirror without history, and a
/// later full snapshot brings the scrollback back.
#[test]
fn screen_snapshot_then_full() {
    let f = Fake::new();
    let id = f.create(40, 5);
    for _ in 0..3000 {
        f.output(&id, "history line\r\n");
    }
    let s = attach(&f.dial("a"), &id, opts(40, 5));
    s.wait_text("history line");
    let full = f.with(&id, |fs| fs.term.0.scrollback_rows());
    eventually("full history", || s.scrollback() == full);

    // The holder skipped output and sends just the screen. Only the history
    // that shares a page with the screen comes along.
    f.with(&id, |fs| fs.term.0.write(b"\x1b[2J\x1b[Hcaught up"));
    f.screen_snapshot(&id);
    s.wait_text("caught up");
    let n = s.scrollback();
    assert!(n < full / 2, "screen snapshot brought {n} rows of history");
    let next = f.with(&id, |fs| fs.next);
    eventually("screen ack", || f.with(&id, |fs| fs.acked) == next);
    assert!(s.connected(), "screen snapshot broke the stream");
    f.output(&id, "\r\nlive after");
    s.wait_text("live after");

    // More screens while still behind.
    f.screen_snapshot(&id);
    f.output(&id, "\r\nstill live");
    s.wait_text("still live");

    // Caught up: the full snapshot restores scrollback.
    f.resnapshot(&id);
    eventually("scrollback back", || s.scrollback() >= full / 2);
}

/// A connection lost while the mirror is screen-only resumes with a full
/// snapshot, not from the position, or the scrollback would never return.
#[test]
fn resume_after_screen_snapshot_wants_full() {
    let f = Fake::new();
    let id = f.create(40, 5);
    for _ in 0..3000 {
        f.output(&id, "old\r\n");
    }
    let s = attach(&f.dial("a"), &id, opts(40, 5));
    s.wait_text("old");
    f.with(&id, |fs| fs.term.0.write(b"now"));
    f.screen_snapshot(&id);
    s.wait_text("now");
    sleep(Duration::from_millis(50));

    f.drop_all();
    wait_for("lost", s.lost());
    run(s.resume(&f.dial("a"))).unwrap();
    assert!(!f.lock().attaches.last().unwrap().resumed, "resumed onto a screen-only mirror");
    let full = f.with(&id, |fs| fs.term.0.scrollback_rows());
    eventually("scrollback", || s.scrollback() >= full / 2);
}
