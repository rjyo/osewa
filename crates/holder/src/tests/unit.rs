//! Ported from unit_test.go.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;
use tokio::io::unix::AsyncFd;

use super::catchup::session_cfg;
use super::helpers::*;
use super::main::W2Loop;
use crate::input::{PtyInput, MAX_PENDING_INPUT, MAX_REPLY_BACKLOG};
use crate::log::{OutputLog, Record, REC_EVENT, REC_OUTPUT};
use crate::queue::{SendQueue, INITIAL_WINDOW, MAX_FRAME_DATA, MIN_FRAME_DATA, MIN_WINDOW};
use crate::session::MAX_PARKED_INPUT;

#[test]
fn log_ring() {
    let mut l = OutputLog::new(30);
    let a = l.append(REC_OUTPUT, b"aaaaa", false, None); // 0..10
    let b = l.append(REC_EVENT, b"bbbbb", true, None); // 10..20
    assert_eq!((a.end(), b.start, l.end), (10, 10, 20));
    let recs = l.since(10).unwrap();
    assert!(recs.len() == 1 && recs[0].owner_only);
    l.append(REC_OUTPUT, b"ccccc", false, None); // 20..30, evicts nothing yet
    l.append(REC_OUTPUT, b"ddddd", false, None); // 30..40, evicts a
    assert_eq!((l.start(), l.size), (10, 30));
    for (p, want) in [(0, false), (10, true), (15, false), (40, true), (41, false)] {
        assert_eq!(l.since(p).is_some(), want, "since {p}");
    }
    // One record larger than the cap is still kept.
    l.append(REC_OUTPUT, &[0; 100], false, None);
    assert_eq!((l.len(), l.start()), (1, 40));
    assert_eq!(OutputLog::new(0).start(), 0);
    // Go's last check, record.frame, has no Rust counterpart: the queue
    // builds frames from records itself.
}

pub(crate) struct FakeClock(Rc<Cell<Instant>>);

impl FakeClock {
    pub fn new() -> FakeClock {
        FakeClock(Rc::new(Cell::new(Instant::now())))
    }

    pub fn queue(&self) -> SendQueue {
        let c = self.0.clone();
        SendQueue::new(Rc::new(move || c.get()))
    }

    pub fn add(&self, d: Duration) {
        self.0.set(self.0.get() + d);
    }
}

fn records(n: usize) -> Vec<crate::log::Record> {
    let mut l = OutputLog::new(1 << 30);
    (0..n).map(|_| l.append(REC_OUTPUT, &[b'x'; 1000], false, None)).collect()
}

pub(crate) fn drain(q: &mut SendQueue) -> Vec<wire::Frame> {
    let mut buf = Vec::new();
    while q.next_into(&mut buf).is_some() {}
    let mut out = Vec::new();
    wire::parse_frames(&buf, &mut out).unwrap();
    out
}

#[test]
fn queue_lagging() {
    let clock = FakeClock::new();
    let mut q = clock.queue();
    q.reset_flow(1);
    assert!(!q.lagging(100, Duration::from_secs(3600)), "empty queue lagging");
    let recs = records(3);
    q.record(1, recs[0].clone());
    assert!(!q.lagging(5000, Duration::from_secs(3600)) && q.lagging(500, Duration::from_secs(3600)), "lag by bytes");
    clock.add(Duration::from_millis(20));
    // No round trip measured yet: age doesn't count.
    assert!(!q.lagging(5000, Duration::from_millis(10)));
    q.srtt = Duration::from_millis(1);
    assert!(q.lagging(5000, Duration::from_millis(10)) && !q.lagging(5000, Duration::ZERO), "lag by age");
    // Age is relative to the link: two round trips at least.
    q.srtt = Duration::from_millis(50);
    assert!(!q.lagging(5000, Duration::from_millis(10)), "behind within two round trips");
    assert!(q.keeping_up(Duration::from_secs(3600)));
    q.srtt = Duration::from_millis(5);
    assert!(!q.keeping_up(Duration::from_secs(3600)));
    // A snapshot not yet at READY gets up to twice the bytes, and no age.
    q.drop_data();
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_READY, 1, vec![]), 0);
    q.record(1, recs[1].clone());
    clock.add(Duration::from_millis(20));
    assert!(!q.lagging(600, Duration::from_millis(10)) && q.lagging(400, Duration::from_millis(10)), "READY grace");
    drain(&mut q);
    assert!(!q.lagging(1, Duration::from_millis(10)));
    assert_eq!((q.lag_bytes(), q.ready), (0, 0));
}

#[test]
fn queue_coalesces_and_respects_window() {
    let clock = FakeClock::new();
    let mut q = clock.queue();
    q.reset_flow(1);
    // Small records merge up to the frame size.
    let mut l = OutputLog::new(1 << 30);
    for _ in 0..10 {
        q.record(1, l.append(REC_OUTPUT, &[b'y'; 1000], false, None));
    }
    let frames = drain(&mut q);
    assert_eq!(frames.len(), 3, "{frames:?}");
    let (end, data) = wire::split_end(&frames[0].payload).unwrap();
    assert_eq!((data.len(), end), (4000, 4 * 1005));
    // An event never merges.
    q.record(1, l.append(REC_EVENT, b"{}", false, None));
    q.record(1, l.append(REC_OUTPUT, b"z", false, None));
    let frames = drain(&mut q);
    assert_eq!(frames.iter().map(|f| f.ty).collect::<Vec<_>>(), [wire::TYPE_EVENT, wire::TYPE_OUTPUT]);
    // Past the window nothing goes until credit.
    let mut q = clock.queue();
    q.reset_flow(1);
    for r in records(80) {
        q.record(1, r);
    }
    let sent: usize = drain(&mut q).iter().map(|f| f.payload.len()).sum();
    assert!(sent <= crate::queue::INITIAL_WINDOW as usize, "{sent}");
    assert!(q.credit(1, sent as u32).is_none());
    assert!(!drain(&mut q).is_empty());
    // Control frames jump ahead and never wait.
    q.control(wire::input_ack(1, 9));
    assert_eq!(drain(&mut q)[0].ty, wire::TYPE_INPUT_ACK);
}

struct FailWriter;

impl tokio::io::AsyncWrite for FailWriter {
    fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[test]
fn send_queue_write_error() {
    let clock = FakeClock::new();
    let q = RefCell::new(clock.queue());
    q.borrow_mut().control(wire::Frame::new(wire::TYPE_RESPONSE, 0, vec![0; 70 << 10]));
    let r = local(crate::run_writer(&q, &tokio::sync::Notify::new(), FailWriter, None, |_| {}));
    assert!(r.is_err(), "no error");
}

/// A pipe for the PTY writer: the read end for the child, the write end
/// non-blocking, for an AsyncFd.
fn pipe() -> (std::fs::File, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe {
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
        (std::fs::File::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    }
}

/// Starts write_pty for s's input on w.
fn spawn_writer(s: &Rc<RefCell<crate::session::Session>>, w: OwnedFd) -> tokio::task::JoinHandle<()> {
    let (input, wake) = {
        let b = s.borrow();
        (b.core.input.clone(), b.core.input_wake.clone())
    };
    // Sound: the AsyncFd owns w, and nothing else closes it.
    let master = Rc::new(unsafe { AsyncFd::register(w) }.unwrap());
    tokio::task::spawn_local(crate::write_pty(Rc::downgrade(s), master, input, wake))
}

/// Lets the runtime run until cond holds or WAIT passes.
async fn until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

/// The input cap: a child that never reads holds pending at the cap, and the
/// writer makes room once it reads.
#[test]
fn pty_writer_cap() {
    local(async {
        let s = unit_session(session_cfg(10, 10, 1000));
        let input = s.borrow().core.input.clone();
        let (r, w) = pipe();
        let writer = spawn_writer(&s, w);
        // Go's io.Pipe holds nothing; a kernel pipe takes up to 64 KiB
        // before its writer blocks, so this pushes that much more.
        let pipe_slack = 128 << 10;
        input.borrow_mut().push(&vec![0; MAX_PENDING_INPUT + pipe_slack + 10], false);
        s.borrow().core.input_wake.notify_one();
        assert!(input.borrow().full(), "not full");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(input.borrow().full(), "room with the child not reading");
        // Replies past the backlog cap are dropped, input is not.
        input.borrow_mut().push(&vec![0; MAX_REPLY_BACKLOG], false);
        let before = input.borrow().pending();
        input.borrow_mut().push(b"reply", true);
        assert_eq!(input.borrow().pending(), before, "reply queued past the backlog cap");
        let reader = std::thread::spawn(move || std::io::copy(&mut { r }, &mut std::io::sink()));
        assert!(until(|| !input.borrow().full()).await, "no room after the child read");
        input.borrow_mut().close();
        s.borrow().core.input_wake.notify_one();
        tokio::time::timeout(WAIT, writer).await.expect("writer still running").unwrap();
        drop(reader);

        let mut p2 = PtyInput::new(None);
        p2.close();
        assert!(!p2.full(), "closed writer full");
        let s2 = unit_session(session_cfg(10, 10, 1000));
        s2.borrow().core.input.borrow_mut().close();
        let (r2, w2) = pipe();
        // Returns at once.
        tokio::time::timeout(WAIT, spawn_writer(&s2, w2)).await.expect("closed writer ran").unwrap();
        drop(r2);

        let s3 = unit_session(session_cfg(10, 10, 1000));
        let (r3, w3) = pipe();
        drop(r3);
        s3.borrow().core.input.borrow_mut().push(b"x", false);
        // A write error closes it.
        tokio::time::timeout(WAIT, spawn_writer(&s3, w3)).await.expect("failed writer ran on").unwrap();
        assert_eq!(s3.borrow().core.input.borrow().pending(), 0, "failed writer kept bytes");
    });
}

/// Past the pending cap, input is parked unacked, in order, and applied and
/// acked once the child reads; the connection is never stopped from reading.
#[test]
fn input_parking() {
    local(async {
        let s = unit_session(session_cfg(10, 10, 1000));
        s.borrow_mut().core.owner = "a".into();
        let (mut r, w) = pipe();
        spawn_writer(&s, w);
        let q = add_conn(&s, 1, "a", Some(unit_stream(1, 1, 0, false)));
        let text = "\0".repeat(4096);
        for i in 1..=40 {
            let input = wire::Input { seq: i, ty: "text".into(), text: text.clone(), ..Default::default() };
            s.borrow_mut().handle_input(1, input, 4096);
        }
        let (n, last) = acks(&q);
        let parked = s.borrow().conns[&1].parked.len();
        assert!(last < 40 && parked != 0 && last as usize + parked == 40, "acked {n} (last {last}), parked {parked}");

        let got = std::thread::spawn(move || {
            let mut b = vec![0; 40 * 4096];
            r.read_exact(&mut b).map(|_| b.len())
        });
        // The writer reports room, which unparks.
        let drained = until(|| s.borrow().conns[&1].parked.is_empty()).await;
        assert!(drained, "stuck with {} parked", s.borrow().conns[&1].parked.len());
        let got = got.join().unwrap().unwrap();
        let (_, last) = acks(&q);
        assert!(last == 40 && got == 40 * 4096, "last ack {last}, child read {got}");
        s.borrow().core.input.borrow_mut().close();
        s.borrow().core.input_wake.notify_one();
    });
}

/// Review 2 #2: past the parking cap, input is rejected deterministically:
/// acked in order, never applied, and the connection stays up, so a client
/// can't loop resending it.
#[test]
fn input_parking_limit() {
    let s = unit_session(session_cfg(10, 10, 1000));
    s.borrow_mut().core.owner = "a".into();
    // No writer: stays full.
    s.borrow().core.input.borrow_mut().push(&vec![0; MAX_PENDING_INPUT], false);
    let q = add_conn(&s, 1, "a", Some(unit_stream(1, 1, 0, false)));
    let n = MAX_PARKED_INPUT / 4096 + 10;
    for i in 1..=n as u64 {
        s.borrow_mut().handle_input(1, wire::Input { seq: i, ty: "text".into(), text: "x".into(), ..Default::default() }, 4096);
    }
    {
        let b = s.borrow();
        let c = &b.conns[&1];
        let drops = c.parked.iter().filter(|p| p.drop).count();
        assert!(
            c.parked.len() == n && drops != 0 && c.parked_bytes <= MAX_PARKED_INPUT,
            "parked {}, {drops} dropped, {} bytes",
            c.parked.len(),
            c.parked_bytes
        );
    }
    assert_eq!(acks(&q).0, 0, "acks while parked");
    // The child reads (here: the writer goes away): everything is acked.
    s.borrow().core.input.borrow_mut().close();
    s.borrow_mut().unpark();
    let (_, last) = acks(&q);
    let b = s.borrow();
    let c = &b.conns[&1];
    assert!(last == n as u64 && c.parked.is_empty() && c.parked_bytes == 0, "last ack {last} of {n}, {} left", c.parked.len());
}

/// Review 2 #5: a viewer's input is acked at once while the owner's waits.
#[test]
fn viewer_input_not_parked() {
    let s = unit_session(session_cfg(10, 10, 1000));
    s.borrow_mut().core.owner = "a".into();
    s.borrow().core.input.borrow_mut().push(&vec![0; MAX_PENDING_INPUT], false);
    let owner = add_conn(&s, 1, "a", Some(unit_stream(1, 1, 0, false)));
    let viewer = add_conn(&s, 2, "b", Some(unit_stream(2, 2, 0, false)));
    let input = |text: &str| wire::Input { seq: 1, ty: "text".into(), text: text.into(), ..Default::default() };
    s.borrow_mut().handle_input(1, input("x"), 10);
    s.borrow_mut().handle_input(2, input("y"), 10);
    assert!(acks(&owner).0 == 0 && s.borrow().conns[&1].parked.len() == 1, "owner input not parked");
    assert!(acks(&viewer).1 == 1 && s.borrow().conns[&2].parked.is_empty(), "viewer input parked");
}

fn counted(fs: &[wire::Frame]) -> usize {
    fs.iter()
        .filter(|f| matches!(f.ty, wire::TYPE_SNAPSHOT_HISTORY | wire::TYPE_OUTPUT | wire::TYPE_EVENT))
        .map(|f| f.payload.len())
        .sum()
}

#[test]
fn queue_coalesces_within_frame_size() {
    let mut q = FakeClock::new().queue();
    q.reset_flow(1);
    let recs = records(10);
    for r in &recs {
        q.record(1, r.clone());
    }
    let ev = Record { start: recs[9].end(), kind: REC_EVENT, data: Rc::from(&br#"{"kind":"bell"}"#[..]), owner_only: false, only: None };
    q.record(1, ev);
    let fs = drain(&mut q);
    // 4 KiB frames hold four 1000-byte records; the event goes alone.
    assert!(fs.len() == 4 && fs[3].ty == wire::TYPE_EVENT, "{} frames", fs.len());
    let (end, data) = wire::split_end(&fs[0].payload).unwrap();
    assert!(data.len() == 4000 && end == recs[3].end(), "first frame {} bytes ending at {end}, want {}", data.len(), recs[3].end());
    let (end, data) = wire::split_end(&fs[2].payload).unwrap();
    assert!(data.len() == 2000 && end == recs[9].end(), "last output frame {} bytes ending at {end}", data.len());
    assert_eq!(q.lag_bytes(), 0, "lag");
}

#[test]
fn queue_window_and_order() {
    let mut q = FakeClock::new().queue();
    q.reset_flow(1);
    let data: Rc<[u8]> = Rc::from(vec![0u8; 100 << 10]);
    let hist: Rc<[u8]> = Rc::from(vec![0u8; 200 << 10]);
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_BEGIN, 1, Vec::new()), 0);
    q.marker_bytes(wire::TYPE_SNAPSHOT_DATA, 1, &data, 0, data.len());
    q.chunk(wire::TYPE_SNAPSHOT_HISTORY, 1, &hist, 0, hist.len());
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_READY, 1, Vec::new()), 0);
    q.end(wire::Frame::new(wire::TYPE_STREAM_END, 1, Vec::new()));
    let fs = drain(&mut q);
    assert!(
        fs[0].ty == wire::TYPE_SNAPSHOT_BEGIN
            && fs[1].ty == wire::TYPE_SNAPSHOT_DATA
            && fs[1].payload.len() == 100 << 10
            && counted(&fs) == INITIAL_WINDOW as usize,
        "sent {} counted bytes in {} frames",
        counted(&fs),
        fs.len()
    );
    // Control frames don't wait; STREAM_END keeps its place.
    q.control(wire::Frame::new(wire::TYPE_RESPONSE, 0, Vec::new()));
    let fs = drain(&mut q);
    assert!(fs.len() == 1 && fs[0].ty == wire::TYPE_RESPONSE, "control: {fs:?}");
    // Credit for another stream doesn't count.
    q.credit(2, 1 << 20);
    assert!(drain(&mut q).is_empty(), "stale credit opened the window");
    // A nearly closed window doesn't split chunks into slivers.
    q.credit(1, 100);
    assert!(drain(&mut q).is_empty(), "sent a sliver");
    let mut total = INITIAL_WINDOW as usize;
    for _ in 0..10 {
        q.credit(1, 64 << 10);
        let fs = drain(&mut q);
        total += counted(&fs);
        if fs.last().is_some_and(|f| f.ty == wire::TYPE_STREAM_END) {
            assert!(
                fs[fs.len() - 2].ty == wire::TYPE_SNAPSHOT_READY && total == 200 << 10,
                "end after {total} bytes, frames {fs:?}"
            );
            return;
        }
    }
    panic!("never finished");
}

#[test]
fn queue_drop_keeps_end() {
    let mut q = FakeClock::new().queue();
    q.reset_flow(1);
    for r in records(100) {
        q.record(1, r);
    }
    q.end(wire::Frame::new(wire::TYPE_STREAM_END, 1, Vec::new()));
    let x: Rc<[u8]> = Rc::from(&b"x"[..]);
    q.chunk(wire::TYPE_SNAPSHOT_DATA, 1, &x, 0, 1);
    assert_eq!(q.lag_bytes(), 100 * 1008, "lag");
    q.drop_data();
    assert_eq!(q.lag_bytes(), 0, "lag");
    q.close();
    q.control(wire::Frame::new(wire::TYPE_RESPONSE, 0, Vec::new()));
    q.record(1, Record { start: 0, kind: REC_OUTPUT, data: Rc::from(&b"late"[..]), owner_only: false, only: None });
    let fs = drain(&mut q);
    assert!(fs.len() == 1 && fs[0].ty == wire::TYPE_STREAM_END, "after drop: {fs:?}");
    // An event bigger than the window still goes once nothing is in flight.
    let mut q2 = FakeClock::new().queue();
    q2.reset_flow(1);
    let big = vec![0u8; INITIAL_WINDOW as usize * 2];
    q2.record(1, Record { start: 0, kind: REC_EVENT, data: Rc::from(big), owner_only: false, only: None });
    assert_eq!(drain(&mut q2).len(), 1, "oversized event wedged");
}

/// Simulates a client behind a link with the given round trip that credits
/// at most per_rtt bytes per round trip, and returns the window and frame
/// size.
fn link(rtt: Duration, per_rtt: usize, rounds: usize) -> (i64, i64) {
    // A fake clock: each round trip takes exactly rtt.
    let clock = FakeClock::new();
    let mut q = clock.queue();
    q.reset_flow(1);
    let mut l = OutputLog::new(1 << 40);
    // Enough output that the sender never runs dry: an idle link would
    // rightly shrink frames.
    let data = vec![0u8; 4000];
    for _ in 0..8000 {
        q.record(1, l.append(REC_OUTPUT, &data, false, None));
    }
    let mut owed = 0;
    for _ in 0..rounds {
        owed += counted(&drain(&mut q));
        clock.add(rtt);
        let n = owed.min(per_rtt);
        owed -= n;
        q.credit(1, n as u32);
    }
    (q.window, q.frame_data)
}

#[test]
fn window_adapts() {
    // A fast link with a real round trip: the window grows past the initial
    // 64 KiB and frames grow with it.
    let (w, f) = link(Duration::from_millis(10), 1 << 30, 12);
    assert!(w > 4 * INITIAL_WINDOW && f == MAX_FRAME_DATA, "fast link: window {w}, frames {f}");
    // A slow client crediting 16 KiB per round trip: it shrinks, and frames
    // stay at their smallest.
    let (w, f) = link(Duration::from_millis(10), 16 << 10, 12);
    assert!((MIN_WINDOW..INITIAL_WINDOW).contains(&w) && f == MIN_FRAME_DATA, "slow link: window {w}, frames {f}");
}

/// Output queued behind history goes first, before END, and history follows
/// with what credit is left; a later snapshot's frames keep their order.
#[test]
fn queue_records_before_history() {
    let mut q = FakeClock::new().queue();
    q.reset_flow(1);
    let hist: Rc<[u8]> = Rc::from(vec![0u8; 10]);
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_READY, 1, Vec::new()), 0);
    q.chunk(wire::TYPE_SNAPSHOT_HISTORY, 1, &hist, 0, hist.len());
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_END, 1, Vec::new()), 0);
    let recs = records(2);
    q.record(1, recs[0].clone());
    q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_BEGIN, 1, Vec::new()), 1);
    q.record(1, recs[1].clone());
    let types: Vec<u8> = drain(&mut q).iter().map(|f| f.ty).collect();
    let want = [
        wire::TYPE_SNAPSHOT_READY,
        wire::TYPE_OUTPUT,
        wire::TYPE_SNAPSHOT_HISTORY,
        wire::TYPE_SNAPSHOT_END,
        wire::TYPE_SNAPSHOT_BEGIN,
        wire::TYPE_OUTPUT,
    ];
    assert_eq!(types, want, "order");
}

#[test]
fn w2_loop() {
    let mut w = W2Loop::new(80, 24);
    let mut total = 0;
    for _ in 0..2000 {
        let b = w.next(12500);
        assert!(b.len() >= 12500 && b[0] == 0x1b, "chunk of {} starting {:?}", b.len(), &b[..b.len().min(4)]);
        total += b.len();
    }
    assert!(total >= 2 * w.buf.len() && w.next(0).is_empty(), "didn't wrap: {total} of {}", w.buf.len());
}
