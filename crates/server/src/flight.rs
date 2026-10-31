//! In-flight windows for channel streams, as the holder sizes session
//! streams (docs/PROTOCOL.md, Flow control and In flight).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) const FLIGHT_INITIAL: usize = 64 << 10;
pub(crate) const FLIGHT_MIN: usize = 16 << 10;
pub(crate) const FLIGHT_MAX: usize = 4 << 20;
const FLIGHT_RTT_SPAN: Duration = Duration::from_secs(10);
/// The queue a stream may build on the path ahead of a keystroke's echo
/// while someone types; QUEUE_IDLE while nobody does, which lets a transfer
/// fill a path that queues little (most do at full speed) and bounds what
/// the next keystroke waits behind on one that would queue seconds (a deep
/// buffer before a slow link).
const QUEUE_TARGET: Duration = Duration::from_millis(40);
const QUEUE_IDLE: Duration = Duration::from_millis(200);
/// Bounds the round trips base keeps.
const FLIGHT_RTTS: usize = 128;
/// How far above the minimum round trip base may sit.
pub(crate) const JITTER_MAX: Duration = Duration::from_millis(20);

/// Keeps a channel stream's uncredited CHANNEL_DATA near what the link
/// carries per round trip. Credit alone bounds buffering, not latency: on a
/// slow link a window of credit is seconds of queue that a keystroke's echo
/// would wait behind, in the TCP send buffer where priority no longer
/// applies.
#[derive(Debug)]
pub(crate) struct Flight {
    pub window: usize,
    pub sent: u64,
    pub credited: u64,
    marks: VecDeque<Mark>,
    /// When credit last came.
    credited_at: Option<Instant>,
    /// The shortest credit round trip in the last FLIGHT_RTT_SPAN. Sizing
    /// from it keeps the queue this stream builds from inflating the window
    /// that builds it.
    min_rtt: Duration,
    min_rtt_at: Option<Instant>,
    /// The credit round trips of the last FLIGHT_RTT_SPAN, for the base the
    /// queue allowance is measured from (base).
    rtts: VecDeque<(Duration, Instant)>,
    /// Waited on the window since the last resize.
    pub blocked: bool,
    /// Bytes per second credited, as last measured.
    rate: f64,
    /// The rates measured in the last FLIGHT_RTT_SPAN, for rate_max.
    rates: VecDeque<(f64, Instant)>,
    /// Sized for someone typing, not left open.
    pub capped: bool,
    /// When the round the window is next resized after began.
    rate_from: Option<Instant>,
    /// The round's highest delivery rate (Mark).
    round_rate: f64,
    /// The round's shortest credit round trip: a queue raises them all,
    /// while the client's batching delays only the last credit of a burst.
    round_min: Option<Duration>,
}

/// A send: where it ends, when it went, and how much had been credited by
/// then and when, as BBR's delivery rate samples. Its credit gives the rate
/// as the bytes credited since over the time since. Averaged over a round
/// instead, a stream held to its window measures the window's tail: it
/// sends the window, idles until credit comes, and the round ends a credit
/// step into the next burst, so it sizes the window down from a rate it
/// never had.
#[derive(Debug, Clone, Copy)]
struct Mark {
    end: u64,
    at: Instant,
    credited: u64,
    credited_at: Instant,
}

impl Default for Flight {
    fn default() -> Flight {
        Flight {
            window: FLIGHT_INITIAL,
            sent: 0,
            credited: 0,
            marks: VecDeque::new(),
            credited_at: None,
            min_rtt: Duration::ZERO,
            min_rtt_at: None,
            rtts: VecDeque::new(),
            blocked: false,
            rate: 0.0,
            rates: VecDeque::new(),
            capped: false,
            rate_from: None,
            round_rate: 0.0,
            round_min: None,
        }
    }
}

fn clamp_window(rate: f64, d: Duration) -> usize {
    ((rate * d.as_secs_f64()) as usize).clamp(FLIGHT_MIN, FLIGHT_MAX)
}

impl Flight {
    pub fn in_flight(&self) -> u64 {
        self.sent - self.credited
    }

    /// How many more bytes may be in flight; a caller that finds too little
    /// marks the window as binding.
    pub fn room(&self) -> i64 {
        self.window as i64 - self.in_flight() as i64
    }

    #[cfg(test)]
    pub fn marks(&self) -> usize {
        self.marks.len()
    }

    pub fn send(&mut self, n: usize, now: Instant) {
        // With nothing in flight the link was idle, not slow: measure from
        // now, or the first credit after a pause would count the pause.
        let idle = self.sent == self.credited;
        if self.rate_from.is_none() {
            self.rate_from = Some(now);
        }
        let credited_at = match self.credited_at {
            Some(t) if !idle => t,
            _ => now,
        };
        self.sent += n as u64;
        self.marks.push_back(Mark { end: self.sent, at: now, credited: self.credited, credited_at });
    }

    /// Applies a CREDIT and resizes the window once per round trip: to what
    /// the link carries per minimum round trip plus the queue allowed,
    /// QUEUE_TARGET while someone types on the connection (typing) and
    /// QUEUE_IDLE otherwise, so the window grows while the path queues less
    /// than that. It shrinks only when it actually held data back, so a
    /// quiet stream keeps its window; the first keystroke after a quiet
    /// spell shrinks it at once (tighten).
    pub fn credit(&mut self, n: u32, now: Instant, typing: bool) {
        self.credited = (self.credited + n as u64).min(self.sent);
        self.credited_at = Some(now);
        let mut acked = None;
        while let Some(&m) = self.marks.front() {
            if m.end > self.credited {
                break;
            }
            acked = Some(m);
            self.marks.pop_front();
        }
        let sample = acked.map(|m| now.saturating_duration_since(m.at));
        if let Some(m) = acked {
            let d = now.saturating_duration_since(m.credited_at);
            if !d.is_zero() {
                self.round_rate = self.round_rate.max((self.credited - m.credited) as f64 / d.as_secs_f64());
            }
        }
        if let Some(s) = sample {
            let expired = self.min_rtt_at.is_some_and(|at| now.saturating_duration_since(at) > FLIGHT_RTT_SPAN);
            if self.min_rtt.is_zero() || s <= self.min_rtt || expired {
                self.min_rtt = s.max(Duration::from_micros(1));
                self.min_rtt_at = Some(now);
            }
            self.add_rtt(s, now);
            self.round_min = Some(self.round_min.map_or(s, |m| m.min(s)));
        }
        let Some(from) = self.rate_from else { return };
        let elapsed = now.saturating_duration_since(from);
        if !self.min_rtt.is_zero() && elapsed >= self.min_rtt && self.round_rate > 0.0 {
            let rate = self.round_rate;
            self.rate = rate;
            while self.rates.front().is_some_and(|&(_, at)| now.saturating_duration_since(at) > FLIGHT_RTT_SPAN) {
                self.rates.pop_front();
            }
            while self.rates.back().is_some_and(|&(r, _)| r <= rate) {
                self.rates.pop_back();
            }
            self.rates.push_back((rate, now));
            let queue = if typing { QUEUE_TARGET } else { QUEUE_IDLE };
            let mut est = clamp_window(rate, self.base() + queue);
            // The rate was measured under this window, so a window that
            // held data back can't measure its way past itself: while it
            // carried a full window per round trip and the round's round
            // trips show no more queue than allowed, it doubles, as slow
            // start does, and the rate bounds it again once a queue shows.
            // Without this a stream that starts while someone types stays a
            // fraction of the link for as long as they do.
            let filled = rate * self.base().as_secs_f64() >= 0.9 * self.window as f64;
            if self.blocked && filled && self.round_min.is_some_and(|s| s <= self.base() + queue) {
                est = est.max((self.window * 2).min(FLIGHT_MAX));
            }
            if self.blocked || est > self.window || (typing && !self.capped) {
                self.window = est;
            }
            self.capped = typing;
            self.blocked = false;
            self.rate_from = Some(now);
            self.round_rate = 0.0;
            self.round_min = None;
        }
    }

    /// Sizes the window for someone typing as soon as they start, from the
    /// rate last measured, instead of at the next resize: until then the
    /// stream would keep sending into the idle allowance. What is already in
    /// flight stays there.
    pub fn tighten(&mut self) {
        // With nothing in flight there is no queue to keep short, and the
        // rate last measured may be a stream's tail, near nothing: the next
        // resize sizes the window from what it carries then.
        if self.rate > 0.0 && !self.min_rtt.is_zero() && self.sent != self.credited {
            self.window = self.window.min(clamp_window(self.rate, self.base() + QUEUE_TARGET));
        }
        self.capped = true;
    }

    /// The highest rate measured in the last FLIGHT_RTT_SPAN: what the link
    /// carried when let, not what a sender held back by a tight bound
    /// carried, which sized from would shrink that bound again.
    pub fn rate_max(&self, now: Instant) -> f64 {
        self.rates
            .iter()
            .find(|&&(_, at)| now.saturating_duration_since(at) <= FLIGHT_RTT_SPAN)
            .map_or(0.0, |&(r, _)| r)
    }

    /// Takes n bytes out of flight without a credit: a channel ended with
    /// them uncredited, and no credit will come for them. They are no round
    /// trip sample, nor part of the rate.
    pub fn release(&mut self, n: u64) {
        let d = (self.credited + n).min(self.sent) - self.credited;
        self.credited += d;
        self.marks.retain(|m| m.end > self.credited);
        for m in &mut self.marks {
            m.credited += d;
        }
    }

    fn add_rtt(&mut self, d: Duration, now: Instant) {
        self.rtts.push_back((d, now));
        while let Some(&(_, at)) = self.rtts.front() {
            if now.saturating_duration_since(at) > FLIGHT_RTT_SPAN || self.rtts.len() > FLIGHT_RTTS {
                self.rtts.pop_front();
            } else {
                break;
            }
        }
    }

    /// The round trip the queue allowance is added to: the 10th percentile
    /// of the last FLIGHT_RTT_SPAN's, but at most JITTER_MAX above their
    /// minimum. A path's round trip jitters (a tailnet, Wi-Fi, a long
    /// route): against one lucky minimum, the jitter would count as queue,
    /// and a stream held for someone typing would shrink while nothing
    /// queued, to the floor. Uncapped, the percentile would follow the
    /// stream's own queue up, every sample being queued, and stop bounding
    /// it. The minimum still paces resizes.
    pub fn base(&self) -> Duration {
        if self.rtts.is_empty() {
            return self.min_rtt;
        }
        let mut d: Vec<Duration> = self.rtts.iter().map(|&(d, _)| d).collect();
        d.sort_unstable();
        d[d.len() / 10].min(self.min_rtt + JITTER_MAX)
    }
}

/// What a channel whose own window is ch_window may still put in flight
/// under the connection's window cf. The connection's window never binds
/// below that: sized from combined credit, it can measure less than one
/// stream's window does (a stale rate, a sample a small window's credit
/// pacing stretched) and then shrink a lone stream it should leave alone,
/// down to the floor. Many streams together still stay within the largest's
/// window.
pub(crate) fn conn_room(cf: &Flight, ch_window: usize) -> i64 {
    cf.window.max(ch_window) as i64 - cf.in_flight() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // While someone types, the window follows what credit shows the link
    // carrying: the bytes credited per minimum round trip plus QUEUE_TARGET,
    // within its bounds.
    // A stream that starts while someone types, on a path that doesn't
    // queue: each round trip it fills its window and the round trip stays
    // put, so the window doubles to the link instead of staying at the rate
    // it held itself to. Once the round trip grows, it stops.
    #[test]
    fn grows_while_typing_without_a_queue() {
        grows_while_typing(false);
        grows_while_typing(true);
    }

    // credit_steps: the client's way, 8 KiB at once and the rest after
    // its batching delay, not the whole window in one credit.
    fn grows_while_typing(credit_steps: bool) {
        let mut f = Flight::default();
        let mut now = Instant::now();
        let round = |f: &mut Flight, now: &mut Instant, rtt: Duration| {
            let n = f.window;
            f.send(n, *now);
            f.blocked = true;
            let start = *now;
            *now += rtt;
            if !credit_steps {
                f.credit(n as u32, *now, true);
                return;
            }
            let step = 8 << 10;
            for _ in 0..n / step {
                f.credit(step as u32, *now, true);
            }
            if !n.is_multiple_of(step) {
                *now += ms(50);
                f.credit((n % step) as u32, *now, true);
            }
            *now = (*now).max(start + rtt);
        };
        for _ in 0..6 {
            round(&mut f, &mut now, ms(100));
        }
        assert!(f.window >= FLIGHT_INITIAL << 5, "window {} after 6 round trips with no queue", f.window);
        let w = f.window;
        for i in 1..4 {
            round(&mut f, &mut now, ms(100 + 100 * i));
        }
        assert!(f.window <= w, "window {} grew from {w} with a growing queue", f.window);
    }

    // The same on the wire's terms: the server sends frames of up to 32 KiB
    // whenever credit leaves room, the client credits each frame it reads
    // at once if 8 KiB are owed and after its batching delay if not, and
    // nothing queues. Each burst's tail
    // is still out when a round ends, which a rate averaged per round
    // counted as all the round carried. Where a burst's last frame is short
    // of 8 KiB depends on the window it starts from, so it starts from many.
    #[test]
    fn grows_while_typing_as_credit_comes() {
        for w in (16..128).map(|k| (k << 10) + 333) {
            let got = ramp(w);
            assert!(got >= 1 << 20, "window {got} after 3 s with no queue, from {w}");
        }
    }

    fn ramp(start: usize) -> usize {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        const RTT: Duration = Duration::from_millis(100);
        const DELAY: Duration = Duration::from_millis(50);
        const FRAME: i64 = 32 << 10;
        const STEP: u64 = 8 << 10;
        let t0 = Instant::now();
        let mut f = Flight { window: start, ..Flight::default() };
        // (when, arrived bytes, or a credit timer when 0)
        let mut ev: BinaryHeap<Reverse<(Duration, u64)>> = BinaryHeap::new();
        let (mut owed, mut timer) = (0u64, None::<Duration>);
        let fill = |f: &mut Flight, now: Duration, ev: &mut BinaryHeap<Reverse<(Duration, u64)>>| {
            while f.room() > 0 {
                let n = f.room().min(FRAME);
                f.send(n as usize, t0 + now);
                ev.push(Reverse((now + RTT, n as u64)));
            }
            f.blocked = true;
        };
        fill(&mut f, Duration::ZERO, &mut ev);
        while let Some(Reverse((now, n))) = ev.pop() {
            if now > Duration::from_secs(3) {
                break;
            }
            if n > 0 {
                owed += n;
                if owed >= STEP {
                    f.credit(owed as u32, t0 + now, true);
                    owed = 0;
                    timer = None;
                    fill(&mut f, now, &mut ev);
                }
                if owed > 0 && timer.is_none() {
                    timer = Some(now + DELAY);
                    ev.push(Reverse((now + DELAY, 0)));
                }
            } else if timer == Some(now) {
                timer = None;
                if owed > 0 {
                    f.credit(owed as u32, t0 + now, true);
                    owed = 0;
                    fill(&mut f, now, &mut ev);
                }
            }
        }
        f.window
    }

    // rate_max keeps the highest rate of the last FLIGHT_RTT_SPAN, so a
    // bound sized from it isn't shrunk by the rate it held a sender to.
    #[test]
    fn rate_max() {
        let mut f = Flight::default();
        let mut now = Instant::now();
        let step = |f: &mut Flight, now: &mut Instant, n: u32| {
            f.send(n as usize, *now);
            *now += ms(100);
            f.credit(n, *now, false);
        };
        assert_eq!(f.rate_max(now), 0.0);
        for _ in 0..5 {
            step(&mut f, &mut now, 100_000); // 1 MB/s
        }
        for _ in 0..5 {
            step(&mut f, &mut now, 10_000); // held to 100 kB/s
        }
        let r = f.rate_max(now);
        assert!((900_000.0..=1_100_000.0).contains(&r), "rate_max {r}");
        now += FLIGHT_RTT_SPAN;
        step(&mut f, &mut now, 10_000);
        let r = f.rate_max(now);
        assert!((90_000.0..=110_000.0).contains(&r), "rate_max {r} after the fast rates expired");
    }

    #[test]
    fn window() {
        let mut f = Flight::default();
        let mut now = Instant::now();
        // 40 KiB per 100 ms round trip: 400 KiB/s, so a window of 400 KiB/s
        // x 140 ms = 56 KiB.
        for _ in 0..20 {
            f.send(40 << 10, now);
            f.blocked = true;
            now += ms(100);
            f.credit(40 << 10, now, true);
        }
        assert!((54 << 10..=58 << 10).contains(&f.window), "window {}, want ~56 KiB", f.window);
        // Faster: the window grows, up to the maximum.
        for _ in 0..20 {
            f.send(4 << 20, now);
            now += ms(100);
            f.credit(4 << 20, now, true);
        }
        assert_eq!(f.window, FLIGHT_MAX);
        // An idle stream keeps its window; a slow, blocked one shrinks to
        // the minimum.
        now += Duration::from_secs(60);
        f.send(100, now);
        now += ms(100);
        f.credit(100, now, true);
        assert_eq!(f.window, FLIGHT_MAX, "idle window");
        for _ in 0..5 {
            f.send(100, now);
            f.blocked = true;
            now += ms(100);
            f.credit(100, now, true);
        }
        assert_eq!(f.window, FLIGHT_MIN);
        // A burst after a pause is measured from its start, not the pause.
        for _ in 0..20 {
            f.send(4 << 20, now);
            now += ms(100);
            f.credit(4 << 20, now, true);
        }
        now += Duration::from_secs(60);
        f.send(4 << 20, now);
        f.blocked = true;
        now += ms(100);
        f.credit(4 << 20, now, true);
        assert_eq!(f.window, FLIGHT_MAX, "window after an idle pause");
        assert_eq!(f.room(), f.window as i64, "room with nothing in flight");
    }

    // With nobody typing the window allows QUEUE_IDLE of queue; a keystroke
    // tightens it to QUEUE_TARGET within a round trip, whether or not it
    // held data back, and it loosens again once typing stops.
    #[test]
    fn open_until_typing() {
        let mut f = Flight::default();
        let mut now = Instant::now();
        let mut step = |f: &mut Flight, typing: bool| {
            for _ in 0..5 {
                f.send(40 << 10, now);
                now += ms(100);
                f.credit(40 << 10, now, typing);
            }
        };
        // 400 KiB/s x (100 + 200 ms) = 120 KiB.
        step(&mut f, false);
        assert!((118 << 10..=122 << 10).contains(&f.window), "idle window {}", f.window);
        step(&mut f, true);
        assert!((54 << 10..=58 << 10).contains(&f.window), "typing window {}", f.window);
        step(&mut f, false);
        assert!((118 << 10..=122 << 10).contains(&f.window), "window {} after typing stopped", f.window);
    }

    // A round trip that jitters with nothing queued doesn't shrink a stream
    // held for someone typing: most round trips take 130-150 ms and one in
    // ten a lucky 100. Measured from that minimum, the jitter would count as
    // queue past the allowance; from a low percentile of recent ones, it
    // doesn't.
    #[test]
    fn tolerates_jitter() {
        let mut f = Flight::default();
        let mut now = Instant::now();
        for i in 0..60u64 {
            let rtt = if i % 10 == 0 { ms(100) } else { ms(130 + 10 * (i % 3)) };
            let w = f.window;
            f.send(w, now);
            f.blocked = true;
            now += rtt;
            f.credit(w as u32, now, true);
        }
        assert!(f.window >= FLIGHT_INITIAL, "window shrank to {} under jitter alone", f.window);
    }

    // A stream's own queue doesn't raise the base it is bounded from: with
    // every round trip since the minimum queued (300 ms over a 100 ms path),
    // base stays within JITTER_MAX of it. (The minimum itself expires after
    // FLIGHT_RTT_SPAN; a queue standing that long raises it, as before.)
    #[test]
    fn base_ignores_own_queue() {
        let mut f = Flight::default();
        let mut now = Instant::now();
        f.send(1, now);
        now += ms(100);
        f.credit(1, now, true);
        for _ in 0..30 {
            f.send(1, now);
            now += ms(300);
            f.credit(1, now, true);
        }
        assert!(f.base() <= ms(100) + JITTER_MAX, "base {:?} followed the queue up", f.base());
    }

    // The first keystroke tightens an idle window at once, from the rate
    // last measured, not at the next resize.
    #[test]
    fn tightens_on_typing() {
        let mut f = Flight::default();
        let mut now = Instant::now();
        for _ in 0..5 {
            f.send(40 << 10, now);
            now += ms(100);
            f.credit(40 << 10, now, false);
        }
        assert!(f.window >= 118 << 10, "idle window {}", f.window);
        // Nothing in flight: nothing to keep short, and no fresh rate.
        f.tighten();
        assert!(f.window >= 118 << 10 && f.capped, "idle window tightened to {}", f.window);
        f.send(40 << 10, now);
        f.tighten();
        assert!((54 << 10..=58 << 10).contains(&f.window), "tightened window {}", f.window);
    }

    // release takes bytes out of flight without counting them as credit.
    #[test]
    fn release() {
        let mut f = Flight::default();
        f.send(40 << 10, Instant::now());
        f.release(40 << 10);
        assert!(f.room() == f.window as i64 && f.marks() == 0);
        f.release(1 << 20); // never past what was sent
        assert_eq!(f.credited, f.sent);
    }
}
