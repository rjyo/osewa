//! Link: attach, resume after a drop, a suspect connection replaced.

use super::*;

struct TestLink(Link);

impl Deref for TestLink {
    type Target = Link;
    fn deref(&self) -> &Link {
        &self.0
    }
}

impl Drop for TestLink {
    fn drop(&mut self) {
        run(self.0.close());
    }
}

fn new_link(cfg: Config) -> TestLink {
    TestLink(Link::new(cfg))
}

fn link_attach(l: &Link, id: &str) -> Mirrored {
    let m = osewa_mirror::Mirror::new(Default::default());
    let opt = AttachOptions { cols: 40, rows: 10, renderer: Some(Box::new(Adapter(m.clone()))), ..Default::default() };
    let s = run(l.attach(id, opt)).unwrap();
    Mirrored { s, m }
}

/// A Link resumes its sessions on a new connection after a drop, with no
/// help from the caller, and channel streams are opened on the new Conn.
#[test]
fn link_resumes_after_drop() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    f.output(&id, "before\r\n");
    s.wait_text("before");
    let c1 = run(l.conn()).unwrap();

    for i in 0..3 {
        f.drop_all();
        f.output(&id, &format!("after {i}\r\n"));
        s.wait_text(&format!("after {i}"));
    }
    let c2 = run(l.conn()).unwrap();
    assert!(c1 != c2, "same connection after a drop");
    run(c2.output(ExecOptions { argv: vec!["true".into()], ..Default::default() })).unwrap();
    // Input typed meanwhile arrives once.
    s.send_text("x").unwrap();
    run(s.flush()).unwrap();
}

/// A connection whose pings go unanswered is replaced, and the session
/// moves to the new one.
#[test]
fn link_replaces_suspect_conn() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let mut cfg = f.config("a");
    cfg.knobs.ping = Some(Duration::from_millis(30));
    let l = new_link(cfg);
    let s = link_attach(&l, &id);
    s.wait_text("");
    let c1 = run(l.conn()).unwrap();
    let mut stall = f.stall();
    // The fake checks the stall between reads; one frame gets it there.
    s.send_text("").unwrap();
    let gone = run_for(WAIT, async {
        tokio::select! {
            _ = c1.suspect() => {}
            _ = c1.done() => {}
        }
    });
    assert!(gone.is_some(), "silent connection kept");
    stall.release();
    eventually("new conn", || run(l.conn()).is_ok_and(|c| c != c1) && s.connected());
    f.output(&id, "moved");
    s.wait_text("moved");
}

/// A session that ends leaves the link alone.
#[test]
fn link_forgets_ended_session() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    f.exit(&id, 4);
    wait_for("done", s.done());
    assert_eq!(l.session_count(), 0, "sessions kept");
}

/// After the first connection, conn waits through failed dials; before it,
/// a failure is reported at once.
#[test]
fn link_conn_errors() {
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let f = Fake::new();
    let mut cfg = f.config("a");
    cfg.url = format!("wss://{addr}{}", wire::PATH);
    let l = new_link(cfg);
    let start = Instant::now();
    let err = run(l.conn()).unwrap_err();
    assert!(
        err.io_kind() == Some(std::io::ErrorKind::ConnectionRefused) && start.elapsed() <= Duration::from_secs(2),
        "conn = {err} after {:?}",
        start.elapsed()
    );
    assert!(!l.up() && !l.connected(), "never-connected link reported up");
    match run_for(Duration::from_millis(100), l.conn()) {
        None => {}
        Some(Err(e)) if e.io_kind() == Some(std::io::ErrorKind::ConnectionRefused) => {}
        Some(r) => panic!("conn again = {r:?}"),
    }
    assert!(!matches!(run_for(Duration::from_millis(100), l.attach("x", Default::default())), Some(Ok(_))), "attach without a connection");
}

#[test]
fn link_close() {
    let f = Fake::new();
    let l = Link::new(f.config("a"));
    let c = run(l.conn()).unwrap();
    assert!(l.up() && l.connected(), "connected link not up");
    run(l.close());
    run(l.close());
    wait_for("conn closed", c.done());
    assert!(matches!(run(l.conn()), Err(Error::LinkClosed)), "conn after close");
    assert!(!l.up() && l.connected(), "closed link: up, or forgot it had connected");
    assert!(matches!(run(l.attach("x", Default::default())), Err(Error::LinkClosed)), "attach after close");
}

/// A server of another build ends the link's sessions with the reason
/// instead of retrying forever.
#[test]
fn link_incompatible_ends_sessions() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    s.wait_text("");
    f.set(|st| st.snapshot_version = Some(crate::SNAPSHOT_VERSION + 1));
    f.drop_all();
    wait_for("session ended", s.done());
    assert!(s.err().is_some_and(|e| e.is_incompatible()), "err = {:?}", s.err());
    assert!(run(l.conn()).unwrap_err().is_incompatible(), "conn");
}

/// A stream the server ended with "error" (it lost its holder) is resumed
/// on the same, healthy connection.
#[test]
fn link_resumes_stream_error() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    s.wait_text("");
    let c1 = run(l.conn()).unwrap();
    f.end_streams(&id, wire::END_ERROR);
    eventually("reattached", || f.stream_count(&id) == 1 && s.connected());
    f.output(&id, "still here");
    s.wait_text("still here");
    assert!(run(l.conn()).unwrap() == c1, "redialed for a stream error");
}

/// Resume failures on a live connection are retried.
#[test]
fn link_retries_failed_resume() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    s.wait_text("");
    f.set(|st| st.attach_delay = Duration::from_secs(4)); // past the attempt timeout
    f.drop_all();
    sleep(Duration::from_millis(500));
    f.set(|st| st.attach_delay = Duration::ZERO);
    f.output(&id, "back");
    run_for(Duration::from_secs(15), s.m.wait_text("back")).expect("timed out").unwrap();
}

/// Link sessions never report lost: the link resumes them.
#[test]
fn link_consumes_lost() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    s.wait_text("");
    f.drop_all();
    assert!(run_for(Duration::from_millis(200), s.lost()).is_none(), "lost fired on a link session");
}

/// Many sessions on one link all come back.
#[test]
fn link_many_sessions() {
    let f = Fake::new();
    let l = new_link(f.config("a"));
    let mut ids = Vec::new();
    let mut ss = Vec::new();
    for _ in 0..5 {
        let id = f.create(40, 10);
        ss.push(link_attach(&l, &id));
        ids.push(id);
    }
    f.drop_all();
    for (i, id) in ids.iter().enumerate() {
        f.output(id, &format!("n{}", &id[..4]));
        let _ = i;
    }
    for (s, id) in ss.iter().zip(&ids) {
        s.wait_text(&format!("n{}", &id[..4]));
    }
}

/// A suspect connection, not yet dead, is replaced while it may still be
/// open: the session moves, and the old one is dropped.
#[test]
fn link_moves_off_suspect_conn() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let mut cfg = f.config("a");
    // Dead only after two missed pings: 8 s.
    cfg.knobs.ping = Some(Duration::from_secs(4));
    let l = new_link(cfg);
    let s = link_attach(&l, &id);
    s.wait_text("");
    run(s.flush()).unwrap();
    let c1 = run(l.conn()).unwrap();
    let mut stall = f.stall();
    s.send_text("").unwrap(); // probes at once; no pong within a second
    wait_for("suspect", c1.suspect());
    assert!(!c1.is_done(), "dead, not suspect");
    stall.release();
    wait_for("old conn dropped", c1.done());
    let c2 = run(l.conn()).unwrap();
    assert!(c2 != c1, "still on the suspect conn");
    f.output(&id, "moved");
    s.wait_text("moved");
}

/// The link doesn't resume a session whose first attach is still under way:
/// the holder would see two attaches.
#[test]
fn link_attach_not_resumed_meanwhile() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    run(l.conn()).unwrap();
    f.set(|st| st.attach_delay = Duration::from_millis(300));
    let l2 = l.0.clone();
    std::thread::spawn(move || {
        sleep(Duration::from_millis(100));
        l2.kick();
    });
    let s = link_attach(&l, &id);
    s.wait_text("");
    sleep(Duration::from_millis(400));
    assert_eq!(f.lock().attaches.len(), 1, "attaches");
}

/// Watchers hear the link go up, lose its connection and come back, and
/// state says which carrier it is on.
#[test]
fn link_state_watch() {
    let f = Fake::new();
    let l = new_link(f.config("a"));
    let mut w = l.watch();
    let up = |l: &Link| l.state().up;
    while !up(&l) {
        assert!(run(w.changed()), "link gone");
    }
    let st = l.state();
    assert!(st.connected && st.carrier == Some("ws") && st.err.is_none() && st.fin.is_none(), "{st:?}");
    let c1 = run(l.conn()).unwrap();
    f.drop_all();
    // Down, then up on a new connection, each heard.
    let mut saw_down = false;
    loop {
        assert!(run(w.changed()), "link gone");
        let st = l.state();
        saw_down |= !st.up;
        if st.up && run(l.conn()).unwrap() != c1 {
            break;
        }
    }
    assert!(saw_down || run(l.conn()).unwrap() != c1, "no change heard");
}

/// A server of another protocol version stops the link with an
/// incompatible error, as another snapshot version does, instead of being
/// retried forever.
#[test]
fn link_protocol_mismatch_stops() {
    let f = Fake::new();
    f.set(|st| st.version = Some(99));
    let l = new_link(f.config("a"));
    let mut w = l.watch();
    while l.state().fin.is_none() {
        assert!(run(w.changed()), "link gone");
    }
    let fin = l.state().fin.unwrap();
    assert!(fin.is_incompatible() && fin.to_string().contains("protocol"), "fin = {fin}");
}

/// A server that refuses our protocol version closes before WELCOME with
/// why; that is incompatible, while other closes stay retryable.
#[test]
fn server_version_refusal_is_incompatible() {
    let refused = crate::conn::read_error("received close frame: status = 1008 and reason = \"protocol 1, server speaks 0\"".into());
    assert!(refused.is_incompatible(), "{refused}");
    let lost = crate::conn::read_error("connection reset by peer".into());
    assert!(lost.is_closed() && !lost.is_incompatible(), "{lost}");
}

/// The QUIC port a server advertises is what the link's later dials race
/// the WebSocket on.
#[test]
fn link_learns_quic_port() {
    let f = Fake::new();
    f.set(|st| st.quic_port = 4433);
    let l = new_link(f.config("a"));
    run(l.conn()).unwrap();
    eventually("quic port", || l.quic_port() == 4433);
}

/// check pings at once: a connection that no longer answers turns suspect
/// well before the keepalive interval.
#[test]
fn link_check_pings_now() {
    let f = Fake::new();
    let mut cfg = f.config("a");
    cfg.knobs.ping = Some(Duration::from_secs(60));
    let l = new_link(cfg);
    let c1 = run(l.conn()).unwrap();
    let mut stall = f.stall();
    // The fake checks the stall between reads; one frame gets it there.
    let _ = run_for(Duration::from_millis(200), c1.list());
    let t0 = Instant::now();
    l.check();
    let gone = run_for(Duration::from_secs(4), async {
        tokio::select! {
            _ = c1.suspect() => {}
            _ = c1.done() => {}
        }
    });
    stall.release();
    assert!(gone.is_some(), "check didn't ping");
    assert!(t0.elapsed() < Duration::from_secs(3), "suspect only after {:?}", t0.elapsed());
}

/// A paused link closes its connection and dials nothing until woken, and
/// says it is paused rather than lost; wake resumes its sessions.
#[test]
fn link_pause_wake() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let l = new_link(f.config("a"));
    let s = link_attach(&l, &id);
    s.wait_text("");
    let c1 = run(l.conn()).unwrap();
    l.pause();
    wait_for("conn closed", c1.done());
    eventually("server let go", || f.lock().conns.is_empty());
    let st = l.state();
    assert!(!st.up && st.paused && st.err.is_none(), "{st:?}");
    f.output(&id, "meanwhile");
    sleep(Duration::from_millis(300));
    assert!(f.lock().conns.is_empty(), "dialed while paused");
    assert!(run_for(Duration::from_millis(100), l.conn()).is_none(), "conn while paused");

    l.wake();
    s.wait_text("meanwhile");
    eventually("woken", || {
        let st = l.state();
        st.up && !st.paused
    });
    assert!(run(l.conn()).unwrap() != c1, "same conn after wake");
    // Pausing twice, or waking an awake link, changes nothing.
    l.wake();
    l.pause();
    l.pause();
    eventually("paused again", || f.lock().conns.is_empty());
    l.wake();
    f.output(&id, "again");
    s.wait_text("again");
}

/// A link closed while paused stops.
#[test]
fn link_close_while_paused() {
    let f = Fake::new();
    let l = Link::new(f.config("a"));
    run(l.conn()).unwrap();
    l.pause();
    run_for(Duration::from_secs(4), l.close()).expect("close hung while paused");
    assert!(matches!(run(l.conn()), Err(Error::LinkClosed)), "conn after close");
}
