//! link_test.go

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
    f.set(|st| st.commit = "other".into());
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
