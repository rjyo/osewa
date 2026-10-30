use std::time::Duration;

use osewa_wire as wire;

use super::helpers::*;
use crate::Config;

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
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
}

#[test]
fn bad_requests() {
    let h = start(cfg(sh("sleep 30")));
    let mut c = h.dial("dev1");
    let req = |method: &str| wire::Request { method: method.into(), ..Default::default() };
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
    let resp = c.attach(wire::Request { own: true, ..Default::default() });
    c.wait_text("line3");
    let (pos, before, prefix) = (c.end, c.raw.clone(), c.gens[0].position);
    c.close();
    std::thread::sleep(Duration::from_millis(200));

    let mut c2 = h.dial("dev1");
    let r2 = c2.attach(wire::Request { epoch: resp.epoch.clone(), position: Some(pos), ..Default::default() });
    assert!(r2.resumed, "{r2:?}");
    c2.end = pos;
    // A resumed client keeps its terminal; build one from what c had.
    c2.term = Some(
        osewa_ghostty::Terminal::new(40, 10, osewa_ghostty::Options { continuation: true, ..Default::default() }).unwrap(),
    );
    c2.wait_for("line10", |c| String::from_utf8_lossy(&c.raw).contains("line10"));
    assert!(c2.gens.is_empty(), "resume sent a snapshot");
    assert_eq!((c.gaps, c2.gaps), (0, 0));
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
    let resp = c.attach(wire::Request { own: true, ..Default::default() });
    c.wait_text("3000");
    let mut c2 = h.dial("dev1");
    let r2 = c2.attach(wire::Request { epoch: resp.epoch, position: Some(0), ..Default::default() });
    assert!(!r2.resumed, "resumed from an evicted position");
    c2.wait_text("3000");
    assert_eq!(c2.gens.len(), 1);
}

#[test]
fn input_echo_and_dedupe() {
    let h = start(cfg(sh("stty -echo; while read l; do echo got:$l; done")));
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
    std::thread::sleep(Duration::from_millis(200));
    c.text_input(1, "one\r");
    c.wait_text("got:one");
    c.wait_for("ack 1", |c| c.acked == 1);
    // A resend of seq 1 after a reconnect is dropped; 2 applies.
    c.close();
    let mut c2 = h.dial("dev1");
    c2.attach(wire::Request { own: true, ..Default::default() });
    c2.wait_for("ack from attach", |c| c.acked == 1);
    c2.text_input(1, "one\r");
    c2.text_input(2, "two\r");
    c2.wait_text("got:two");
    assert_eq!(c2.text().matches("got:one").count(), 1, "{}", c2.text());
}

#[test]
fn non_owner_input_dropped() {
    let h = start(cfg(sh("stty -echo; while read l; do echo got:$l; done")));
    let mut owner = h.dial("dev1");
    owner.attach(wire::Request { own: true, ..Default::default() });
    let mut viewer = h.dial("dev2");
    viewer.attach(wire::Request::default());
    viewer.text_input(1, "nope\r");
    viewer.wait_for("ack", |c| c.acked == 1);
    owner.text_input(1, "yes\r");
    owner.wait_text("got:yes");
    assert!(!owner.text().contains("got:nope"));
}

#[test]
fn key_paste_focus_input() {
    // The child prints what it reads as one run of hex digits after "H:",
    // so neither od's lines nor the terminal's wrapping can split it.
    let h = start(cfg(sh(r#"stty raw -echo; printf '\033[?1004h\033[?2004hGO'; h=$(dd bs=1 count=19 2>/dev/null | od -An -v -tx1 | tr -d ' \n'); printf 'H:%s:END' "$h"; sleep 30"#)));
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
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

fn unwrapped(s: &str) -> String {
    s.replace('\n', "")
}

#[test]
fn dsr_answered_without_client() {
    let h = start(cfg(sh("stty raw -echo; printf '\\033[6n'; dd bs=1 count=6 2>/dev/null | od -c; sleep 30")));
    std::thread::sleep(Duration::from_millis(500));
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    c.wait_text("033   [   1   ;   1   R");
}

#[test]
fn exit_code() {
    let mut h = start(Config { exit_retention: Duration::from_millis(300), ..cfg(sh("echo bye; exit 3")) });
    let mut c = h.dial("dev1");
    c.attach(wire::Request::default());
    c.wait_for("stream end", |c| c.ended.is_some());
    let end = c.ended.clone().unwrap();
    assert_eq!((end.reason.as_str(), end.exit_code), (wire::END_EXITED, Some(3)));
    assert!(c.events.iter().any(|e| e.kind == wire::EVENT_EXIT && e.code == Some(3)) || c.text().contains("bye"));
    let mut c2 = h.dial("dev2");
    let info = c2.info();
    assert!(info.exited && info.exit_code == 3, "{info:?}");
    // An attach after the exit gets the final screen, then the end.
    c2.attach(wire::Request::default());
    c2.wait_for("end", |c| c.ended.is_some());
    assert!(c2.text().contains("bye"));
    drop((c, c2));
    h.done(Duration::from_secs(5)).unwrap();
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
    c.attach(wire::Request { own: true, ..Default::default() });
    c.wait_text("up");
    let mut k = h.dial("");
    let resp = k.request(wire::Request { method: wire::METHOD_KILL.into(), session: "s1".into(), ..Default::default() });
    assert!(resp.error.is_empty(), "kill: {resp:?}");
    k.request(wire::Request { method: wire::METHOD_KILL.into(), ..Default::default() });
    c.wait_for("stream end", |c| c.ended.is_some());
    let end = c.ended.clone().unwrap();
    assert_eq!(end.reason, wire::END_KILLED);
    assert!(end.exit_code.is_some_and(|c| c != 0), "{end:?}");
    drop((c, k));
    h.done(Duration::from_secs(5)).unwrap();
}

#[test]
fn ownership_lapse() {
    let h = start(Config { owner_grace: Duration::from_millis(200), ..cfg(sh("sleep 30")) });
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
    let mut v = h.dial("dev2");
    v.attach(wire::Request::default());
    c.close();
    v.wait_for("owner cleared", |v| v.events.iter().any(|e| e.kind == wire::EVENT_OWNER && e.owner.is_empty()));
    assert!(v.info().owner.is_empty());
    // Owning again works for anyone.
    let resp = v.request(wire::Request { method: wire::METHOD_OWN.into(), stream: 7, ..Default::default() });
    assert_eq!(resp.owner, "dev2");
}

#[test]
fn events() {
    let h = start(cfg(sh("sleep 0.3; printf '\\007\\033]2;work\\033\\\\\\033]52;c;aGVsbG8=\\033\\\\'; sleep 30")));
    let mut owner = h.dial("dev1");
    owner.attach(wire::Request { own: true, ..Default::default() });
    let mut viewer = h.dial("dev2");
    viewer.attach(wire::Request::default());
    owner.wait_for("clipboard", |c| c.events.iter().any(|e| e.kind == wire::EVENT_CLIPBOARD));
    let clip = owner.events.iter().find(|e| e.kind == wire::EVENT_CLIPBOARD).unwrap();
    assert_eq!(clip.data, b"hello");
    assert!(owner.events.iter().any(|e| e.kind == wire::EVENT_BELL));
    assert!(owner.events.iter().any(|e| e.kind == wire::EVENT_TITLE && e.title == "work"));
    viewer.wait_for("title", |c| c.events.iter().any(|e| e.kind == wire::EVENT_TITLE));
    viewer.drain(Duration::from_millis(200));
    assert!(!viewer.events.iter().any(|e| e.kind == wire::EVENT_CLIPBOARD), "clipboard went to a viewer");
    assert_eq!(viewer.info().title, "work");
}

#[test]
fn attach_head_and_input_ack() {
    let h = start(cfg(sh("stty -echo; cat")));
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
    c.text_input(1, "a");
    c.text_input(2, "b");
    c.wait_for("ack", |c| c.acked == 2);
    c.close();
    // The same client id learns where its seqs continue.
    let mut c2 = h.dial("dev1");
    let r = c2.attach(wire::Request::default());
    assert!(r.head.is_some());
    c2.wait_for("input ack", |c| c.acked == 2);
}

#[test]
fn config_errors() {
    let bad = [
        Config { id: "".into(), ..Default::default() },
        Config { id: "a/b".into(), run_dir: "/tmp".into(), argv: sh("true"), cols: 80, rows: 24, ..Default::default() },
        Config { id: "x".into(), argv: sh("true"), cols: 80, rows: 24, ..Default::default() },
        Config { id: "x".into(), run_dir: "/tmp".into(), cols: 80, rows: 24, ..Default::default() },
        Config { id: "x".into(), run_dir: "/tmp".into(), argv: sh("true"), cols: 0, rows: 24, ..Default::default() },
        Config { id: "x".into(), run_dir: "/tmp".into(), argv: sh("true"), cols: 80, rows: 1001, ..Default::default() },
    ];
    for mut c in bad {
        assert!(c.defaults().is_err(), "{c:?}");
    }
}

#[test]
fn parse_flags() {
    let args: Vec<String> = ["--run", "/r", "--id", "x", "--cols", "100", "--rows", "30", "--cwd", "/w", "--idle-timeout", "0", "--", "sh", "-l"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let c = crate::parse_flags(&args).unwrap();
    assert_eq!((c.run_dir.to_str().unwrap(), c.id.as_str(), c.cols, c.rows, c.cwd.as_str()), ("/r", "x", 100, 30, "/w"));
    assert_eq!(c.argv, ["sh", "-l"]);
    assert!(c.idle.is_zero());
    assert_eq!(c.scrollback_bytes, crate::DEFAULT_SCROLLBACK_BYTES);
    let d = crate::parse_flags(&["--id".to_string(), "y".to_string()]).unwrap();
    assert_eq!(d.idle, crate::DEFAULT_IDLE);
    assert!(crate::parse_flags(&["--nope".to_string()]).is_err());
}

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
