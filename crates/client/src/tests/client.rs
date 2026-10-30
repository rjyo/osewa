//! client_test.go

use super::*;

#[test]
fn dial_welcome() {
    let f = Fake::new();
    let c = f.dial("a");
    let w = run(c.welcome()).unwrap();
    assert!(w.server == "fake" && w.ghostty == ghostty::COMMIT, "welcome = {w:?}");
    assert_eq!(c.client_id(), "a");
}

/// Without a TLS config there is no pin: dial refuses rather than trust any
/// server with keystrokes.
#[test]
fn dial_requires_tls() {
    let f = Fake::new();
    let mut cfg = f.config("a");
    cfg.tls = None;
    let err = run(Conn::dial(cfg)).unwrap_err();
    assert!(err.to_string().contains("tls"), "err = {err}");
}

#[test]
fn dial_fails() {
    let f = Fake::new();
    let mut cfg = f.config("a");
    cfg.url = format!("wss://127.0.0.1:1{}", wire::PATH);
    assert!(run(Conn::dial(cfg)).is_err());
    let mut cfg = f.config("a");
    cfg.url = format!("ws://127.0.0.1:1{}", wire::PATH);
    assert!(run(Conn::dial(cfg)).is_err());
}

#[test]
fn list_create_kill() {
    let f = Fake::new();
    let c = f.dial("a");
    let id = run(c.create(CreateOptions { cols: 80, rows: 24, argv: vec!["/bin/sh".into()], ..Default::default() })).unwrap();
    assert_eq!(id.len(), 32);
    let list = run(c.list()).unwrap();
    assert!(list.len() == 1 && list[0].id == id && list[0].cols == 80, "list = {list:?}");
    run(c.kill(&id)).unwrap();
    let err = run(c.kill(&id)).unwrap_err();
    assert!(err.is_code(wire::ERR_NOT_FOUND), "kill twice: {err}");
    assert!(err.to_string().contains("not_found"), "{err}");
}

#[test]
fn attach_snapshot_output_and_acks() {
    let f = Fake::new();
    let id = f.create(40, 10);
    // Enough history that the snapshot has history pages.
    for i in 0..200 {
        f.output(&id, &format!("old line {i}\r\n"));
    }
    let c = f.dial("a");
    let (ready, out) = (shared::<usize>(), shared::<usize>());
    let (r2, o2) = (ready.clone(), out.clone());
    let s = attach(
        &c,
        &id,
        AttachOptions {
            on_ready: Some(Box::new(move || *r2.lock().unwrap() += 1)),
            on_output: Some(Box::new(move |b| *o2.lock().unwrap() += b.len())),
            ..Default::default()
        },
    );
    s.wait_text("old line 199");
    f.output(&id, "\x1b[1mhello\x1b[0m world\r\n");
    s.wait_text("hello world");
    let next = f.with(&id, |fs| fs.next);
    let (epoch, pos) = s.position();
    assert!(pos == next && !epoch.is_empty(), "position = {epoch} {pos}, want {next}");
    // The small tail is acked by the timer.
    eventually("ack", || f.with(&id, |fs| fs.acked) == next);
    assert_eq!(s.size(), (40, 10));
    assert!(*ready.lock().unwrap() == 1 && *out.lock().unwrap() > 0);
    assert!(s.id() == id && s.connected());

    // A large output is acked without waiting for the timer.
    f.output(&id, &"x".repeat(40 << 10));
    let next = f.with(&id, |fs| fs.next);
    eventually("big ack", || f.with(&id, |fs| fs.acked) == next);
}

#[test]
fn attach_errors() {
    let f = Fake::new();
    let c = f.dial("a");
    let err = run(c.attach("nope", opts(10, 5))).unwrap_err();
    assert!(err.is_code(wire::ERR_NOT_FOUND), "err = {err}");
}

#[test]
fn resume_from_position() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = f.dial("a");
    let s = attach(&c, &id, Default::default());
    f.output(&id, "one\r\n");
    s.wait_text("one");

    f.drop_all();
    eventually("disconnect", || !s.connected());
    f.output(&id, "two\r\n");
    f.output(&id, "three\r\n");
    // Typing while disconnected is queued.
    s.send_text("queued").unwrap();

    let c2 = f.dial("a");
    run(s.resume(&c2)).unwrap();
    s.wait_text("three");
    assert!(f.lock().attaches.last().unwrap().resumed, "resume did not resume from position");
    assert_eq!(s.text(), "one\ntwo\nthree");
    eventually("queued input", || f.with(&id, |fs| fs.applied.len()) == 1 && s.pending() == 0);
}

#[test]
fn resume_evicted_gets_snapshot() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = f.dial("a");
    let s = attach(&c, &id, Default::default());
    f.output(&id, "before\r\n");
    s.wait_text("before");
    f.drop_all();
    eventually("disconnect", || !s.connected());
    f.output(&id, "\x1b[2Jafter\r\n");
    f.evict(&id);

    run(s.resume(&f.dial("a"))).unwrap();
    s.wait_text("after");
    let want = f.with(&id, |fs| fs.term.0.text());
    assert_eq!(s.text(), want);
    f.output(&id, "live\r\n");
    s.wait_text("live");
}

#[test]
fn new_snapshot_replaces_mirror() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, Default::default());
    f.output(&id, "logged\r\n");
    s.wait_text("logged");
    // Change the host terminal outside the log; only a snapshot can carry it.
    f.with(&id, |fs| fs.term.0.write(b"\x1b[2J\x1b[Hreplaced"));
    f.resnapshot(&id);
    s.wait_text("replaced");
    assert!(!s.text().contains("logged"), "old mirror kept: {:?}", s.text());
}

fn input_line(i: &wire::Input) -> String {
    format!("{}:{}:{}:{:?}:{}:{}", i.ty, i.key, i.text, i.mods, i.action, i.focused)
}

#[test]
fn input_resend_and_dedupe() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, own());
    f.set(|st| st.no_ack = true);
    let (code, mods, text, action) = osewa_mirror::key(&ghostty::Key {
        code: "KeyC".into(),
        mods: ghostty::parse_mods(&["ctrl", "shift"]).unwrap(),
        text: "C".into(),
        action: ghostty::KeyAction::Repeat,
    });
    let mods: Vec<&str> = mods.iter().map(String::as_str).collect();
    let (code2, mods2, text2, action2) =
        osewa_mirror::key(&ghostty::Key { code: "KeyC".into(), action: ghostty::KeyAction::Release, ..Default::default() });
    let mods2: Vec<&str> = mods2.iter().map(String::as_str).collect();
    s.send_text("ls\r").unwrap();
    s.paste("pasted").unwrap();
    s.focus(true).unwrap();
    s.key(&code, &mods, &text, &action).unwrap();
    s.key(&code2, &mods2, &text2, &action2).unwrap();
    s.key("Enter", &["alt", "super"], "", "press").unwrap();
    eventually("applied", || f.with(&id, |fs| fs.applied.len()) == 6);
    assert_eq!(s.pending(), 6);

    f.drop_all();
    eventually("disconnect", || !s.connected());
    f.set(|st| st.no_ack = false);
    s.send_text("7").unwrap();
    run(s.resume(&f.dial("a"))).unwrap();
    eventually("acked", || s.pending() == 0);

    f.with(&id, |fs| {
        // The post-attach INPUT_ACK reports the 6 as applied, so only the
        // input queued while disconnected goes out again.
        assert_eq!(fs.dupes, 0, "dupes");
        for (i, input) in fs.applied.iter().enumerate() {
            assert_eq!(input.seq, i as u64 + 1, "seq at {i}");
        }
        let got: Vec<String> = fs.applied.iter().map(input_line).collect();
        let want = [
            "text::ls\r:[]::false",
            "paste::pasted:[]::false",
            "focus:::[]::true",
            "key:KeyC:C:[\"shift\", \"ctrl\"]:repeat:false",
            "key:KeyC::[]:release:false",
            "key:Enter::[\"alt\", \"super\"]::false",
            "text::7:[]::false",
        ];
        assert_eq!(got, want);
    });
}

#[test]
fn events_only_live() {
    let f = Fake::new();
    let id = f.create(40, 10);
    f.emit(&id, wire::EventMsg::kind(wire::EVENT_BELL)); // before attach: in the snapshot's past
    let s = attach(&f.dial("a"), &id, Default::default());
    let mut events = s.take_events().unwrap();
    f.output(&id, "x");
    s.wait_text("x");
    f.emit(&id, wire::EventMsg { title: "live1".into(), ..wire::EventMsg::kind(wire::EVENT_TITLE) });
    let e = run(events.recv()).unwrap();
    assert_eq!(e.title, "live1", "event = {e:?}");

    f.drop_all();
    eventually("disconnect", || !s.connected());
    f.emit(&id, wire::EventMsg::kind(wire::EVENT_BELL));
    f.emit(&id, wire::EventMsg { data: b"secret".to_vec(), ..wire::EventMsg::kind(wire::EVENT_CLIPBOARD) });
    f.emit(&id, wire::EventMsg { cols: 30, rows: 6, ..wire::EventMsg::kind(wire::EVENT_RESIZE) });
    run(s.resume(&f.dial("a"))).unwrap();
    f.emit(&id, wire::EventMsg { title: "live2".into(), ..wire::EventMsg::kind(wire::EVENT_TITLE) });
    let e = run_for(WAIT, events.recv()).expect("no live event").unwrap();
    assert_eq!(e.title, "live2", "catch-up event delivered: {e:?}");
    // Catch-up events still apply their state.
    assert_eq!(s.size(), (30, 6), "mirror did not follow resize");
}

#[test]
fn stable_client_id_continues_above_holder_seq() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c1 = f.dial("dev");
    let s1 = attach(&c1, &id, own());
    for x in ["a", "b", "c"] {
        s1.send_text(x).unwrap();
    }
    eventually("first acked", || s1.pending() == 0);
    drop(s1);
    run(c1.close());

    // A new process with the same client_id: input typed before the
    // holder's base INPUT_ACK arrives waits for it, then goes out above it.
    f.set(|st| st.hold_base = true);
    let s2 = attach(&f.dial("dev"), &id, own());
    s2.send_text("d").unwrap();
    s2.send_text("e").unwrap();
    sleep(Duration::from_millis(20));
    assert_eq!(f.with(&id, |fs| fs.applied.len()), 3, "input sent before the base ack");
    f.release_base();
    eventually("second acked", || s2.pending() == 0);
    s2.send_text("f").unwrap();
    eventually("third acked", || s2.pending() == 0);
    f.with(&id, |fs| {
        let got: Vec<String> = fs.applied.iter().map(|i| format!("{}{}", i.seq, i.text)).collect();
        assert!(got.join(" ") == "1a 2b 3c 4d 5e 6f" && fs.dupes == 0, "applied {got:?}, dupes {}", fs.dupes);
    });
}

#[test]
fn exit() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, Default::default());
    let mut events = s.take_events().unwrap();
    f.output(&id, "bye");
    s.wait_text("bye");
    assert!(s.exit_code().is_none(), "exit code before exit");
    f.exit(&id, 7);
    wait_for("done", s.done());
    assert_eq!(s.exit_code(), Some(7));
    assert!(s.err().is_none(), "{:?}", s.err());
    while run(events.recv()).is_some() {}
    assert_eq!(s.text(), "bye", "final screen");
    assert!(s.send_text("x").unwrap_err().is_ended(), "input after end");
    assert!(run(s.resume(&f.dial("a"))).unwrap_err().is_ended(), "resume after end");
    assert!(s.try_wait_text(WAIT, "never").is_err(), "wait_text on ended session");
}

#[test]
fn killed_session_err() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = f.dial("a");
    let s = attach(&c, &id, Default::default());
    run(c.kill(&id)).unwrap();
    wait_for("done", s.done());
    let err = s.err().expect("no error");
    assert!(err.to_string().contains("killed"), "err = {err}");
}

#[test]
fn resume_ended() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, Default::default());
    f.output(&id, "x");
    s.wait_text("x");
    f.drop_all();
    f.with(&id, |fs| fs.epoch = "restarted".into());
    let err = run(s.resume(&f.dial("a"))).unwrap_err();
    assert!(err.is_code(wire::ERR_ENDED), "err = {err}");
    wait_for("done", s.done());
    assert!(s.err().is_some_and(|e| e.is_ended()), "err = {:?}", s.err());
}

#[test]
fn detach_own_resize() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let a = attach(&f.dial("a"), &id, Default::default());
    let b = attach(&f.dial("b"), &id, own());
    assert_eq!(b.owner(), "b");
    let err = run(a.resize(50, 12)).unwrap_err();
    assert!(err.is_code(wire::ERR_NOT_OWNER), "non-owner resize: {err}");
    run(a.own()).unwrap();
    assert_eq!(a.owner(), "a");
    eventually("owner event", || b.owner() == "a");
    run(a.resize(50, 12)).unwrap();
    for s in [&a, &b] {
        eventually("resize", || s.size().0 == 50);
    }
    run(a.detach()).unwrap();
    wait_for("done", a.done());
    assert!(a.err().is_none(), "{:?}", a.err());
    assert!(run(a.own()).unwrap_err().is_closed(), "own after detach");
}

#[test]
fn conn_loss_fails_requests() {
    let f = Fake::new();
    let c = f.dial("a");
    f.drop_all();
    wait_for("conn done", c.done());
    assert!(c.err().unwrap().is_closed(), "{:?}", c.err());
    assert!(run(c.list()).is_err(), "request on dead conn");
    assert!(run(c.attach("x", opts(1, 1))).is_err(), "attach on dead conn");
}

#[test]
fn request_cancelled() {
    let f = Fake::new();
    let c = f.dial("a");
    // Hold the fake so the response can't arrive.
    let held = f.lock();
    assert!(run_for(Duration::from_millis(20), c.list()).is_none());
    drop(held);
    // The forgotten request leaves the connection fine.
    run(c.list()).unwrap();
}

#[test]
fn error_message() {
    let e = |code: &str, message: &str| Error::Response { code: code.into(), message: message.into() }.to_string();
    assert_eq!(e("x", ""), "osewa: x");
    assert_eq!(e("x", "m"), "osewa: x: m");
}
