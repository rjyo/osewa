//! robust_test.go

use super::*;
use crate::session::ACK_BYTES;

/// Dials with a keepalive interval of d.
pub(super) fn dial_ping(f: &Fake, client_id: &str, d: Duration) -> Conn {
    let mut cfg = f.config(client_id);
    cfg.knobs.ping = Some(d);
    let c = run(Conn::dial(cfg)).unwrap();
    run(c.welcome()).unwrap();
    c
}

#[test]
fn keepalive_detects_half_open_conn() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = dial_ping(&f, "a", Duration::from_millis(30));
    let s = attach(&c, &id, Default::default());
    f.output(&id, "before");
    s.wait_text("before");

    // Pongs keep a healthy connection alive past several intervals.
    sleep(Duration::from_millis(200));
    assert!(s.connected(), "healthy connection declared dead");

    let mut stall = f.stall();
    // The fake checks the stall between reads; one frame gets it there.
    s.send_text("").unwrap();
    wait_for("conn death", c.done());
    let err = c.err().unwrap();
    assert!(err.is_closed() && err.to_string().contains("no pong"), "err = {err}");
    wait_for("lost", s.lost());
    stall.release();
    run(s.resume(&f.dial("a"))).unwrap();
    f.output(&id, " after");
    s.wait_text("before after");
}

/// Input backpressure (a holder not reading) must not stop output: the
/// reader never writes to the network itself.
#[test]
fn output_flows_while_input_is_backed_up() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = ready(&f.dial("a"), &id, own());

    let _stall = f.stall();
    s.send_text("").unwrap();
    let paste = "p".repeat(512 << 10);
    let ss = s.s.clone();
    std::thread::spawn(move || {
        for _ in 0..64 {
            if ss.paste(&paste).is_err() {
                return;
            }
        }
    });
    sleep(Duration::from_millis(300));
    f.output(&id, &"x".repeat(ACK_BYTES as usize + 1)); // crosses the ACK threshold
    f.output(&id, "\r\nmarker");
    s.wait_text("marker");
}

#[test]
fn cut_snapshot_resumes_with_fresh_snapshot() {
    let f = Fake::new();
    let id = f.create(40, 10);
    for _ in 0..300 {
        f.output(&id, "history line\r\n");
    }
    f.set(|st| st.drop_after_ready = true);
    let s = attach(&f.dial("a"), &id, Default::default());
    wait_for("lost", s.lost());
    run(s.resume(&f.dial("a"))).unwrap();
    assert!(!f.lock().attaches.last().unwrap().resumed, "resumed onto a mirror missing its history");
    f.output(&id, "after");
    s.wait_text("after");
}

#[test]
fn resume_does_not_take_ownership() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let a = attach(&f.dial("a"), &id, own());
    let b = attach(&f.dial("b"), &id, Default::default());
    run(b.own()).unwrap();
    eventually("a sees b own", || a.owner() == "b");

    f.drop_all();
    wait_for("lost", a.lost());
    run(a.resume(&f.dial("a"))).unwrap();
    assert_eq!(f.with(&id, |fs| fs.owner.clone()), "b", "owner after a's resume");
}

#[test]
fn abandoned_attach_is_detached() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = f.dial("a");
    f.set(|st| st.attach_delay = Duration::from_millis(200));
    let r = run_for(Duration::from_millis(50), c.attach(&id, AttachOptions { cols: 40, rows: 10, own: true, ..Default::default() }));
    assert!(r.is_none(), "attach finished within 50 ms");
    eventually("late attach", || f.lock().attaches.len() == 1);
    eventually("stream detached", || f.stream_count(&id) == 0);

    f.set(|st| st.attach_delay = Duration::ZERO);
    let s = attach(&c, &id, Default::default());
    f.output(&id, "ok");
    s.wait_text("ok");
}

#[test]
fn stream_error_is_resumable() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = f.dial("a");
    let s = attach(&c, &id, Default::default());
    f.output(&id, "one ");
    s.wait_text("one");

    f.end_streams(&id, wire::END_ERROR);
    wait_for("lost", s.lost());
    assert!(!s.is_done(), "stream error ended the session: {:?}", s.err());
    run(s.resume(&c)).unwrap();
    f.output(&id, "two");
    s.wait_text("one two");
}

#[test]
fn flush_waits_for_acks() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = ready(&f.dial("a"), &id, own());
    f.set(|st| st.no_ack = true);
    s.send_text("a").unwrap();
    assert!(run_for(Duration::from_millis(50), s.flush()).is_none(), "flush without ack");
    f.set(|st| st.no_ack = false);
    s.send_text("b").unwrap();
    run(s.flush()).unwrap();
    s.close();
    // Nothing pending.
    run(s.flush()).unwrap();
}

/// Closes the connection that takes the next attach response, after the
/// response leaves pending but before its hook runs.
fn fail_before_hook(cfg: &mut Config) {
    let once = Arc::new(std::sync::Once::new());
    cfg.knobs.response_taken = Some(Arc::new(move |c: &Conn, r: &wire::Response| {
        if r.epoch.is_empty() {
            return; // not an attach
        }
        once.call_once(|| c.close_now());
    }));
}

fn dial_cfg(f: &Fake, cfg: Config) -> Conn {
    let _ = f;
    let c = run(Conn::dial(cfg)).unwrap();
    run(c.welcome()).unwrap();
    c
}

/// A connection that dies as the attach response arrives must fail the
/// attach: the session would otherwise sit on a dead connection that its
/// lost() already passed over.
#[test]
fn attach_conn_dies_before_hook() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let mut cfg = f.config("a");
    fail_before_hook(&mut cfg);
    let c = dial_cfg(&f, cfg);
    let r = run(c.attach(&id, opts(40, 10)));
    assert!(r.is_err(), "attach succeeded on a connection that died");
}

/// The same for resume: it fails, so the caller resumes again elsewhere.
#[test]
fn resume_conn_dies_before_hook() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach_raw(&f.dial("a"), &id, opts(40, 10));
    let mut cfg = f.config("a");
    fail_before_hook(&mut cfg);
    let c2 = dial_cfg(&f, cfg);
    assert!(run(s.resume(&c2)).is_err(), "resume succeeded on a connection that died");
    run(s.resume(&f.dial("a"))).unwrap();
    s.close();
}
