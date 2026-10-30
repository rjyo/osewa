//! The per-connection send queue (docs/PROTOCOL.md, Priority).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use osewa_wire as wire;
use tokio::sync::Notify;

/// A frame's write priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// RESPONSE, INPUT_ACK, GRANT, STREAM_END
    Control = 0,
    Session = 1,
    Exec = 2,
    Forward = 3,
}

const N_CLASSES: usize = 4;

/// How much higher-class data a waiting class lets go ahead before it gets
/// the next frame: a terminal that prints without pause must not starve
/// forwards an app's requests ride on.
pub(crate) const STARVE_BYTES: usize = 64 << 10;

/// One stream's frames, in order.
#[derive(Default)]
struct Fifo {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    cls: usize,
}

/// Reports a frame type that goes ahead of data.
fn is_control(b: &[u8]) -> bool {
    matches!(b[0], wire::TYPE_RESPONSE | wire::TYPE_INPUT_ACK | wire::TYPE_GRANT | wire::TYPE_STREAM_END | wire::TYPE_WELCOME)
}

struct Inner {
    ctrl: Fifo,
    streams: HashMap<u32, Fifo>,
    /// Per class, the streams with frames queued, in round-robin order.
    ring: [VecDeque<u32>; N_CLASSES],
    bytes: usize,
    class_bytes: [usize; N_CLASSES],
    /// Per class, the higher-class bytes written while it had frames
    /// waiting since it last got one.
    passed: [usize; N_CLASSES],
    soft: usize,
    hard: usize,
    chan_soft: usize,
    closed: bool,
}

/// Holds encoded frames for one client connection and hands them to the
/// writer by priority: connection-level control frames first, then a stream
/// whose next frame is a control frame, then session, exec and forward
/// streams, round-robin within each class, except that a class kept waiting
/// behind STARVE_BYTES of higher-class data goes next. Frames of one stream
/// keep their order, so STREAM_END still follows the stream's last data and
/// a response queued behind it stays behind it.
///
/// Pushers of data wait instead of growing the queue: session relays while
/// the session class holds soft bytes, which pushes lag back to the holder
/// (where it becomes a fresh snapshot); channel pumps while their own stream
/// holds chan_soft, so a bulk forward never has more than that queued ahead
/// of a keystroke's echo. Anything that would take the queue past hard means
/// the client is gone or hostile, and closes it.
pub(crate) struct SendQueue {
    inner: RefCell<Inner>,
    /// Wakes the writer.
    ready: Notify,
    /// Wakes pushers waiting for room.
    room: Notify,
}

impl SendQueue {
    pub fn new(soft: usize, hard: usize, chan_soft: usize) -> SendQueue {
        SendQueue {
            inner: RefCell::new(Inner {
                ctrl: Fifo::default(),
                streams: HashMap::new(),
                ring: Default::default(),
                bytes: 0,
                class_bytes: [0; N_CLASSES],
                passed: [0; N_CLASSES],
                soft,
                hard,
                chan_soft,
                closed: false,
            }),
            ready: Notify::new(),
            room: Notify::new(),
        }
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.inner.borrow().bytes
    }

    /// Bytes queued on exec and forward streams.
    pub fn bulk_bytes(&self) -> usize {
        let q = self.inner.borrow();
        q.class_bytes[Class::Exec as usize] + q.class_bytes[Class::Forward as usize]
    }

    /// Queues b behind stream's frames, or with the connection's control
    /// frames when stream is 0, without waiting. It returns false once the
    /// queue is closed, including when b overflowed it.
    pub fn push(&self, stream: u32, cls: Class, b: Vec<u8>) -> bool {
        let mut q = self.inner.borrow_mut();
        if q.closed {
            return false;
        }
        if q.bytes + b.len() > q.hard {
            q.close();
            drop(q);
            self.wake_all();
            return false;
        }
        let n = b.len();
        let fcls = if stream == 0 {
            q.ctrl.frames.push_back(b);
            q.ctrl.bytes += n;
            q.ctrl.cls
        } else {
            let q = &mut *q;
            let f = q.streams.entry(stream).or_insert_with(|| {
                // A stream id reused while its old frames are queued keeps
                // the old class until they are written.
                q.ring[cls as usize].push_back(stream);
                Fifo { cls: cls as usize, ..Default::default() }
            });
            f.frames.push_back(b);
            f.bytes += n;
            f.cls
        };
        q.bytes += n;
        q.class_bytes[fcls] += n;
        drop(q);
        self.ready.notify_one();
        true
    }

    /// push, first waiting while cls is over its bound.
    pub async fn push_wait(&self, stream: u32, cls: Class, b: Vec<u8>) -> bool {
        loop {
            {
                let q = self.inner.borrow();
                if q.closed || !q.full(stream, cls) {
                    break;
                }
            }
            self.room.notified().await;
        }
        self.push(stream, cls, b)
    }

    /// Takes the next frame without waiting.
    pub fn try_pop(&self) -> Option<Vec<u8>> {
        let b = self.inner.borrow_mut().pop()?;
        self.room.notify_waiters();
        Some(b)
    }

    /// Waits for the next frame; None once the queue is closed.
    pub async fn pop(&self) -> Option<Vec<u8>> {
        loop {
            if self.inner.borrow().closed {
                return None;
            }
            if let Some(b) = self.try_pop() {
                return Some(b);
            }
            self.ready.notified().await;
        }
    }

    pub fn close(&self) {
        self.inner.borrow_mut().close();
        self.wake_all();
    }

    fn wake_all(&self) {
        self.room.notify_waiters();
        self.ready.notify_one();
    }
}

impl Inner {
    fn full(&self, stream: u32, cls: Class) -> bool {
        match cls {
            Class::Session => self.class_bytes[Class::Session as usize] >= self.soft,
            Class::Exec | Class::Forward => self.streams.get(&stream).is_some_and(|f| f.bytes >= self.chan_soft),
            Class::Control => false,
        }
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        if self.closed || self.bytes == 0 {
            return None;
        }
        if !self.ctrl.frames.is_empty() {
            let b = self.ctrl.frames.pop_front().unwrap();
            self.ctrl.bytes -= b.len();
            self.taken(self.ctrl.cls, b.len());
            return Some(b);
        }
        for cls in 1..N_CLASSES {
            if let Some(i) = self.ring[cls].iter().position(|id| is_control(&self.streams[id].frames[0])) {
                return Some(self.next(cls, i));
            }
        }
        for cls in 2..N_CLASSES {
            if self.passed[cls] >= STARVE_BYTES && !self.ring[cls].is_empty() {
                return Some(self.next(cls, 0));
            }
        }
        for cls in 1..N_CLASSES {
            if !self.ring[cls].is_empty() {
                return Some(self.next(cls, 0));
            }
        }
        panic!("server: send queue bytes without frames");
    }

    /// Takes the first frame of the i'th stream in cls's ring and moves that
    /// stream to the back.
    fn next(&mut self, cls: usize, i: usize) -> Vec<u8> {
        let id = self.ring[cls].remove(i).unwrap();
        let f = self.streams.get_mut(&id).unwrap();
        let b = f.frames.pop_front().unwrap();
        f.bytes -= b.len();
        let fcls = f.cls;
        if f.frames.is_empty() {
            self.streams.remove(&id);
        } else {
            self.ring[cls].push_back(id);
        }
        self.taken(fcls, b.len());
        self.passed[cls] = 0;
        for lower in cls + 1..N_CLASSES {
            if !self.ring[lower].is_empty() {
                self.passed[lower] += b.len();
            }
        }
        b
    }

    fn taken(&mut self, cls: usize, n: usize) {
        self.bytes -= n;
        self.class_bytes[cls] -= n;
    }

    fn close(&mut self) {
        self.closed = true;
        self.ctrl = Fifo::default();
        self.streams.clear();
        self.ring = Default::default();
        self.passed = [0; N_CLASSES];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::time::Duration;

    /// A marshaled frame whose payload names it, for checking order.
    fn frame(ty: u8, stream: u32, name: &str) -> Vec<u8> {
        wire::Frame::new(ty, stream, name.as_bytes().to_vec()).marshal()
    }

    fn pop_names(q: &SendQueue, n: usize) -> String {
        let mut names = Vec::new();
        for _ in 0..n {
            let b = q.try_pop().expect("queue empty or closed");
            names.push(String::from_utf8(b[wire::HEADER_SIZE..].to_vec()).unwrap());
        }
        names.join(" ")
    }

    fn local<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }

    async fn pending_for(h: &tokio::task::JoinHandle<bool>, d: Duration) -> bool {
        tokio::time::sleep(d).await;
        !h.is_finished()
    }

    #[test]
    fn send_queue() {
        local(async {
            let q = Rc::new(SendQueue::new(8, 20, 1 << 20));
            assert!(q.push(1, Class::Session, frame(wire::TYPE_OUTPUT, 1, "abc")));
            let q2 = q.clone();
            let blocked = tokio::task::spawn_local(async move {
                q2.push_wait(1, Class::Session, frame(wire::TYPE_OUTPUT, 1, "d")).await
            });
            assert!(pending_for(&blocked, Duration::from_millis(50)).await, "push above soft did not wait");
            // Control frames and channel streams don't wait on the session
            // bound.
            assert!(q.push_wait(0, Class::Control, frame(wire::TYPE_RESPONSE, 0, "r")).await);
            assert_eq!(pop_names(&q, 2), "r abc");
            assert!(blocked.await.unwrap(), "waiting push failed");
            assert!(!q.push(2, Class::Session, vec![0; 20]), "overflow accepted");
            assert!(q.pop().await.is_none(), "pop after overflow");
            assert!(!q.push_wait(1, Class::Session, frame(wire::TYPE_OUTPUT, 1, "x")).await, "push after close");
        });
    }

    // Control first, then a stream whose next frame is control, then
    // session, exec and forward streams; round-robin within a class; each
    // stream in order.
    #[test]
    fn priority() {
        let q = SendQueue::new(1 << 20, 1 << 20, 1 << 20);
        let push = |stream, cls, ty, name| assert!(q.push(stream, cls, frame(ty, stream, name)));
        push(5, Class::Forward, wire::TYPE_CHANNEL_DATA, "f1");
        push(5, Class::Forward, wire::TYPE_CHANNEL_DATA, "f2");
        push(5, Class::Forward, wire::TYPE_STREAM_END, "f-end");
        push(5, Class::Forward, wire::TYPE_RESPONSE, "f-closed");
        push(8, Class::Forward, wire::TYPE_CHANNEL_DATA, "g1");
        push(8, Class::Forward, wire::TYPE_CHANNEL_DATA, "g2");
        push(6, Class::Exec, wire::TYPE_CHANNEL_DATA, "e1");
        push(7, Class::Exec, wire::TYPE_STREAM_END, "e-end");
        push(1, Class::Session, wire::TYPE_OUTPUT, "s1");
        push(2, Class::Session, wire::TYPE_OUTPUT, "t1");
        push(1, Class::Session, wire::TYPE_OUTPUT, "s2");
        push(0, Class::Control, wire::TYPE_GRANT, "grant");
        assert_eq!(pop_names(&q, 12), "grant e-end s1 t1 s2 e1 f1 g1 f2 f-end f-closed g2");
    }

    // While a forward pushes as fast as its credit allows, a session's OUTPUT
    // queued after the forward's frames is written before the forward's next
    // frame.
    #[test]
    fn forward_yields() {
        local(async {
            let q = Rc::new(SendQueue::new(1 << 20, 4 << 20, 2 * frame(wire::TYPE_CHANNEL_DATA, 9, "fwd-00000").len()));
            let q2 = q.clone();
            let pusher = tokio::task::spawn_local(async move {
                for i in 0.. {
                    if !q2.push_wait(9, Class::Forward, frame(wire::TYPE_CHANNEL_DATA, 9, &format!("fwd-{i:05}"))).await {
                        return;
                    }
                }
            });
            for i in 0..100 {
                // The writer is busy with forward frames when the echo
                // arrives.
                tokio::task::yield_now().await;
                let got = pop_names(&q, 1);
                assert!(got.starts_with("fwd-"), "popped {got:?}");
                let echo = format!("echo-{i}");
                q.push(1, Class::Session, frame(wire::TYPE_OUTPUT, 1, &echo));
                assert_eq!(pop_names(&q, 1), echo, "after OUTPUT was queued");
            }
            q.close();
            pusher.await.unwrap();
        });
    }

    // A channel stream waits on its own queued bytes, not on other streams.
    #[test]
    fn channel_bound() {
        local(async {
            let q = Rc::new(SendQueue::new(1 << 20, 1 << 20, 10));
            assert!(q.push_wait(3, Class::Forward, vec![0; 10]).await);
            let q2 = q.clone();
            let blocked = tokio::task::spawn_local(async move { q2.push_wait(3, Class::Forward, vec![0; 10]).await });
            assert!(pending_for(&blocked, Duration::from_millis(50)).await, "push over the stream's bound did not wait");
            assert!(q.push_wait(4, Class::Forward, vec![0; 10]).await && q.push_wait(1, Class::Session, vec![0; 10]).await);
            q.try_pop(); // session
            q.try_pop(); // stream 3, freeing room
            assert!(blocked.await.unwrap(), "waiting push failed");
        });
    }

    // A session printing without pause doesn't starve exec and forward: each
    // gets a frame once STARVE_BYTES of higher-class data has passed it.
    #[test]
    fn no_starvation() {
        let q = SendQueue::new(1 << 30, 1 << 30, 1 << 30);
        let chunk = vec![0u8; 16 << 10];
        let mk = |s| wire::Frame::new(wire::TYPE_OUTPUT, s, chunk.clone()).marshal();
        for _ in 0..200 {
            q.push(1, Class::Session, mk(1));
        }
        // Enough that neither runs out within the frames popped below.
        for _ in 0..60 {
            q.push(5, Class::Forward, mk(5));
            q.push(6, Class::Exec, mk(6));
        }
        let frame_len = wire::HEADER_SIZE + chunk.len();
        // ahead[cls] is the higher-class bytes written since cls last got a
        // frame.
        let mut ahead = [0usize; N_CLASSES];
        let mut got = [0usize; N_CLASSES];
        for _ in 0..120 {
            let b = q.try_pop().unwrap();
            let f = wire::Frame::unmarshal(&b).unwrap();
            let cls = match f.stream {
                1 => Class::Session,
                6 => Class::Exec,
                _ => Class::Forward,
            } as usize;
            got[cls] += 1;
            ahead[cls] = 0;
            for (lower, a) in ahead.iter_mut().enumerate().skip(cls + 1) {
                *a += b.len();
                // The frame that crosses STARVE_BYTES, and when exec and
                // forward come due together, the one exec gets first.
                assert!(*a <= STARVE_BYTES + 2 * frame_len, "class {lower} waited behind {a} bytes");
            }
        }
        assert!(
            got[Class::Exec as usize] > 0 && got[Class::Forward as usize] > 0 && got[Class::Session as usize] >= 80,
            "frames by class {got:?}"
        );
    }

    // With nothing of a lower class waiting, nothing changes: the session's
    // frames go out in order, and a forward that starts waiting doesn't jump
    // ahead of an echo until it has waited behind STARVE_BYTES.
    #[test]
    fn starvation_only_when_waiting() {
        let q = SendQueue::new(1 << 30, 1 << 30, 1 << 30);
        let chunk = vec![0u8; 16 << 10];
        for _ in 0..10 {
            q.push(1, Class::Session, wire::Frame::new(wire::TYPE_OUTPUT, 1, chunk.clone()).marshal());
        }
        // The forward wasn't waiting while those were written.
        for _ in 0..10 {
            q.try_pop();
        }
        q.push(5, Class::Forward, frame(wire::TYPE_CHANNEL_DATA, 5, "f1"));
        q.push(1, Class::Session, frame(wire::TYPE_OUTPUT, 1, "echo"));
        assert_eq!(pop_names(&q, 2), "echo f1");
    }
}
