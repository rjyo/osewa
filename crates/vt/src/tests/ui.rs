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

/// A live status reaches the local terminal as OSC 7501, even with notices
/// off; catch-up ones don't, nor one that would end or extend the sequence.
#[test]
fn status_events_report_locally() {
    use osewa_mirror::{Predictor, PredictorConfig};
    let out = Buf::default();
    let status = |state: &str| osewa_wire::EventMsg { state: state.into(), app: "fx".into(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_STATUS) };
    // An app embedding osewa gets statuses as events, not in its terminal.
    let a = Attached::new(Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())), "me".into());
    a.m.event(&status("blocked"), true).unwrap();
    a.clear_status();
    assert_eq!(out.string(), "", "a status forwarded without with_status_forward");
    let a = Attached::new(Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())).with_status_forward(), "me".into());
    a.m.event(&status("done"), false).unwrap();
    assert_eq!(out.string(), "", "a catch-up status was reported");
    a.m.event(&status("done:x=1"), true).unwrap();
    a.m.event(&osewa_wire::EventMsg { app: "fx\x1b\\".into(), ..status("done") }, true).unwrap();
    a.m.event(&osewa_wire::EventMsg { status_kind: "auth:x=1".into(), ..status("blocked") }, true).unwrap();
    assert_eq!(out.string(), "", "a status that breaks out of the sequence was reported");
    a.m.event(&osewa_wire::EventMsg { msg: "Apply?".into(), status_kind: "permission".into(), title: "Work\x1b\\".into(), ..status("blocked") }, true).unwrap();
    a.m.event(&osewa_wire::EventMsg { progress: Some(250), ..status("working") }, true).unwrap();
    a.m.event(&status("clear"), true).unwrap();
    assert_eq!(out.string(), "\x1b]7501;state=blocked:kind=permission:app=fx:title=V29yaxtc:msg=QXBwbHk/\x1b\\\x1b]7501;state=working:app=fx:progress=100\x1b\\\x1b]7501;state=clear:app=fx\x1b\\");
}

/// Leaving clears a status the local terminal still shows, once; one the
/// program cleared, or a task's own clear followed by nothing, needs none.
#[test]
fn status_cleared_on_leave() {
    use osewa_mirror::{Predictor, PredictorConfig};
    let status = |state: &str, id: &str| osewa_wire::EventMsg { state: state.into(), id: id.into(), ..osewa_wire::EventMsg::kind(osewa_wire::EVENT_STATUS) };
    let clear = "\x1b]7501;state=clear\x1b\\";
    for (events, want_clear) in [
        (vec![], false),
        (vec![status("working", "")], true),
        (vec![status("working", ""), status("clear", "")], false),
        (vec![status("working", "build"), status("clear", "build")], true),
    ] {
        let out = Buf::default();
        let a = Attached::new(Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())).with_status_forward(), "me".into());
        for e in &events {
            a.m.event(e, true).unwrap();
        }
        let before = out.string();
        a.clear_status();
        a.clear_status();
        let added = out.string()[before.len()..].to_string();
        assert_eq!(added, if want_clear { clear } else { "" }, "after {events:?}");
    }
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

/// A redraw part way through a graphics transmission must not write into
/// it: the local terminal gets the transmission cut off, and the image
/// from the mirror once the mirror has it whole. (Without a pixel size the
/// image covers no cells, so there is no placement to check.)
#[test]
fn redraw_mid_image_places_it_from_the_mirror() {
    use osewa_ghostty::{self as ghostty, Terminal};
    use osewa_mirror::{Predictor, PredictorConfig};
    let out = Buf::default();
    let ui = Ui::new(Box::new(out.clone()), true, Predictor::new(PredictorConfig::default())).with_graphics();
    let a = Attached::new(ui, "me".into());
    let mut t = Terminal::new(20, 4, ghostty::Options::default()).unwrap();
    t.write(b"$ ");
    let snap = t.snapshot().unwrap();
    a.m.snapshot_begin(&osewa_wire::SnapshotBegin { history: Some(false), ..Default::default() }).unwrap();
    a.m.snapshot_data(&snap[..ghostty::ready_end(&snap)], false).unwrap();
    a.m.snapshot_ready().unwrap();
    a.m.snapshot_end().unwrap();

    a.m.output(b"\x1b_Ga=T,f=24,s=2,v=1,i=9,m=1;AA").unwrap();
    let before = out.string().len();
    a.redraw();
    let redraw = out.string()[before..].to_string();
    assert!(redraw.starts_with('\x18'), "the partial command isn't cut first: {redraw:?}");
    a.m.output(b"AA\x1b\\\x1b_Gm=0;AAAA\x1b\\").unwrap();
    let after = out.string()[before + redraw.len()..].to_string();
    assert!(!after.contains("m=0,q=2;AAAA"), "the cut transmission's last chunk passed: {after:?}");
    assert!(after.contains("a=t,f=24,s=2,v=1,i=9,"), "the image wasn't sent from the mirror: {after:?}");
}
