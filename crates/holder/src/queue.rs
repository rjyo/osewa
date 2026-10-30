use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use crate::log::{Record, REC_EVENT, REC_OUTPUT};

// Flow control limits (docs/PROTOCOL.md, Flow control).
pub(crate) const INITIAL_WINDOW: i64 = 64 << 10;
pub(crate) const MIN_WINDOW: i64 = 16 << 10;
pub(crate) const MAX_WINDOW: i64 = 4 << 20;
pub(crate) const MIN_FRAME_DATA: i64 = 4 << 10;
pub(crate) const MAX_FRAME_DATA: i64 = 64 << 10;
/// Keeps a nearly closed window from cutting snapshot chunks into slivers.
const MIN_SPLIT: i64 = 1 << 10;
/// How long a minimum round trip stands.
const RTT_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// SNAPSHOT_BEGIN, READY or END, or the first snapshot's SNAPSHOT_DATA:
    /// uncounted, but in order, and dropped on lag.
    Marker,
    /// STREAM_END: uncounted, in order, and never dropped.
    End,
    /// SNAPSHOT_HISTORY, or SNAPSHOT_DATA after a stream's first snapshot:
    /// counted and splittable, but not lag, so a snapshot larger than the
    /// lag threshold can't trigger another.
    Chunk,
    /// A log record: counted, lag, and consecutive output records merge
    /// into one OUTPUT frame.
    Record,
}

enum Body {
    Frame(Vec<u8>),
    /// Snapshot bytes buf[off..end]; chunks share the snapshot's buffer.
    Chunk { buf: Rc<[u8]>, off: usize, end: usize },
    Rec(Record),
}

pub(crate) struct Entry {
    pub kind: Kind,
    pub ty: u8,
    pub stream: u32,
    body: Body,
    /// SNAPSHOT_BEGIN, READY, END: its snapshot's generation.
    pub gen: u32,
    /// Records: when queued, for lag by age.
    at: Option<Instant>,
}

impl Entry {
    pub fn is_marker(&self, t: u8) -> bool {
        self.kind == Kind::Marker && self.ty == t
    }

    fn is_history(&self) -> bool {
        self.kind == Kind::Chunk && self.ty == wire::TYPE_SNAPSHOT_HISTORY
    }

    pub fn rec(&self) -> &Record {
        match &self.body {
            Body::Rec(r) => r,
            _ => unreachable!("not a record"),
        }
    }

    /// The frame payload this entry carries, but for a record.
    #[cfg(test)]
    pub fn payload(&self) -> &[u8] {
        match &self.body {
            Body::Frame(p) => p,
            Body::Chunk { buf, off, end } => &buf[*off..*end],
            Body::Rec(_) => &[],
        }
    }
}

/// What next_into wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sent {
    pub ty: u8,
    pub stream: u32,
    /// The generation of a snapshot marker.
    pub gen: u32,
}

pub(crate) type Clock = Rc<dyn Fn() -> Instant>;

/// Decouples the holder loop from a connection's socket: the loop pushes
/// without blocking, and the connection's writer takes frames as they may
/// go. RESPONSE and INPUT_ACK go out first and never wait. Data frames wait
/// for credit; the counted bytes of records still waiting are the client's
/// lag.
pub(crate) struct SendQueue {
    pub prio: VecDeque<wire::Frame>,
    pub data: VecDeque<Entry>,
    lag: i64,
    /// SNAPSHOT_READY markers not yet sent.
    pub ready: i64,
    pub closed: bool,
    now: Clock,
    /// Generation of the last snapshot marker taken.
    pub mark_gen: u32,
    /// Sent offset at the last SNAPSHOT_END written.
    pub end_at: u64,
    pub end_armed: bool,
    /// Generation of that SNAPSHOT_END.
    pub end_gen: u32,
    /// Generation of the last SNAPSHOT_END taken to send.
    pub last_end: u32,
    /// SNAPSHOT_DATA bytes sent since the last SNAPSHOT_BEGIN.
    pub snap_data: i64,
    /// When the last SNAPSHOT_END went out.
    pub end_sent_at: Option<Instant>,

    /// The stream whose credit this is.
    id: u32,
    /// Keeps the window at least this large: two screens while a stream
    /// catches up, so the next screen (a keystroke's) goes out while the last
    /// is still uncredited instead of a round trip later.
    floor: i64,
    /// Counted bytes allowed in flight.
    pub window: i64,
    /// OUTPUT payload target.
    pub frame_data: i64,
    sent: u64,
    credited: u64,
    marks: VecDeque<(u64, Instant)>,
    /// The shortest credit round trip in the last RTT_WINDOW. Sizing from it
    /// rather than a smoothed RTT keeps a slow client's own queueing, which
    /// stretches every later round trip, from inflating the window that
    /// causes it.
    min_rtt: Duration,
    min_rtt_at: Option<Instant>,
    /// The smoothed credit round trip, for judging how long a record may
    /// wait before the stream counts as behind: on a long link the first
    /// window alone holds output back for a whole round trip.
    pub srtt: Duration,
    /// Waited on credit since the last window update.
    blocked: bool,
    rate_from: Option<Instant>,
    /// Credited at rate_from.
    rate_base: u64,
}

fn later(a: Instant, b: Option<Instant>) -> Instant {
    match b {
        Some(b) if b > a => b,
        _ => a,
    }
}

fn header(out: &mut Vec<u8>, ty: u8, stream: u32, payload: usize) {
    out.extend_from_slice(&((wire::HEADER_SIZE + payload) as u32).to_le_bytes());
    out.push(ty);
    out.extend_from_slice(&stream.to_le_bytes());
}

impl SendQueue {
    pub fn new(now: Clock) -> SendQueue {
        let mut q = SendQueue {
            prio: VecDeque::new(),
            data: VecDeque::new(),
            lag: 0,
            ready: 0,
            closed: false,
            now,
            mark_gen: 0,
            end_at: 0,
            end_armed: false,
            end_gen: 0,
            last_end: 0,
            snap_data: 0,
            end_sent_at: None,
            id: 0,
            floor: 0,
            window: 0,
            frame_data: 0,
            sent: 0,
            credited: 0,
            marks: VecDeque::new(),
            min_rtt: Duration::ZERO,
            min_rtt_at: None,
            srtt: Duration::ZERO,
            blocked: false,
            rate_from: None,
            rate_base: 0,
        };
        q.reset_flow(0);
        q
    }

    /// Queues a frame that is never held back.
    pub fn control(&mut self, f: wire::Frame) {
        if !self.closed {
            self.prio.push_back(f);
        }
    }

    fn add(&mut self, mut e: Entry) {
        if self.closed {
            return;
        }
        if e.kind == Kind::Record {
            e.at = Some((self.now)());
            self.lag += e.rec().cost() as i64;
        }
        if e.is_marker(wire::TYPE_SNAPSHOT_READY) {
            self.ready += 1;
        }
        self.data.push_back(e);
    }

    fn frame_entry(kind: Kind, f: wire::Frame, gen: u32) -> Entry {
        Entry { kind, ty: f.ty, stream: f.stream, body: Body::Frame(f.payload), gen, at: None }
    }

    /// Queues SNAPSHOT_BEGIN, READY or END of generation gen. END's gen comes
    /// back from end_written, so a late completion of a snapshot that was
    /// since replaced can be told apart.
    pub fn snapshot_marker(&mut self, f: wire::Frame, gen: u32) {
        self.add(Self::frame_entry(Kind::Marker, f, gen));
    }

    /// Queues the first snapshot's SNAPSHOT_DATA: uncounted.
    pub fn marker_bytes(&mut self, ty: u8, stream: u32, buf: &Rc<[u8]>, off: usize, end: usize) {
        self.add(Entry { kind: Kind::Marker, ty, stream, body: Body::Frame(buf[off..end].to_vec()), gen: 0, at: None });
    }

    pub fn end(&mut self, f: wire::Frame) {
        self.add(Self::frame_entry(Kind::End, f, 0));
    }

    pub fn chunk(&mut self, ty: u8, stream: u32, buf: &Rc<[u8]>, off: usize, end: usize) {
        self.add(Entry { kind: Kind::Chunk, ty, stream, body: Body::Chunk { buf: buf.clone(), off, end }, gen: 0, at: None });
    }

    pub fn record(&mut self, stream: u32, r: Record) {
        let ty = r.kind;
        self.add(Entry { kind: Kind::Record, ty, stream, body: Body::Rec(r), gen: 0, at: None });
    }

    #[cfg(test)]
    pub fn lag_bytes(&self) -> i64 {
        self.lag
    }

    /// Whether the client is too far behind to stream to: more than
    /// max_bytes of records waiting, or the oldest waiting longer than
    /// max(delay, 2 round trips). Age only counts once a credit round trip
    /// has been measured, so a burst in the first window on a long link
    /// isn't mistaken for a slow client. While a snapshot hasn't reached
    /// READY it gets the benefit of the doubt, up to twice max_bytes, so on
    /// a slow link each screen lands before the next replaces it.
    pub fn lagging(&self, max_bytes: i64, delay: Duration) -> bool {
        if self.ready > 0 {
            return self.lag > 2 * max_bytes;
        }
        if self.lag > max_bytes {
            return true;
        }
        if delay.is_zero() || self.srtt.is_zero() {
            return false;
        }
        self.oldest() > delay.max(2 * self.srtt)
    }

    /// Whether no unsent record has waited longer than one round trip (or
    /// delay, before one is measured): the test for leaving catch-up under
    /// steady output the link can carry.
    pub fn keeping_up(&self, delay: Duration) -> bool {
        let limit = if self.srtt.is_zero() { delay } else { self.srtt };
        self.oldest() <= limit
    }

    /// How long the oldest unsent record has waited for the link. Waiting
    /// behind a snapshot doesn't count: that's the snapshot's time, and a
    /// stream must not fall behind just because a catch-up screen was ahead
    /// of its output.
    fn oldest(&self) -> Duration {
        for e in &self.data {
            if e.kind == Kind::Record {
                let since = later(e.at.unwrap(), self.end_sent_at);
                return (self.now)().saturating_duration_since(since);
            }
            if e.kind != Kind::End && !e.is_history() {
                // BEGIN, DATA or READY ahead: the record can't go yet.
                return Duration::ZERO;
            }
        }
        Duration::ZERO
    }

    /// Removes queued records only, keeping snapshot frames.
    pub fn drop_records(&mut self) {
        self.data.retain(|e| e.kind != Kind::Record);
        self.lag = 0;
    }

    /// Removes every queued data frame but STREAM_END.
    pub fn drop_data(&mut self) {
        self.data.retain(|e| e.kind == Kind::End);
        self.lag = 0;
        self.ready = 0;
    }

    /// Starts a new stream's credit: the window and frame size go back to
    /// their initial values and nothing is in flight.
    pub fn reset_flow(&mut self, id: u32) {
        self.id = id;
        self.window = INITIAL_WINDOW;
        self.frame_data = MIN_FRAME_DATA;
        self.sent = 0;
        self.credited = 0;
        self.marks.clear();
        self.min_rtt = Duration::ZERO;
        self.min_rtt_at = None;
        self.srtt = Duration::ZERO;
        self.blocked = false;
        self.rate_from = None;
        self.rate_base = 0;
        // Catch-up state belongs to the old stream too.
        self.snap_data = 0;
        self.end_sent_at = None;
        self.floor = 0;
        self.end_armed = false;
        self.end_at = 0;
        self.end_gen = 0;
        self.last_end = 0;
    }

    /// Applies a CREDIT frame for stream id and resizes the window once per
    /// round trip: about twice the bytes credited per round trip, but
    /// shrinking only when the stream actually waited on credit, so an idle
    /// stream keeps its window. Returns the generation of the snapshot this
    /// credit completed, if any.
    pub fn credit(&mut self, id: u32, n: u32) -> Option<u32> {
        let now = (self.now)();
        if id != self.id {
            // Credit for a stream this connection had before: its bytes were
            // never counted against the current window.
            return None;
        }
        self.credited = (self.credited + n as u64).min(self.sent);
        let mut done = None;
        if self.end_armed && self.credited >= self.end_at {
            self.end_armed = false;
            done = Some(self.end_gen);
        }
        let mut sample = None;
        while let Some(&(off, at)) = self.marks.front() {
            if off > self.credited {
                break;
            }
            sample = Some(now.saturating_duration_since(at));
            self.marks.pop_front();
        }
        if let Some(s) = sample {
            let stale = self.min_rtt_at.is_none_or(|at| now.saturating_duration_since(at) > RTT_WINDOW);
            if self.min_rtt.is_zero() || s <= self.min_rtt || stale {
                self.min_rtt = s.max(Duration::from_micros(1));
                self.min_rtt_at = Some(now);
            }
            self.srtt = if self.srtt.is_zero() { s.max(Duration::from_micros(1)) } else { (7 * self.srtt + s) / 8 };
        }
        if let Some(from) = self.rate_from {
            let elapsed = now.saturating_duration_since(from);
            if !self.min_rtt.is_zero() && elapsed >= self.min_rtt {
                let rate = (self.credited - self.rate_base) as f64 / elapsed.as_secs_f64();
                let raw = (2.0 * rate * self.min_rtt.as_secs_f64()) as i64;
                let est = raw.clamp(MIN_WINDOW, MAX_WINDOW);
                // Frames grow while the link has headroom over the window,
                // and shrink back toward small, low-latency frames when it
                // falls behind. The gap between the two keeps a steady link
                // from flapping. The headroom test uses the unclamped
                // estimate, or frames would stop growing once the window
                // reaches MAX_WINDOW.
                if raw >= self.window + self.window / 4 {
                    self.frame_data = (self.frame_data * 2).min(MAX_FRAME_DATA);
                } else if est < self.window {
                    self.frame_data = (self.frame_data / 2).max(MIN_FRAME_DATA);
                }
                if self.blocked || est > self.window {
                    self.window = est;
                }
                self.blocked = false;
                self.rate_from = Some(now);
                self.rate_base = self.credited;
            }
        }
        done
    }

    /// Arms completion for the SNAPSHOT_END just written, and returns its
    /// generation when it is already due because nothing before it awaits
    /// credit.
    pub fn end_written(&mut self) -> Option<u32> {
        self.end_at = self.sent;
        self.end_gen = self.last_end;
        self.end_armed = self.credited < self.end_at;
        (!self.end_armed).then_some(self.end_gen)
    }

    /// Drops a snapshot queued behind another one in progress, and only it,
    /// reporting whether there was one.
    pub fn drop_queued_snapshot(&mut self) -> bool {
        for i in (1..self.data.len()).rev() {
            if self.data[i].is_marker(wire::TYPE_SNAPSHOT_BEGIN) {
                self.take(i, self.data.len() - i);
                return true;
            }
        }
        false
    }

    /// Whether a SNAPSHOT_BEGIN is still waiting to be written: then nothing
    /// of that snapshot has reached the client.
    pub fn begin_queued(&self) -> bool {
        self.data.iter().any(|e| e.is_marker(wire::TYPE_SNAPSHOT_BEGIN))
    }

    /// Whether any of the current snapshot is still waiting to be written.
    pub fn snapshot_queued(&self) -> bool {
        self.data.iter().any(|e| e.kind == Kind::Marker || e.kind == Kind::Chunk)
    }

    /// Whether the snapshot in progress hasn't sent any SNAPSHOT_DATA yet,
    /// so a newer one can replace it at no loss.
    pub fn screen_unstarted(&self) -> bool {
        self.begin_queued() || self.snap_data == 0
    }

    pub fn set_floor(&mut self, n: i64) {
        self.floor = n;
    }

    /// Drops queued data but STREAM_END and makes the writer exit once the
    /// rest is written.
    pub fn close(&mut self) {
        self.closed = true;
        self.drop_data();
    }

    /// Counts n bytes as in flight.
    fn sent_bytes(&mut self, n: usize, now: Instant) {
        if self.rate_from.is_none() {
            self.rate_from = Some(now);
            self.rate_base = self.credited;
        }
        self.sent += n as u64;
        self.marks.push_back((self.sent, now));
    }

    fn snap_sent(&mut self, ty: u8, n: usize) {
        match ty {
            wire::TYPE_SNAPSHOT_BEGIN => self.snap_data = 0,
            wire::TYPE_SNAPSHOT_DATA => self.snap_data += n as i64,
            wire::TYPE_SNAPSHOT_END => self.end_sent_at = Some((self.now)()),
            _ => {}
        }
    }

    /// Removes data[i..i+n] once sent.
    fn take(&mut self, i: usize, n: usize) {
        for e in self.data.drain(i..i + n) {
            if e.kind == Kind::Record {
                self.lag -= e.rec().cost() as i64;
            }
            if e.is_marker(wire::TYPE_SNAPSHOT_READY) {
                self.ready -= 1;
            }
        }
    }

    /// Takes the next frame that may go out now and appends it,
    /// length-prefixed, to out. Only the frames in out are beyond the reach
    /// of drop_data while the socket blocks.
    pub fn next_into(&mut self, out: &mut Vec<u8>) -> Option<Sent> {
        let now = (self.now)();
        if let Some(f) = self.prio.pop_front() {
            header(out, f.ty, f.stream, f.payload.len());
            out.extend_from_slice(&f.payload);
            return Some(Sent { ty: f.ty, stream: f.stream, gen: 0 });
        }
        if self.data.is_empty() {
            return None;
        }
        let inflight = (self.sent - self.credited) as i64;
        let avail = self.window.max(self.floor) - inflight;
        let mut start = 0;
        if self.data[0].is_history() {
            // Output waiting behind history goes first: records may
            // interleave between READY and END, and history must not hold
            // back the live screen. History gets what credit is left.
            while start < self.data.len()
                && (self.data[start].is_history() || self.data[start].is_marker(wire::TYPE_SNAPSHOT_END))
            {
                start += 1;
            }
            if start == self.data.len() || self.data[start].kind != Kind::Record {
                start = 0;
            }
        }
        let kind = self.data[start].kind;
        match kind {
            Kind::Marker | Kind::End => {
                let e = self.data.pop_front().unwrap();
                if e.is_marker(wire::TYPE_SNAPSHOT_READY) {
                    self.ready -= 1;
                }
                if e.ty == wire::TYPE_SNAPSHOT_END {
                    self.last_end = e.gen;
                }
                self.mark_gen = e.gen;
                let Body::Frame(p) = &e.body else { unreachable!() };
                header(out, e.ty, e.stream, p.len());
                out.extend_from_slice(p);
                self.snap_sent(e.ty, p.len());
                Some(Sent { ty: e.ty, stream: e.stream, gen: e.gen })
            }
            Kind::Chunk => {
                let e = &mut self.data[0];
                let Body::Chunk { buf, off, end } = &mut e.body else { unreachable!() };
                let mut n = (*end - *off) as i64;
                if n > avail {
                    if inflight > 0 && avail < n.min(MIN_SPLIT) {
                        self.blocked = true;
                        return None;
                    }
                    n = avail.max(n.min(MIN_SPLIT));
                }
                let n = n as usize;
                let (ty, stream) = (e.ty, e.stream);
                header(out, ty, stream, n);
                out.extend_from_slice(&buf[*off..*off + n]);
                *off += n;
                if off == end {
                    self.take(0, 1);
                }
                self.snap_sent(ty, n);
                self.sent_bytes(n, now);
                Some(Sent { ty, stream, gen: 0 })
            }
            Kind::Record => {
                // One event, or a run of output records up to the frame size.
                // The first record always goes when nothing is in flight, so
                // one larger than the window can't wedge the stream.
                let first = &self.data[start];
                let (stream, first_kind) = (first.stream, first.rec().kind);
                let mut size = 8i64;
                let mut i = 0;
                for d in self.data.range(start..) {
                    if d.kind != Kind::Record
                        || d.stream != stream
                        || (i > 0 && (d.rec().kind != REC_OUTPUT || first_kind != REC_OUTPUT))
                    {
                        break;
                    }
                    let next = size + d.rec().data.len() as i64;
                    if i > 0 && next > self.frame_data {
                        break;
                    }
                    if next > avail && (i > 0 || inflight > 0) {
                        break;
                    }
                    size = next;
                    i += 1;
                }
                if i == 0 {
                    self.blocked = true;
                    return None;
                }
                let last_end = self.data[start + i - 1].rec().end();
                let ty = if i == 1 && first_kind == REC_EVENT { wire::TYPE_EVENT } else { wire::TYPE_OUTPUT };
                header(out, ty, stream, size as usize);
                out.extend_from_slice(&last_end.to_le_bytes());
                for d in self.data.range(start..start + i) {
                    out.extend_from_slice(&d.rec().data);
                }
                self.take(start, i);
                self.sent_bytes(size as usize, now);
                Some(Sent { ty, stream, gen: 0 })
            }
        }
    }
}

