use std::collections::VecDeque;
use std::rc::Rc;

use serde_json::json;

use crate::trace::Tracer;

/// Caps input bytes waiting for the child to read stdin; past it the holder
/// parks input unacked.
pub(crate) const MAX_PENDING_INPUT: usize = 64 << 10;
/// Caps query replies waiting for the child.
pub(crate) const MAX_REPLY_BACKLOG: usize = 1 << 20;
/// Bounds one PTY write.
pub(crate) const WRITE_CHUNK: usize = 4096;

struct TracedInput {
    end: u64,
    client: String,
    seq: u64,
    bytes: usize,
}

/// Bytes waiting for the PTY. The connection writer task drains it, so a
/// child that doesn't read stdin never stalls the holder loop. Terminal
/// query replies land here from inside libghostty's write callback, which is
/// why it lives apart from the session.
#[derive(Default)]
pub(crate) struct PtyInput {
    buf: VecDeque<u8>,
    pub closed: bool,
    /// With tracing, applied input is reported once its last byte has been
    /// written: pushed and written count bytes since the start.
    pub tr: Option<Rc<Tracer>>,
    pushed: u64,
    written: u64,
    inputs: VecDeque<TracedInput>,
}

impl PtyInput {
    pub fn new(tr: Option<Rc<Tracer>>) -> PtyInput {
        PtyInput { tr, ..Default::default() }
    }

    /// Queues b. Replies past MAX_REPLY_BACKLOG are dropped: a program that
    /// floods queries and never reads stdin must not grow the holder
    /// unbounded.
    pub fn push(&mut self, b: &[u8], reply: bool) {
        if b.is_empty() || self.closed || (reply && self.buf.len() >= MAX_REPLY_BACKLOG) {
            return;
        }
        self.buf.extend(b);
        self.pushed += b.len() as u64;
    }

    /// push for the owner's input, traced as input_applied once it reaches
    /// the PTY.
    pub fn push_input(&mut self, b: &[u8], client: &str, seq: u64) {
        let Some(tr) = &self.tr else {
            self.push(b, false);
            return;
        };
        if b.is_empty() {
            tr.emit("input_applied", &[("client_id", json!(client)), ("seq", json!(seq)), ("bytes", json!(0))]);
            return;
        }
        if self.closed {
            return;
        }
        self.buf.extend(b);
        self.pushed += b.len() as u64;
        self.inputs.push_back(TracedInput { end: self.pushed, client: client.into(), seq, bytes: b.len() });
    }

    /// Whether input must wait for the child to read.
    pub fn full(&self) -> bool {
        !self.closed && self.buf.len() >= MAX_PENDING_INPUT
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.buf = VecDeque::new();
    }

    /// Copies the next chunk to write into out.
    pub fn chunk(&self, out: &mut Vec<u8>) {
        out.clear();
        let n = self.buf.len().min(WRITE_CHUNK);
        let (a, b) = self.buf.as_slices();
        let na = n.min(a.len());
        out.extend_from_slice(&a[..na]);
        out.extend_from_slice(&b[..n - na]);
    }

    /// Records n bytes written; true when there is room for parked input.
    pub fn wrote(&mut self, n: usize) -> bool {
        if self.closed {
            return false;
        }
        self.buf.drain(..n.min(self.buf.len()));
        self.written += n as u64;
        while self.inputs.front().is_some_and(|i| i.end <= self.written) {
            let i = self.inputs.pop_front().unwrap();
            if let Some(tr) = &self.tr {
                tr.emit("input_applied", &[("client_id", json!(i.client)), ("seq", json!(i.seq)), ("bytes", json!(i.bytes))]);
            }
        }
        self.buf.len() < MAX_PENDING_INPUT
    }
}
