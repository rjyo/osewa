//! Review findings: input backpressure, resume without replay, shutdown
//! with stuck clients.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::catchup::session_cfg;
use super::helpers::*;
use crate::queue::{Kind, INITIAL_WINDOW};
use crate::{Config, Tunables};

const WINDOW: usize = INITIAL_WINDOW as usize;

fn cfg(argv: Vec<String>) -> Config {
    Config { argv, ..Default::default() }
}

fn own() -> wire::Request {
    wire::Request { own: true, ..Default::default() }
}

fn method(m: &str) -> wire::Request {
    wire::Request { method: m.into(), ..Default::default() }
}

/// Review #1: input the child won't read must not stop the holder reading
/// CREDIT, requests or EOF from the same connection.
#[test]
fn input_backpressure_keeps_connection_live() {
    let _heavy = heavy(1);
    let h = start(Config { owner_grace: Duration::from_millis(300), ..cfg(sh("stty raw -echo; yes tick")) });
    let mut watch = h.dial("watcher");
    watch.attach(wire::Request { stream: 9, ..Default::default() });
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_text("tick");
    const N: u64 = 64; // 256 KiB, past the 64 KiB cap and the tty's queue
    // From a thread: a holder that stops reading would block these writes.
    let (w, stream) = (c.w.clone(), c.stream);
    std::thread::spawn(move || {
        let big = "x".repeat(4096);
        for i in 1..=N {
            let input = wire::Input { seq: i, ty: "text".into(), text: big.clone(), ..Default::default() };
            if wire::write(&mut *w.lock().unwrap(), &wire::json(wire::TYPE_INPUT, stream, &input)).is_err() {
                return;
            }
        }
    });
    // Output keeps flowing well past one window, so CREDIT is being read.
    let base = c.counted;
    c.wait_for("output past a window", |c| c.counted - base > 2 * WINDOW);
    assert!(c.acked < N, "all {} inputs acked: the child is reading after all", c.acked);
    // Requests and STREAM_END still get through.
    let resp = c.request(method(wire::METHOD_DETACH));
    assert!(
        resp.error.is_empty() && c.ended.as_ref().is_some_and(|e| e.reason == wire::END_DETACHED),
        "detach: {resp:?}, end {:?}",
        c.ended
    );
    // And EOF: closing the owner's connection starts the lapse.
    c.close();
    watch.wait_for("owner lapse", |c| c.has_event(|e| e.kind == wire::EVENT_OWNER && e.owner.is_empty()));
}

/// Review #2: a resume whose catch-up falls behind gets a snapshot, and no
/// record the snapshot already holds is sent after it.
#[test]
fn resume_catch_up_no_replay() {
    let _heavy = heavy(1);
    // About 600 KB of catch-up: past lag_bytes once, and the rest under it.
    let h = start(Config {
        lag_bytes: 3 << 17,
        catch_up_delay: Duration::from_secs(1),
        ..cfg(sh("read x; seq 1 75000; echo DONE; sleep 30"))
    });
    let mut c = h.dial("dev1");
    let resp = c.attach(own());
    c.wait_for("snapshot", |c| c.term.is_some());
    let pos = c.end;
    c.text_input(1, "\r");
    // Closing with frames unread makes Linux reset the socket, which can
    // lose input the holder hasn't read yet: wait until it's applied.
    c.wait_for("ack", |c| c.acked == 1);
    c.close();
    let mut w = h.dial("w");
    w.attach(wire::Request::default());
    w.wait_text("DONE");

    let mut c2 = h.dial("dev1");
    c2.end = pos;
    c2.term = Some(new_ref(40, 10));
    let r2 = c2.attach(wire::Request { epoch: resp.epoch, position: Some(pos), ..Default::default() });
    assert!(r2.resumed, "not resumed");
    // Catch-up ends with a full snapshot once nothing is left unsent, so by
    // then every record queued behind the first snapshot has arrived.
    c2.wait_for("catch-up and back", |c| {
        let n = c.gens.len();
        n >= 2 && c.gens[n - 1].history.is_none() && c.snap_ends.contains_key(&(n - 1))
    });
    c2.wait_text("DONE");
    assert_eq!(c2.stale, 0, "records replayed after the snapshot at {}", c2.gens[0].position);
}

/// Review #3: shutdown with several clients that stopped reading still ends.
#[test]
fn shutdown_with_stuck_clients() {
    let dir = TempDir::new();
    let p = payload(&dir, "screen", super::flow::busy_screen(250, 80).as_bytes());
    let mut h = start(Config { run_dir: dir.0.join("run"), cols: 250, rows: 80, ..cfg(sh(&format!("cat {p}; sleep 30"))) });
    let mut w = h.dial("w");
    w.attach(wire::Request::default());
    w.wait_for("screen", |c| c.ready_at > 0);
    let mut stuck = Vec::new();
    for i in 0..3 {
        let mut nc = UnixStream::connect(&h.sock).unwrap();
        // Attach, then never read: the snapshot fills the socket.
        let hello = wire::Hello { client_id: format!("stuck{i}"), ..Default::default() };
        wire::write(&mut nc, &wire::json(wire::TYPE_HELLO, 0, &hello)).unwrap();
        let req = wire::Request { id: 1, method: wire::METHOD_ATTACH.into(), stream: 1, ..Default::default() };
        wire::write(&mut nc, &wire::json(wire::TYPE_REQUEST, 0, &req)).unwrap();
        stuck.push(nc);
    }
    std::thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    h.cancel();
    let tune = Tunables::default();
    let r = h.done(tune.flush_wait + tune.kill_grace + Duration::from_secs(2));
    assert!(r.is_ok(), "serve = {r:?}");
    let d = start.elapsed();
    assert!(d <= tune.flush_wait + tune.kill_grace + Duration::from_secs(1), "shutdown took {d:?}");
}

/// Review #4: STREAM_END goes ahead of the detach RESPONSE even when the
/// stream's data is waiting for credit.
#[test]
fn detach_end_before_response() {
    let _heavy = heavy(1);
    let h = start(cfg(sh("read x; seq 1 400000; sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_for("snapshot", |c| c.term.is_some());
    c.no_credit = true;
    c.text_input(1, "\r");
    c.wait_for("a full window", |c| c.counted > WINDOW / 2);
    std::thread::sleep(Duration::from_millis(50));
    let resp = c.request(method(wire::METHOD_DETACH));
    assert!(
        resp.error.is_empty() && c.ended.as_ref().is_some_and(|e| e.reason == wire::END_DETACHED),
        "RESPONSE {resp:?} came before STREAM_END ({:?})",
        c.ended
    );
    // Replacing a stream: the old STREAM_END comes before the new RESPONSE.
    c.no_credit = false;
    c.attach(wire::Request { stream: 1, ..Default::default() });
    c.text_input(2, "");
    c.ended = None;
    c.no_credit = true;
    std::thread::sleep(Duration::from_millis(50));
    c.attach(wire::Request { stream: 2, ..Default::default() });
    assert!(c.ended.is_some(), "attach RESPONSE came before the replaced stream's STREAM_END");
}

/// Review #5: events raised by output that a catch-up snapshot skips don't
/// go out after it, so their effects never fire.
#[test]
fn skipped_output_effects_dont_fire() {
    local(async {
        let s = unit_session(session_cfg(80, 24, 1000));
        let q = add_conn(&s, 1, "o", Some(unit_stream(1, 1, 0, false)));
        s.borrow_mut().core.owner = "o".into();
        let mut out = vec![b'x'; 9000];
        out.extend_from_slice(b"\x1b]52;c;aGk=\x07\x07");
        s.borrow_mut().handle_output(&out);
        let q = q.borrow();
        let begin = q.data.iter().position(|e| e.is_marker(wire::TYPE_SNAPSHOT_BEGIN)).expect("no catch-up snapshot");
        for e in q.data.range(begin..) {
            // The owner and input mode restored after the screen are this
            // stream's own records, at the screen's position.
            if e.kind == Kind::Record {
                assert!(e.rec().only.is_some(), "record {:?} after the snapshot that covers it", e.rec().data);
            }
        }
    });
}

/// Review #6: no signal goes to the child's group once it has been reaped.
#[test]
fn no_signal_after_reap() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let s2 = sent.clone();
    crate::session::KILL_GROUP.with(|k| *k.borrow_mut() = Some(Box::new(move |_, sig| s2.borrow_mut().push(sig))));
    let s = unit_session(session_cfg(10, 10, 1000));
    let mut b = s.borrow_mut();
    b.core.pid = 1 << 30;
    b.core.signal_group(libc::SIGHUP);
    b.core.reaped.set(true);
    b.core.signal_group(libc::SIGKILL);
    crate::session::KILL_GROUP.with(|k| *k.borrow_mut() = None);
    assert_eq!(*sent.borrow(), [libc::SIGHUP], "signals");
}

/// Review #7: a stream ended by a snapshot failure starts the owner's lapse.
#[test]
fn snapshot_error_starts_lapse() {
    let tune = Tunables { fail_encode: true, ..Default::default() };
    let h = start(Config { owner_grace: Duration::from_millis(200), tune, ..cfg(sh("sleep 30")) });
    let mut c = h.dial("dev1");
    c.attach(own());
    c.wait_for("stream end", |c| c.ended.is_some());
    assert_eq!(c.ended.as_ref().unwrap().reason, wire::END_ERROR, "end {:?}", c.ended);
    let mut k = h.dial("");
    assert!(eventually(Duration::from_secs(2), || k.info().owner.is_empty()), "ownership never lapsed");
}

/// Security: sizes past wire::MAX_COLS/MAX_ROWS are refused everywhere a
/// client can set one.
#[test]
fn size_caps() {
    let dir = TempDir::new();
    let big = Config { run_dir: dir.0.clone(), id: "x".into(), cols: wire::MAX_COLS + 1, rows: 10, argv: sh("true"), ..Default::default() };
    assert!(crate::run(big, || {}, false).is_err(), "oversized config accepted");
    let h = start(cfg(sh("sleep 30")));
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, cols: wire::MAX_COLS + 1, rows: 50, ..Default::default() });
    let info = c.info();
    assert!(info.cols <= wire::MAX_COLS, "attach resized to {}x{}", info.cols, info.rows);
    let resp = c.request(wire::Request { stream: 7, cols: 100, rows: wire::MAX_ROWS + 1, ..method(wire::METHOD_RESIZE) });
    assert_eq!(resp.error, wire::ERR_BAD_REQUEST, "resize: {resp:?}");
    let resp = c.request(wire::Request { stream: 7, cols: wire::MAX_COLS, rows: wire::MAX_ROWS, ..method(wire::METHOD_RESIZE) });
    assert!(resp.error.is_empty(), "max size refused: {resp:?}");
}

/// Security: a run directory others can enter is refused.
#[test]
fn run_dir_must_be_private() {
    let dir = TempDir::new();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let c = Config { run_dir: dir.0.clone(), id: "x".into(), cols: 10, rows: 10, argv: sh("true"), ..Default::default() };
    assert!(crate::run(c, || {}, false).is_err(), "served from a 0755 run dir");
}
