//! Sessions through the C API: create, type, see, resize, close, reopen,
//! kill, exit, reconnect, and the refusals.

use super::*;

/// The whole life of a session as the app sees it: created, typed into,
/// resized, closed and reopened by id onto the same screen, then killed.
#[test]
fn open_type_resize_close_reopen_kill() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let h = open(&host.open_opts(), &r);
    assert_eq!(r.wait_state(CONNECTED), None);
    assert_eq!(r.states.lock().unwrap()[0], (CONNECTING, None), "first state");
    let id = r.wait_event("session")["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32, "id {id:?}");

    send(h, "echo hi-$((40+2))\r");
    r.wait_screen("hi-42");
    // A rune cut between two sends is held back until it is whole.
    let rune = "é".as_bytes();
    assert_eq!(unsafe { osewa_send(h, b"echo a".as_ptr(), 6) }, 0);
    assert_eq!(unsafe { osewa_send(h, rune.as_ptr(), 1) }, 0);
    assert_eq!(unsafe { osewa_send(h, rune[1..].as_ptr(), 1) }, 0);
    send(h, "z\r");
    r.wait_screen("aéz\n");

    r.term.lock().unwrap().resize(100, 30).unwrap();
    unsafe { osewa_resize(h, 100, 30, 0, 0) };
    send(h, "stty size\r");
    r.wait_screen("30 100");

    let paste = CString::new("echo pasted").unwrap();
    assert_eq!(unsafe { osewa_paste(h, paste.as_ptr()) }, 0);
    send(h, "\r");
    r.wait_screen("pasted\n");

    // Stats come once a keystroke's round trip is measured.
    eventually("stats", || !r.stats.lock().unwrap().is_empty(), String::new);
    assert_eq!(r.stats.lock().unwrap()[0].1, "ws", "carrier");

    unsafe { osewa_close(h) };
    let calls = r.calls();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(r.calls(), calls, "callbacks after close");

    // Reopened by id onto a fresh terminal: the same screen comes back.
    let r2 = Rec::new(100, 30);
    let mut o = host.open_opts();
    o.session_id = Some(id.clone());
    (o.cols, o.rows) = (100, 30);
    let h = open(&o, &r2);
    r2.wait_state(CONNECTED);
    r2.wait_screen("hi-42");
    r2.wait_screen("30 100");
    assert_eq!(r2.wait_event("session")["id"], id.as_str());
    unsafe { osewa_kill(h) };

    // Killed (in the background): opening it again soon finds it gone.
    let last = Mutex::new(None);
    eventually(
        "the session gone",
        || {
            let r3 = Rec::new(80, 24);
            let h = open(&o, &r3);
            let st = r3.wait_state_any(&[CONNECTED, CLOSED]);
            unsafe { osewa_close(h) };
            *last.lock().unwrap() = r3.last_state();
            st == CLOSED
        },
        || format!("{:?}", last.lock().unwrap()),
    );
    // Gone, or (within the holder's exit retention) seen exiting.
    let why = last.lock().unwrap().clone().unwrap().1.unwrap_or_default();
    assert!(why.starts_with("ended: ") || why.starts_with("exit "), "detail {why:?}");
}

impl Rec {
    /// Waits for one of the states, and says which.
    #[track_caller]
    fn wait_state_any(&self, any: &[i32]) -> i32 {
        eventually("a state", || self.last_state().is_some_and(|s| any.contains(&s.0)), || format!("{:?}", self.states.lock().unwrap()));
        self.last_state().unwrap().0
    }
}

/// A program's exit closes with its code, after an exit event.
#[test]
fn exit_closes_with_code() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let h = open(&host.open_opts(), &r);
    r.wait_state(CONNECTED);
    send(h, "exit 3\r");
    assert_eq!(r.wait_state(CLOSED).as_deref(), Some("exit 3"));
    assert_eq!(r.wait_event("exit")["code"], 3);
    // Input after the end is refused, not queued forever.
    eventually("send refused", || unsafe { osewa_send(h, b"x".as_ptr(), 1) } != 0, String::new);
    unsafe { osewa_close(h) };
}

/// A server restart is a reconnect: RECONNECTING, then CONNECTED with the
/// session resumed where it was.
#[test]
fn reconnects_across_server_restart() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let h = open(&host.open_opts(), &r);
    r.wait_state(CONNECTED);
    send(h, "echo one\r");
    r.wait_screen("one\n");
    host.restart();
    eventually("reconnecting", || r.states.lock().unwrap().iter().any(|s| s.0 == RECONNECTING), || format!("{:?}", r.states.lock().unwrap()));
    r.wait_state(CONNECTED);
    send(h, "echo two\r");
    r.wait_screen("two\n");
    let s = r.screen();
    assert_eq!(s.lines().filter(|l| *l == "one").count(), 1, "repainted twice:\n{s}");
    unsafe { osewa_network_changed(h) };
    unsafe { osewa_close(h) };
}

/// redraw repaints everything for a new view: a fresh terminal fed only
/// what came after it shows the screen.
#[test]
fn redraw_repaints() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let h = open(&host.open_opts(), &r);
    r.wait_state(CONNECTED);
    send(h, "echo before\r");
    r.wait_screen("before\n");
    std::thread::sleep(Duration::from_millis(100));
    *r.term.lock().unwrap() = Terminal::new(80, 24, Options::default()).unwrap();
    unsafe { osewa_redraw(h) };
    r.wait_screen("before\n");
    unsafe { osewa_close(h) };
}

/// The app's appearance reaches programs that ask for it: OSC 11 is
/// answered with its background.
#[test]
fn appearance_answers_color_queries() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let mut o = host.open_opts();
    o.appearance = Some(r##"{"color_scheme":"dark","fg":"#ffffff","bg":"#123456"}"##);
    let h = open(&o, &r);
    r.wait_state(CONNECTED);
    // The shell reads the holder's reply to OSC 11 and prints it, escaped.
    send(h, "stty raw -echo; printf '\\033]11;?\\033\\\\'; dd bs=1 count=12 2>/dev/null | od -c; stty sane\r");
    r.wait_screen("r   g   b   :   1   2");
    unsafe { osewa_close(h) };
}

/// A device the server doesn't know is told so once, not retried forever.
#[test]
fn unenrolled_device_is_unauthorized() {
    let host = Host::new();
    let stranger = osewa_auth::Identity::generate();
    let r = Rec::new(80, 24);
    let mut o = host.open_opts();
    o.device = stranger.pkcs8();
    let h = open(&o, &r);
    let why = r.wait_state(CLOSED).unwrap_or_default();
    assert!(why.starts_with("unauthorized: "), "detail {why:?}");
    unsafe { osewa_close(h) };
}

/// A server that isn't the pinned one is refused the same way.
#[test]
fn wrong_server_key_is_unauthorized() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let mut o = host.open_opts();
    o.server_key = osewa_auth::encode_key(&osewa_auth::Identity::generate().public());
    let h = open(&o, &r);
    let why = r.wait_state(CLOSED).unwrap_or_default();
    assert!(why.starts_with("unauthorized: "), "detail {why:?}");
    unsafe { osewa_close(h) };
}

/// Nothing listening is not fatal: it stays CONNECTING, saying why.
#[test]
fn unreachable_keeps_connecting() {
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap().to_string();
    drop(ln);
    let host = Host::new();
    let r = Rec::new(80, 24);
    let mut o = host.open_opts();
    o.addr = addr;
    let h = open(&o, &r);
    eventually("a reason", || r.last_state().is_some_and(|s| s.0 == CONNECTING && s.1.is_some()), || format!("{:?}", r.states.lock().unwrap()));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(r.last_state().unwrap().0, CONNECTING);
    unsafe { osewa_close(h) };
}

/// Bad options fail osewa_open with why, before any connection.
#[test]
fn bad_options_fail_open() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    type Spoil = Box<dyn Fn(&mut Open)>;
    let cases: Vec<(&str, Spoil)> = vec![
        ("addr", Box::new(|o: &mut Open| o.addr = String::new())),
        ("server_key", Box::new(|o: &mut Open| o.server_key = "nope".into())),
        ("device_key", Box::new(|o: &mut Open| o.device = b"junk")),
        ("argv_json", Box::new(|o: &mut Open| o.argv = Some("[1]"))),
        ("appearance_json", Box::new(|o: &mut Open| o.appearance = Some(r#"{"color_scheme":"blue"}"#))),
    ];
    for (want, f) in cases {
        let mut o = host.open_opts();
        f(&mut o);
        let e = try_open(&o, &r).map(|_| ()).unwrap_err();
        assert!(e.contains(want), "{want}: {e}");
    }
    let mut err: *mut c_char = std::ptr::null_mut();
    assert!(unsafe { osewa_open(std::ptr::null(), r.callbacks(), &mut err) }.is_null());
    unsafe { osewa_string_free(err) };
    assert_eq!(r.calls(), 0, "callbacks for a failed open");
}

/// Keys: a generated one is a device key enroll takes.
#[test]
fn keys() {
    let mut len = 0usize;
    assert_eq!(unsafe { osewa_generate_key(std::ptr::null_mut(), 0, &mut len) }, -1);
    assert!(len > 32, "len {len}");
    let mut der = vec![0u8; len];
    assert_eq!(unsafe { osewa_generate_key(der.as_mut_ptr(), der.len(), &mut len) }, 0);
    let id = osewa_auth::Identity::from_pkcs8(der.clone()).unwrap();
    let p = unsafe { osewa_public_key(der.as_ptr(), der.len()) };
    assert!(!p.is_null());
    assert_eq!(unsafe { CStr::from_ptr(p) }.to_str().unwrap(), osewa_auth::encode_key(&id.public()));
    unsafe { osewa_string_free(p) };
    assert!(unsafe { osewa_public_key(b"junk".as_ptr(), 4) }.is_null());
}

#[test]
fn build_info() {
    let s = unsafe { CStr::from_ptr(osewa_build_info()) }.to_str().unwrap();
    assert_eq!(s, osewa_client::build_info());
    let v: serde_json::Value = serde_json::from_str(s).unwrap();
    assert_eq!(v["ghostty"], osewa_ghostty::COMMIT);
    assert_eq!(osewa_build_info(), osewa_build_info(), "not static");
}

/// osewa_send, osewa_resize and osewa_paste may be called from inside a
/// callback (osewa.h): they only queue.
#[test]
fn send_from_inside_a_callback() {
    static H: std::sync::atomic::AtomicPtr<OsewaSession> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
    static SENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    unsafe extern "C" fn answer(ctx: *mut c_void, b: *const u8, n: usize) {
        unsafe { on_output(ctx, b, n) };
        let h = H.load(std::sync::atomic::Ordering::SeqCst);
        if !h.is_null() && rec(ctx).screen().contains("ping-me\n") && !SENT.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let s = "echo pong-$((1+1))\r";
            unsafe {
                osewa_resize(h, 80, 24, 0, 0);
                assert_eq!(osewa_send(h, s.as_ptr(), s.len()), 0);
            }
        }
    }
    let host = Host::new();
    let r = Rec::new(80, 24);
    let mut cb = r.callbacks();
    cb.output = Some(answer);
    let o = host.open_opts();
    let mut s = Strings::default();
    let opts = OpenOptions {
        addr: s.add(&o.addr),
        server_key: s.add(&o.server_key),
        quic_port: 0,
        device_key: o.device.as_ptr(),
        device_key_len: o.device.len(),
        client_id: s.add("phone-test"),
        device_name: s.add("phone"),
        session_id: std::ptr::null(),
        argv_json: s.opt(o.argv),
        cwd: std::ptr::null(),
        cols: 80,
        rows: 24,
        width_px: 0,
        height_px: 0,
        predict: 1,
        appearance_json: std::ptr::null(),
    };
    let h = unsafe { osewa_open(&opts, cb, std::ptr::null_mut()) };
    assert!(!h.is_null());
    H.store(h, std::sync::atomic::Ordering::SeqCst);
    r.wait_state(CONNECTED);
    send(h, "echo ping-me\r");
    r.wait_screen("pong-2\n");
    H.store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
    unsafe { osewa_close(h) };
}
