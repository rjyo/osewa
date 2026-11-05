//! The UI over a mirror: input splitting, the prediction ticker, and the
//! events it draws.

use std::time::Duration;

use super::*;
use crate::{split_utf8, Attached, Ui};

#[test]
fn split_utf8_holds_back_partial_runes() {
    let s = "a你".as_bytes();
    for cut in 0..=s.len() {
        let (done, rest) = split_utf8(&s[..cut]);
        let mut joined = rest.to_vec();
        joined.extend_from_slice(&s[cut..]);
        let (j, _) = split_utf8(&joined);
        let mut all = done.to_vec();
        all.extend_from_slice(j);
        assert_eq!(all, s, "cut {cut}");
    }
    let (done, rest) = split_utf8(&[0xff]);
    assert!(done.len() == 1 && rest.is_empty(), "invalid byte held back");
}

/// An attached UI over a mirror showing a prompt, predicting with timeout.
fn predicting_ui(timeout: Duration) -> Attached {
    use osewa_ghostty::{self as ghostty, Terminal};
    use osewa_mirror::{PredictMode, Predictor, PredictorConfig};
    let pred = Predictor::new(PredictorConfig { mode: PredictMode::Always, timeout, ..Default::default() });
    let a = Attached::new(Ui::new(Box::new(Buf::default()), true, pred), "me".into());
    let mut t = Terminal::new(20, 4, ghostty::Options::default()).unwrap();
    t.write(b"$ ");
    let snap = t.snapshot().unwrap();
    a.m.snapshot_begin(&osewa_wire::SnapshotBegin { history: Some(false), ..Default::default() }).unwrap();
    a.m.snapshot_data(&snap[..ghostty::ready_end(&snap)], false).unwrap();
    a.m.snapshot_ready().unwrap();
    a.m.snapshot_end().unwrap();
    let mode = osewa_wire::EventMsg { echo: Some(true), canonical: Some(true), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_INPUT_MODE) };
    a.m.event(&mode, true).unwrap();
    a
}

/// The ticker sleeps while nothing is predicted, and wakes for a guess's
/// timeout rather than on a period: an idle attach costs no CPU.
#[test]
fn ticker_wakes_only_for_prediction_deadlines() {
    let a = predicting_ui(Duration::from_millis(200));
    let wakes = || a.ui.lock().unwrap().ticker_wakes;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _rt = rt.enter();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let t = rt.spawn(crate::ticker(a.clone(), async move {
        let _ = stopped.await;
    }));
    rt.block_on(tokio::time::sleep(Duration::from_millis(500)));
    assert_eq!(wakes(), 1, "the ticker woke with nothing predicted");

    a.predict("ab");
    a.predict("c"); // more typing only pushes the deadline out
    assert!(a.ui.lock().unwrap().pred.pending(), "nothing predicted");
    rt.block_on(tokio::time::sleep(Duration::from_millis(100)));
    assert!(a.ui.lock().unwrap().pred.pending(), "dropped before the timeout");
    rt.block_on(tokio::time::sleep(Duration::from_millis(250)));
    assert!(!a.ui.lock().unwrap().pred.pending(), "kept past the timeout");
    // Woken by the first guess, then once at the deadline.
    assert_eq!(wakes(), 3, "ticker wakes");
    rt.block_on(tokio::time::sleep(Duration::from_millis(300)));
    assert_eq!(wakes(), 3, "the ticker woke again with nothing predicted");
    let _ = stop.send(());
    rt.block_on(t).unwrap();
}

/// A live clipboard event reaches the local clipboard as OSC 52; one from a
/// catch-up doesn't.
#[test]
fn clipboard_events_set_the_local_clipboard() {
    use osewa_mirror::{Predictor, PredictorConfig};
    let out = Buf::default();
    let a = Attached::new(Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())), "me".into());
    let clip = |data: &[u8]| osewa_wire::EventMsg { data: data.to_vec(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_CLIPBOARD) };
    a.m.event(&clip(b"old"), false).unwrap();
    assert_eq!(out.string(), "", "a catch-up clipboard event was applied");
    a.m.event(&clip(b"hi!"), true).unwrap();
    assert_eq!(out.string(), "\x1b]52;c;aGkh\x1b\\");
}

/// Live notifications reach the local terminal, with nothing in them that
/// could end or extend the sequence; catch-up ones don't.
#[test]
fn notify_events_notify_locally() {
    use osewa_mirror::{Predictor, PredictorConfig};
    let out = Buf::default();
    let a = Attached::new(Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())), "me".into());
    let notify = |title: &str, msg: &str| osewa_wire::EventMsg { title: title.into(), msg: msg.into(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_NOTIFY) };
    a.m.event(&notify("", "old"), false).unwrap();
    assert_eq!(out.string(), "", "a catch-up notification was shown");
    a.m.event(&notify("", "done\x07"), true).unwrap();
    a.m.event(&notify("CI; main", "all\x1b\\ green"), true).unwrap();
    assert_eq!(out.string(), "\x1b]9;done\x1b\\\x1b]777;notify;CI, main;all\\ green\x1b\\");
}

/// With --clipboard-read a live read asks the local terminal; old ones
/// from a catch-up don't, and without the flag it is never asked.
#[test]
fn clipboard_reads_ask_only_when_allowed() {
    use osewa_mirror::{Predictor, PredictorConfig};
    let read = |sel: &str| osewa_wire::EventMsg { selection: sel.into(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_CLIPBOARD_READ) };
    for (allowed, want) in [(true, "\x1b]52;p;?\x1b\\"), (false, "")] {
        let out = Buf::default();
        let mut ui = Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default()));
        if allowed {
            ui = ui.with_clipboard_read();
        }
        let a = Attached::new(ui, "me".into());
        a.m.event(&read("c"), false).unwrap();
        a.m.event(&read("p"), true).unwrap();
        assert_eq!(out.string(), want, "allowed {allowed}");
    }
}


/// An attached UI showing a snapshot of a program with Kitty flags 5.
fn keyed_ui(legacy_viewer: bool) -> (Attached, Buf) {
    use osewa_ghostty::{self as ghostty, Terminal};
    use osewa_mirror::{Predictor, PredictorConfig};
    let out = Buf::default();
    let mut ui = Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default()));
    if legacy_viewer {
        ui = ui.with_legacy_viewer();
    }
    let a = Attached::new(ui, "me".into());
    let mut t = Terminal::new(20, 4, ghostty::Options::default()).unwrap();
    t.write(b"\x1b[>5u$ ");
    let snap = t.snapshot().unwrap();
    a.m.snapshot_begin(&osewa_wire::SnapshotBegin { history: Some(false), ..Default::default() }).unwrap();
    a.m.snapshot_data(&snap[..ghostty::ready_end(&snap)], false).unwrap();
    a.m.snapshot_ready().unwrap();
    (a, out)
}

/// A legacy viewer's terminal gets the program's keyboard modes only while
/// it owns the session, the moment ownership moves, with no output needed.
#[test]
fn legacy_viewer_keyboard_modes_follow_ownership() {
    let owner = |o: &str| osewa_wire::EventMsg { owner: o.into(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_OWNER) };
    let (a, out) = keyed_ui(true);
    assert!(!out.string().contains("\x1b[=5;1u"), "a viewer got the modes");
    a.m.event(&owner("someone"), true).unwrap();
    assert!(!out.string().contains("\x1b[=5;1u"), "another owner gave a viewer the modes");
    a.m.event(&owner("me"), false).unwrap();
    assert!(out.string().ends_with("\x1b[=5;1u"), "taking over: {:?}", out.string());
    a.m.event(&owner("someone"), true).unwrap();
    assert!(out.string().ends_with("\x1b[=0;1u"), "losing it: {:?}", out.string());
}

/// An app's viewer keeps the modes: its first key takes over and reaches
/// the program, encoded by the app from them.
#[test]
fn other_viewers_keep_the_keyboard_modes() {
    let (_a, out) = keyed_ui(false);
    assert!(out.string().contains("\x1b[=5;1u"), "{:?}", out.string());
}

/// An owner event after the session's owner was read must not be undone
/// by that older read; with none in between, the read counts.
#[test]
fn an_owner_read_never_undoes_a_newer_event() {
    use osewa_mirror::{Predictor, PredictorConfig};
    let out = Buf::default();
    let mut ui = Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())).with_legacy_viewer();
    let mut t = osewa_ghostty::Terminal::new(20, 4, osewa_ghostty::Options::default()).unwrap();
    t.write(b"\x1b[>5u");
    ui.ready(&t);
    let owner = |o: &str| osewa_wire::EventMsg { owner: o.into(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_OWNER) };

    // The id is empty here, so "" is this client.
    let events = ui.owner_events;
    ui.event(&owner("someone"), true);
    ui.read_owner("", events);
    assert!(!out.string().contains("\x1b[=5;1u"), "a stale read made this client the owner");

    let events = ui.owner_events;
    ui.read_owner("", events);
    assert!(out.string().ends_with("\x1b[=5;1u"), "{:?}", out.string());
}
