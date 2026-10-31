//! Abandoned sessions: the screen snapshot that follows, with and
//! without a mirror.

use super::nomirror::App;
use super::*;
use crate::session::ACK_DELAY;

/// A SNAPSHOT_BEGIN before the previous snapshot's READY abandons it: the
/// partial decode goes, the mirror stays until the new READY, the abandoned
/// bytes are still credited, and nothing is acked as the abandoned
/// generation.
#[test]
fn abandoned_screen_snapshot() {
    let f = Fake::new();
    let id = f.create(40, 5);
    f.output(&id, "old screen");
    let s = attach(&f.dial("a"), &id, opts(40, 5));
    s.wait_text("old screen");
    f.output(&id, " more"); // leaves an ACK due shortly

    // The host moves on, and abandons the screen it started for that.
    f.with(&id, |fs| fs.term.0.write(b"\x1b[2J\x1b[Hfirst try"));
    f.abandon(&id);
    eventually("abandoned bytes credited", || {
        let (sent, credited) = f.flow();
        sent > 0 && credited == sent
    });
    sleep(2 * ACK_DELAY);
    let got = s.visible_text();
    assert!(got.contains("old screen more"), "mirror changed before a READY: {got:?}");

    f.with(&id, |fs| fs.term.0.write(b"\x1b[2J\x1b[Hsecond try"));
    f.screen_snapshot(&id);
    s.wait_text("second try");
    f.output(&id, " live");
    s.wait_text("second try live");
    eventually("ack", || f.with(&id, |fs| fs.acked == fs.next));

    // Generations: 0 (attach), 1 (abandoned), 2. ACKs only ever name a
    // generation whose screen the client had: never 1.
    f.with(&id, |fs| {
        assert!(!fs.ack_gens.contains(&1), "acked as the abandoned generation: {:?}", fs.ack_gens);
        assert_eq!(*fs.ack_gens.last().unwrap(), 2, "acks {:?}", fs.ack_gens);
    });
}

/// Without a mirror the app gets the abandoned snapshot's BEGIN and partial
/// data, then the new BEGIN, and must drop what it had: the app here does,
/// and ends up with the right screen.
#[test]
fn abandoned_no_mirror() {
    let f = Fake::new();
    let id = f.create(40, 5);
    f.output(&id, "old");
    let a = App::new();
    let _s = attach_raw(&f.dial("a"), &id, a.options());
    eventually("first screen", || a.text().contains("old"));

    f.with(&id, |fs| fs.term.0.write(b"\x1b[2J\x1b[Habandoned"));
    f.abandon(&id);
    f.with(&id, |fs| fs.term.0.write(b"\x1b[2J\x1b[Hfinal"));
    f.screen_snapshot(&id);
    eventually("final screen", || a.text().contains("final"));
    let got = a.log().join(" ");
    assert!(got.ends_with("begin begin ready end"), "callbacks {got:?}");
}
