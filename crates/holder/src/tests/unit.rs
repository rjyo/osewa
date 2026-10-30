use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use crate::log::{OutputLog, REC_EVENT, REC_OUTPUT};
use crate::queue::SendQueue;

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
    assert_eq!(l.start(), 10);
    for (p, want) in [(0, false), (10, true), (15, false), (40, true), (41, false)] {
        assert_eq!(l.since(p).is_some(), want, "since {p}");
    }
    // One record larger than the cap is still kept.
    l.append(REC_OUTPUT, &[0; 100], false, None);
    assert_eq!((l.len(), l.start()), (1, 40));
    assert_eq!(OutputLog::new(0).start(), 0);
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
    assert_eq!(q.lag_bytes(), 0);
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
