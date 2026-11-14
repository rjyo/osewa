//! The attach UI: the escaper, UTF-8 splitting, input forwarding (and its
//! wait for ownership), viewer notices, and attach against a live server.

use std::io::Read;
use std::sync::mpsc;

use osewa_client::hosts;

use super::live::LiveHost;
use super::*;
use crate::attach::{forward_input, EscAction, Escaper, RESET_MODES};
use crate::ui::{viewer_notice, InputHandler, NOTICE_EVERY};

#[test]
fn escaper() {
    let cases: &[(&[&str], &str, EscAction)] = &[
        (&["ls\r"], "ls\r", EscAction::None),
        (&["~."], "", EscAction::Detach),               // at start: counts as after newline
        (&["a~."], "a~.", EscAction::None),             // not after newline
        (&["ls\r~.rest"], "ls\r", EscAction::Detach),   // detach mid-buffer
        (&["x\n~", "."], "x\n", EscAction::Detach),     // split across reads
        (&["\r~~."], "\r~.", EscAction::None),          // ~~ sends one ~
        (&["\r~a"], "\r~a", EscAction::None),           // other bytes pass with the ~
        (&["\r~", "\r~."], "\r~\r", EscAction::Detach), // ~ then newline resets
        (&["\r~tls"], "\rls", EscAction::Take),         // ~t takes, typing goes on
        (&["a~t"], "a~t", EscAction::None),             // not after newline
    ];
    for (input, want, want_act) in cases {
        let mut e = Escaper { nl: true, tilde: false };
        let mut got = Vec::new();
        let mut act = EscAction::None;
        'feed: for i in *input {
            let mut b = i.as_bytes();
            while !b.is_empty() {
                let (out, a, rest) = e.feed(b);
                got.extend_from_slice(&out);
                if a != EscAction::None {
                    act = a;
                }
                if a == EscAction::Detach {
                    break 'feed;
                }
                b = rest;
            }
        }
        assert!(got == want.as_bytes() && act == *want_act, "{input:?}: got {:?} {act:?}, want {want:?} {want_act:?}", String::from_utf8_lossy(&got));
    }
}

#[test]
fn reports_leave_typing() {
    use crate::attach::Report::*;
    let cases: &[(&[&str], &str, &[crate::attach::Report])] = &[
        (&["ab\x1b[?997;1nc"], "abc", &[Scheme(true)]),
        (&["\x1b[?997;2n\x1b[?997;1n"], "", &[Scheme(false), Scheme(true)]),
        (&["a\x1b[?99", "7;2nb"], "ab", &[Scheme(false)]),
        (&["\x1b[?997;", "1", "n"], "", &[Scheme(true)]),
        (&["\x1b]11;rgb:1010/2020/3030\x1b\\x"], "x", &[Background([0x10, 0x20, 0x30])]),
        (&["\x1b]10;rgb:f/80/fff\x07\x1b]12;rgb:0/0/0\x07"], "", &[Foreground([0xff, 0x80, 0xff]), Cursor([0, 0, 0])]),
        (&["\x1b]4;1;rgb:cc", "cc/0000/0000\x1b", "\\\x1b]4;255;rgb:1/2/3\x1b\\"], "", &[Palette(1, [0xcc, 0, 0]), Palette(255, [0x11, 0x22, 0x33])]),
        (&["\x1b]1", "1;rgb:0/0/0\x07"], "", &[Background([0, 0, 0])]),
        // #7: a burst of replies cut at a reply's ESC, ESC ] or ESC [.
        (&["\x1b]4;35;rgb:0/0/0\x1b\\\x1b", "]4;36;rgb:0000/afaf/8787\x1b\\"], "", &[Palette(35, [0, 0, 0]), Palette(36, [0, 0xaf, 0x87])]),
        (&["\x1b[?997;1n\x1b]", "11;rgb:0/0/0\x07"], "", &[Scheme(true), Background([0, 0, 0])]),
        (&["\x1b]10;rgb:0/0/0\x07\x1b[", "?997;2n"], "", &[Foreground([0, 0, 0]), Scheme(false)]),
        // A key after the burst still arrives, with the next read.
        (&["\x1b]10;rgb:0/0/0\x07\x1b", "x"], "\x1bx", &[Foreground([0, 0, 0])]),
        (&["\x1b]10;rgb:0/0/0\x07a\x1b"], "a\x1b", &[Foreground([0, 0, 0])]),
        // Keys: a lone ESC, Alt-], arrows and other reports pass, now.
        (&["\x1b", "x"], "\x1bx", &[]),
        (&["\x1b]", "x"], "\x1b]x", &[]),
        (&["\x1b[A\x1b[?1;2c"], "\x1b[A\x1b[?1;2c", &[]),
        (&["\x1b[?997;9n"], "", &[]),
        (&["\x1b[?997;12345n"], "\x1b[?997;12345n", &[]),
        // Malformed replies are taken but say nothing.
        (&["\x1b]11;nope\x07"], "", &[]),
        (&["\x1b]52;c;aG", "kh\x1b\\"], "", &[Clipboard(b"hi!".to_vec())]),
        (&["\x1b]5", "2;c;\x07"], "", &[Clipboard(Vec::new())]),
    ];
    for (reads, want, want_schemes) in cases {
        let mut r = crate::attach::Reports::default();
        let (mut got, mut schemes) = (Vec::new(), Vec::new());
        for read in *reads {
            let (b, s) = r.feed(read.as_bytes());
            got.extend(b);
            schemes.extend(s);
        }
        assert_eq!((String::from_utf8_lossy(&got).into_owned(), schemes), (want.to_string(), want_schemes.to_vec()), "{reads:?}");
    }
}

#[derive(Default)]
struct Sink {
    got: Mutex<Vec<String>>,
    flushed: Mutex<usize>,
    takes: Mutex<usize>,
    reports: Mutex<Vec<crate::attach::Report>>,
}

impl InputHandler for Sink {
    fn send(&self, t: &str) {
        self.got.lock().unwrap().push(t.into());
    }
    fn take(&self) {
        *self.takes.lock().unwrap() += 1;
    }
    fn reports(&self, r: &[crate::attach::Report]) {
        self.reports.lock().unwrap().extend_from_slice(r);
    }
    fn flush(&self, _: &Ctx) {
        *self.flushed.lock().unwrap() = self.got.lock().unwrap().len();
    }
}

#[test]
fn forward_input_sends_takes_and_detaches() {
    let s = Sink::default();
    forward_input(&ctx(), &mut "echo 你好\x1b[?997;1n\r~tx\r~.ignored".as_bytes(), &s);
    assert!(s.got.lock().unwrap().concat() == "echo 你好\rx\r" && *s.flushed.lock().unwrap() == 0, "got {:?}", s.got.lock().unwrap());
    assert_eq!(*s.takes.lock().unwrap(), 1);
    assert_eq!(*s.reports.lock().unwrap(), [crate::attach::Report::Scheme(true)], "the color scheme report");
    // EOF detaches too, but only once everything typed was delivered.
    let s = Sink::default();
    forward_input(&ctx(), &mut "x".as_bytes(), &s);
    assert!(s.got.lock().unwrap().concat() == "x" && *s.flushed.lock().unwrap() == 1, "got {:?}", s.got.lock().unwrap());
}

/// #12: keys right after ~t wait for the takeover instead of being refused
/// by the old owner.
#[test]
fn forward_input_waits_for_take() {
    #[derive(Default)]
    struct TakeSink {
        st: Mutex<(bool, Vec<String>, Vec<String>)>,
    }
    impl InputHandler for TakeSink {
        fn send(&self, t: &str) {
            let mut st = self.st.lock().unwrap();
            if st.0 {
                st.2.push(t.into());
            } else {
                st.1.push(t.into());
            }
        }
        fn take(&self) {
            std::thread::sleep(Duration::from_millis(30)); // a round trip
            self.st.lock().unwrap().0 = true;
        }
        fn reports(&self, _: &[crate::attach::Report]) {}
        fn flush(&self, _: &Ctx) {}
    }
    let s = TakeSink::default();
    forward_input(&ctx(), &mut "\r~tls\r".as_bytes(), &s);
    let st = s.st.lock().unwrap();
    assert!(st.1.concat() == "\r" && st.2.concat() == "ls\r", "before {:?}, after {:?}", st.1, st.2);
}

#[test]
fn shows_graphics_by_terminal() {
    for (env, want) in [
        (&[("TERM", "xterm-kitty")][..], true),
        (&[("TERM", "xterm-ghostty")], true),
        (&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "ghostty")], true),
        (&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "WezTerm")], true),
        (&[("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")], true),
        (&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "Apple_Terminal")], false),
        (&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "iTerm.app")], false),
        (&[], false),
    ] {
        let got = crate::attach::shows_graphics(|k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()));
        assert_eq!(got, want, "{env:?}");
    }
}

#[test]
fn viewer_notices() {
    for (owner, me, want) in [("phone-abcd", "mac-1234", "phone owns"), ("mac-abcd", "mac-1234", "another window on this device owns"), ("", "mac-1234", "nobody owns")] {
        let got = viewer_notice(owner, me);
        assert!(got.contains(want) && got.contains("Ctrl-T takes over"), "viewer_notice({owner:?}, {me:?}) = {got:?}, want {want:?}");
    }
}

/// stdin as an io.Pipe: written by the test, EOF once the writer drops.
struct PipeReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl Read for PipeReader {
    fn read(&mut self, p: &mut [u8]) -> std::io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(b) => self.buf = b,
                Err(_) => return Ok(0),
            }
        }
        let n = p.len().min(self.buf.len());
        p[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

struct Pipe(Option<mpsc::Sender<Vec<u8>>>);

impl Pipe {
    fn write(&self, s: &str) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(s.as_bytes().to_vec());
        }
    }

    fn close(&mut self) {
        self.0 = None;
    }
}

/// A running `osewa attach`: its stdin, stdout, stderr and exit code.
struct Attach {
    stdin: Pipe,
    out: Buf,
    err: Buf,
    code: mpsc::Receiver<i32>,
    ctx: Ctx,
}

impl Attach {
    #[track_caller]
    fn code(&self) -> i32 {
        self.code.recv_timeout(Duration::from_secs(10)).expect("attach did not return")
    }
}

/// Runs attach with a pipe for stdin, as client id (empty for the
/// process's), with raw mode if given.
fn attach_with(h: &LiveHost, id: &str, raw: Option<crate::cli::RawFn>, a: &[&str]) -> Attach {
    let (tx, rx) = mpsc::channel();
    let (out, err) = (Buf::default(), Buf::default());
    let mut e = Env::new(PipeReader { rx, buf: Vec::new() }, out.clone(), err.clone(), h.dir.path().into());
    e.id = id.into();
    e.raw = raw;
    let ctx = Ctx::new();
    let (ctx2, args) = (ctx.clone(), args(a));
    let (ctx_tx, code) = mpsc::channel();
    std::thread::spawn(move || {
        let c = block(cli::run_client(&ctx2, "attach", &args, &e));
        let _ = ctx_tx.send(c);
    });
    Attach { stdin: Pipe(Some(tx)), out, err, code, ctx }
}

fn attach_async(h: &LiveHost, a: &[&str]) -> Attach {
    attach_with(h, "", None, a)
}

#[test]
fn live_new_ls_attach_kill() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h", "--", "/bin/sh"]).trim().to_string();
    let out = h.run_ok(&["ls", "h"]);
    assert!(out.contains(&id) && out.contains("running"), "ls:\n{out}");

    let a = attach_async(&h, &["h", &id]);
    a.stdin.write("echo osewa-$((6*7))\r");
    wait_for(&a.out, "osewa-42");
    a.stdin.write("\r~.");
    assert_eq!(a.code(), 0, "detach exit");

    // The session survived the detach; attach without an id picks it and
    // the snapshot redraws what was there.
    let mut a = attach_async(&h, &["h"]);
    wait_for(&a.out, "osewa-42");
    // Same device, same client id: input continues above the first run's
    // seq.
    a.stdin.write("echo again-$((2*3))\r");
    wait_for(&a.out, "again-6");
    a.stdin.close(); // EOF detaches too
    a.code();

    h.run_ok(&["kill", "h", &id]);
}

#[test]
fn live_attach_creates_and_exits() {
    let h = LiveHost::new();
    let a = attach_async(&h, &["h"]);
    a.stdin.write("echo created\r");
    wait_for(&a.out, "created");
    a.stdin.write("exit 7\r");
    assert_eq!(a.code(), 7, "exit code");
}

/// #14: --new starts a session of its own beside a running one, and names
/// none.
#[test]
fn live_attach_new() {
    let h = LiveHost::new();
    let old = h.run_ok(&["new", "h"]).trim().to_string();
    let a = attach_async(&h, &["--new", "h"]);
    a.stdin.write("echo fresh-$((3*3))\r");
    wait_for(&a.out, "fresh-9");
    let out = h.run_ok(&["ls", "h"]);
    assert_eq!(out.lines().filter(|l| l.contains("running")).count(), 2, "ls:\n{out}");
    a.stdin.write("exit 0\r");
    assert_eq!(a.code(), 0, "exit code");
    let out = h.run_ok(&["ls", "h"]);
    assert!(out.lines().any(|l| l.contains(&old) && l.contains("running")), "the running session was left alone:\n{out}");

    let (code, _, _) = h.run(&["attach", "--new", "h", &old]);
    assert_eq!(code, 2, "--new with an id");
}

#[test]
fn live_attach_several_sessions() {
    let h = LiveHost::new();
    h.run_ok(&["new", "h"]);
    h.run_ok(&["new", "h"]);
    let (code, _, err) = h.run(&["attach", "h"]);
    assert!(code == 1 && err.contains("several sessions"), "attach: {code} {err}");
}

#[test]
fn live_reconnect() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let a = attach_async(&h, &["h", &id]);
    a.stdin.write("echo one\r");
    wait_for(&a.out, "one");

    h.restart();
    a.stdin.write("echo two-$((1+1))\r");
    wait_for(&a.out, "two-2");
    a.stdin.write("\r~.");
    assert_eq!(a.code(), 0, "exit");
}

#[test]
fn live_piped_input_runs_before_detach() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let (e, _, err) = h.env("echo piped-$((3*4))\r");
    let code = cli::run(&args(&["attach", "h", &id]), e);
    assert_eq!(code, 0, "piped attach: {}", err.string());
    let mut a = attach_async(&h, &["h", &id]);
    wait_for(&a.out, "piped-12");
    a.stdin.close();
    a.code();
}

/// Raw mode comes before the first byte of output, and the mode reset is
/// the last byte written: nothing from the session may land after it.
#[test]
fn live_terminal_reset_is_last() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h", "--", "/bin/sh", "-c", "while :; do echo spam; done"]).trim().to_string();
    let raw_at = Arc::new(Mutex::new(None));
    let out_slot: Arc<Mutex<Option<Buf>>> = Arc::default();
    let (ra, os) = (raw_at.clone(), out_slot.clone());
    let raw: crate::cli::RawFn = Arc::new(move || {
        *ra.lock().unwrap() = Some(os.lock().unwrap().as_ref().map_or(0, |b: &Buf| b.string().len()));
        // A slow restore widens the window in which late output could land
        // after the reset.
        Ok(Some(Box::new(|| std::thread::sleep(Duration::from_millis(200))) as Box<dyn FnOnce() + Send>))
    });
    let a = attach_with(&h, "", Some(raw), &["h", &id]);
    *out_slot.lock().unwrap() = Some(a.out.clone());
    wait_for(&a.out, "spam");
    a.ctx.cancel();
    a.code();
    assert_eq!(*raw_at.lock().unwrap(), Some(0), "raw mode after output");
    let got = a.out.string();
    assert!(got.ends_with(RESET_MODES), "output after the reset: {:?}", &got[got.len().saturating_sub(80)..]);
}

#[test]
fn live_detach_while_reconnecting() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let a = attach_async(&h, &["h", &id]);
    a.stdin.write("echo up-$((1+1))\r");
    wait_for(&a.out, "up-2");
    h.stop_serving();
    std::thread::sleep(Duration::from_millis(300));
    a.stdin.write("\r~.");
    assert_eq!(a.code(), 0, "detach during reconnect");
}

#[test]
fn live_queries_not_forwarded() {
    let h = LiveHost::new();
    let id = h
        .run_ok(&["new", "h", "--", "/bin/sh", "-c", "sleep 1; printf 'before\\033[6n\\033[c\\033]52;c;aGk=\\007after\\n'; sleep 30"])
        .trim()
        .to_string();
    let mut a = attach_async(&h, &["h", &id]);
    wait_for(&a.out, "after");
    a.stdin.close();
    a.code();
    for q in ["\x1b[6n", "\x1b[c", "\x1b]52;c;aGk=\x07"] {
        assert!(!a.out.string().contains(q), "forwarded {q:?}");
    }
    // The clipboard write reaches this owner once, from the holder's event.
    assert_eq!(a.out.string().matches("\x1b]52;c;aGk=\x1b\\").count(), 1, "clipboard set");
}

/// A second client attaches as a viewer: its typing is refused with a
/// notice until it takes ownership with Ctrl-T (or ~t, or --take).
#[test]
fn live_viewer_and_takeover() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let mut a = attach_async(&h, &["h", &id]);
    a.stdin.write("echo a-$((1+1))\r");
    wait_for(&a.out, "a-2");

    let mut b = attach_with(&h, "phone-abcd", None, &["h", &id]);
    wait_for(&b.out, "a-2");
    wait_for(&b.out, "viewing:");
    std::thread::sleep(NOTICE_EVERY + Duration::from_millis(100));
    b.stdin.write("echo b-$((2+3))\r");
    wait_for(&b.out, "owns this session; Ctrl-T takes over");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!a.out.string().contains("b-5"), "a viewer's input was applied");

    // Mid-line, with no newline before it: a viewer's Ctrl-T needs none,
    // and the keys after it are sent once it owns the session.
    std::thread::sleep(NOTICE_EVERY);
    b.stdin.write("zz\x14echo c-$((3+4))\r");
    wait_for(&b.out, "you own this session now");
    wait_for(&a.out, "c-7");

    // Now the first client is the viewer; ~t still takes it back.
    a.stdin.write("x");
    wait_for(&a.out, "phone owns this session");
    a.stdin.write("\r~t");
    wait_for(&a.out, "you own this session now");

    b.stdin.close();
    b.code();
    a.stdin.close();
    a.code();
}

/// A program's keyboard modes reach only the owner's terminal: a viewer's
/// keys are for osewa, and Ctrl-T must stay the byte it looks for. They
/// follow ownership both ways, whether they were set before the attach or
/// during it.
#[test]
fn live_keyboard_modes_follow_the_owner() {
    let keys = |s: String| -> Vec<String> {
        let mut seen = Vec::new();
        for (i, _) in s.match_indices("\x1b[") {
            let rest = &s[i + 2..];
            let n = rest.find(|c: char| c.is_ascii_alphabetic() || c == '~').unwrap_or(0);
            let (body, fin) = (&rest[..n], rest[n..].chars().next());
            if (fin == Some('u') && !body.is_empty()) || (fin == Some('m') && body.starts_with(">4")) {
                seen.push(rest[..=n].to_string());
            }
        }
        seen
    };
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let mut a = attach_async(&h, &["h", &id]);
    a.stdin.write("printf '\\033[>5u\\033[>4;2m'; echo mode-$((1+1))\r");
    wait_for(&a.out, "mode-2");
    eventually(|| keys(a.out.string()) == ["=5;1u", ">4;2m"]);

    let mut b = attach_with(&h, "phone-abcd", None, &["h", &id]);
    wait_for(&b.out, "viewing:");
    a.stdin.write("printf '\\033[>9u'; echo live-$((2+2))\r");
    wait_for(&b.out, "live-4");
    assert_eq!(keys(b.out.string()), Vec::<String>::new(), "a viewer got the program's modes");

    b.stdin.write("\x14");
    wait_for(&b.out, "you own this session now");
    eventually(|| keys(b.out.string()) == ["=9;1u", ">4;2m"]);
    eventually(|| keys(a.out.string()).ends_with(&["=0;1u".to_string(), ">4m".to_string()]));

    // A pop reaches the owner as the flags under it, which its own terminal
    // never had pushed.
    b.stdin.write("printf '\\033[<u'; echo pop-$((3+3))\r");
    wait_for(&b.out, "pop-6");
    eventually(|| keys(b.out.string()).last().map(String::as_str) == Some("=5;1u"));

    b.stdin.close();
    b.code();
    a.stdin.close();
    a.code();
}

/// Attaching to a session this client already owns changes no owner, so
/// the holder says nothing: the attach response has to be enough for the
/// terminal to get the program's modes.
#[test]
fn live_keyboard_modes_on_attach_as_the_owner() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let mut a = attach_with(&h, "laptop-1", None, &["h", &id]);
    a.stdin.write("printf '\\033[>5u'; echo mode-$((1+1))\r");
    wait_for(&a.out, "mode-2");
    wait_for(&a.out, "\x1b[=5;1u");
    a.stdin.close();
    a.code();

    let mut b = attach_with(&h, "laptop-1", None, &["h", &id]);
    wait_for(&b.out, "mode-2");
    wait_for(&b.out, "\x1b[=5;1u");
    b.stdin.close();
    b.code();
}

#[test]
fn live_attach_take() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let mut a = attach_async(&h, &["h", &id]);
    a.stdin.write("echo first\r");
    wait_for(&a.out, "first");

    let mut b = attach_with(&h, "tablet-1", None, &["--take", "--predict", "never", "h", &id]);
    wait_for(&b.out, "first");
    b.stdin.write("echo took-$((4+4))\r");
    wait_for(&a.out, "took-8");
    assert!(!b.out.string().contains("viewing:"), "--take attached as a viewer");
    b.stdin.close();
    b.code();
    a.stdin.close();
    a.code();

    let (code, _, err) = h.run(&["attach", "--predict", "sometimes", "h"]);
    assert!(code == 1 && err.contains("predict mode"), "bad --predict: {code} {err}");
}

/// After the network drops the connection silently (a new IP, a NAT
/// reset), typing brings the screen back within about a second and a half:
/// the silent connection turns suspect and attach resumes on a fresh one.
#[test]
fn live_recovers_from_blackhole() {
    let h = LiveHost::new();
    let p = Proxy::new(h.addr.parse().unwrap());
    let mut host = hosts::find(h.dir.path(), "h").unwrap();
    host.addr = p.addr.to_string();
    hosts::save(h.dir.path(), host).unwrap();

    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let a = attach_async(&h, &["h", &id]);
    a.stdin.write("echo up-$((1+1))\r");
    wait_for(&a.out, "up-2");

    p.blackhole();
    let start = Instant::now();
    a.stdin.write("echo back-$((2+2))\r");
    wait_for(&a.out, "back-4");
    let d = start.elapsed();
    eprintln!("screen fresh {d:?} after typing into a dead connection");
    assert!(d <= Duration::from_millis(2500), "recovery took {d:?}");
    a.stdin.write("\r~.");
    assert_eq!(a.code(), 0, "exit");
}

/// A program reporting blocked (OSC 7501) gets a notice in osewa attach,
/// and nothing with --quiet; the OSC itself never reaches the terminal.
#[test]
fn live_status_notice() {
    let h = LiveHost::new();
    // The report comes after the attach (so it's live), and after the
    // typing quiet period that follows the newline the test sends.
    let report = "echo ready; read x; sleep 2.2; printf '\\033]7501;state=blocked:kind=permission:app=claude-code:msg=QXBwbHk/\\007'; echo reported; sleep 30";
    for quiet in [false, true] {
        let id = h.run_ok(&["new", "h", "--", "/bin/sh", "-c", report]).trim().to_string();
        let mut argv = vec!["h", id.as_str()];
        if quiet {
            argv.insert(0, "--quiet");
        }
        let mut a = attach_async(&h, &argv);
        wait_for(&a.out, "ready");
        a.stdin.write("\r");
        wait_for(&a.out, "reported");
        if !quiet {
            wait_for(&a.out, "claude-code: blocked: Apply?");
        } else {
            std::thread::sleep(Duration::from_millis(200));
            assert!(!a.out.string().contains("claude-code: blocked"), "--quiet showed the notice");
        }
        assert!(!a.out.string().contains("7501"), "OSC 7501 forwarded");
        a.stdin.close();
        a.code();
    }
}

/// Exiting the program from one window ends the session for every window,
/// and each says so instead of looking detached; ~. says it detached.
#[test]
fn live_end_and_detach_messages() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h"]).trim().to_string();
    let a = attach_with(&h, "mac-aaaa", None, &["h", &id]);
    a.stdin.write("echo a-$((1+1))\r");
    wait_for(&a.out, "a-2");

    let b = attach_with(&h, "mac-bbbb", None, &["h", &id]);
    wait_for(&b.out, "another window on this device owns");
    b.stdin.write("\x14");
    wait_for(&b.out, "you own this session now");
    b.stdin.write("\r~.");
    b.code();
    wait_for(&b.err, "detached; the session is still running");

    let c = attach_with(&h, "mac-cccc", None, &["h", &id]);
    wait_for(&c.out, "a-2");
    c.stdin.write("\x14exit 3\r");
    wait_for(&a.err, "session ended, exit 3");
    assert_eq!(a.code(), 3, "first window exit code");
    assert_eq!(c.code(), 3, "closing window exit code");
}

/// Attach over QUIC against a live server: echo.
#[test]
fn live_quic_attach() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h", "--", "/bin/sh"]).trim().to_string();
    let (tx, rx) = mpsc::channel();
    let (out, err) = (Buf::default(), Buf::default());
    let mut e = Env::new(PipeReader { rx, buf: Vec::new() }, out.clone(), err.clone(), h.dir.path().into());
    e.carrier = Some("quic".into());
    let (done_tx, done) = mpsc::channel();
    let argv = args(&["h", &id]);
    std::thread::spawn(move || {
        let _ = done_tx.send(block(cli::run_client(&ctx(), "attach", &argv, &e)));
    });
    tx.send(b"echo quic-$((6*7))\r".to_vec()).unwrap();
    wait_for(&out, "quic-42");
    drop(tx);
    done.recv_timeout(Duration::from_secs(10)).expect("attach did not return");
}

/// A proxy: forwards TCP to target and can black-hole the
/// connections it has: they stay open but carry nothing, like a half-open
/// connection after a network change. New connections work.
struct Proxy {
    addr: std::net::SocketAddr,
    conns: Arc<Mutex<Vec<Arc<std::sync::atomic::AtomicBool>>>>,
}

impl Proxy {
    fn new(target: std::net::SocketAddr) -> Proxy {
        use std::sync::atomic::AtomicBool;
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap();
        let conns: Arc<Mutex<Vec<Arc<AtomicBool>>>> = Arc::default();
        let cs = conns.clone();
        std::thread::spawn(move || {
            for c in ln.incoming() {
                let Ok(c) = c else { return };
                let Ok(up) = std::net::TcpStream::connect(target) else { continue };
                let frozen = Arc::new(AtomicBool::new(false));
                cs.lock().unwrap().push(frozen.clone());
                let (c2, up2, f2) = (c.try_clone().unwrap(), up.try_clone().unwrap(), frozen.clone());
                std::thread::spawn(move || pipe(c, up, frozen));
                std::thread::spawn(move || pipe(up2, c2, f2));
            }
        });
        Proxy { addr, conns }
    }

    /// Freezes every connection made so far.
    fn blackhole(&self) {
        for c in self.conns.lock().unwrap().iter() {
            c.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn pipe(mut src: std::net::TcpStream, mut dst: std::net::TcpStream, frozen: Arc<std::sync::atomic::AtomicBool>) {
    use std::io::Write;
    let mut buf = vec![0; 32 << 10];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = dst.shutdown(std::net::Shutdown::Write);
                return;
            }
            Ok(n) => {
                if !frozen.load(std::sync::atomic::Ordering::SeqCst) && dst.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        }
    }
}
