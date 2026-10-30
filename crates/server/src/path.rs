//! The kernel's view of a client's TCP path: what it holds unacked or
//! unsent, and the path's minimum round trip.
//!
//! Credit sees the path through the client: its round trips include the
//! client's credit batching (8 KiB at 1 Mbit/s is 64 ms) and its in-flight
//! count includes bytes the client holds uncredited. Neither tells how much
//! sits in the bottleneck's queue ahead of a keystroke's echo. The kernel
//! knows both exactly.

use std::cell::Cell;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// One reading of the socket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Bytes the kernel has for the path: in flight, plus unsent.
    pub queued: u64,
    /// The path's minimum round trip, without any queue.
    pub min_rtt: Duration,
}

/// How long a minimum round trip stands on macOS, which reports only the
/// latest one: a route that changed must not keep an old, shorter minimum.
#[cfg(target_os = "macos")]
const MIN_RTT_SPAN: Duration = Duration::from_secs(30);

pub(crate) struct Probe {
    fd: RawFd,
    /// macOS's running minimum and when it was taken.
    #[allow(dead_code)]
    min: Cell<Option<(Duration, Instant)>>,
}

impl Probe {
    /// fd must stay open for the probe's life: the connection that owns it
    /// owns the probe.
    pub fn new(fd: RawFd) -> Probe {
        Probe { fd, min: Cell::new(None) }
    }

    /// Reads the socket; None where the platform can't say or before the
    /// first round trip.
    pub fn sample(&self, now: Instant) -> Option<Sample> {
        self.read(now)
    }

    #[cfg(target_os = "linux")]
    fn read(&self, _now: Instant) -> Option<Sample> {
        // struct tcp_info's layout only ever grows; these offsets have been
        // fixed since Linux 4.6 (tcpi_min_rtt). Reading into a raw buffer
        // avoids depending on how much of it the libc crate declares.
        let mut b = [0u8; 160];
        let n = getsockopt(self.fd, libc::IPPROTO_TCP, libc::TCP_INFO, &mut b)?;
        if n < 152 {
            return None;
        }
        let snd_mss = u32_at(&b, 16) as u64;
        // tcp_packets_in_flight(): lost and SACKed segments have left the
        // path, and counting them would hold bulk back through every loss
        // recovery.
        let (unacked, sacked, lost, retrans) = (u32_at(&b, 24), u32_at(&b, 28), u32_at(&b, 32), u32_at(&b, 36));
        let in_flight = (unacked + retrans).saturating_sub(sacked + lost) as u64;
        let notsent = u32_at(&b, 144) as u64;
        let min_rtt = u32_at(&b, 148);
        if min_rtt == 0 || min_rtt == u32::MAX {
            return None;
        }
        Some(Sample { queued: in_flight * snd_mss + notsent, min_rtt: Duration::from_micros(min_rtt as u64) })
    }

    #[cfg(target_os = "macos")]
    fn read(&self, now: Instant) -> Option<Sample> {
        // The libc crate declares struct tcp_connection_info's bitfield as
        // fifteen u32s; the offsets here are <netinet/tcp.h>'s.
        let mut b = [0u8; 52];
        let n = getsockopt(self.fd, libc::IPPROTO_TCP, libc::TCP_CONNECTION_INFO, &mut b)?;
        if n < 48 {
            return None;
        }
        let sbbytes = u32_at(&b, 32) as u64;
        let rttcur = u32_at(&b, 40);
        if rttcur == 0 {
            return self.min.get().map(|(m, _)| Sample { queued: sbbytes, min_rtt: m });
        }
        let cur = Duration::from_millis(rttcur as u64);
        let min = match self.min.get() {
            Some((m, at)) if m <= cur && now.saturating_duration_since(at) <= MIN_RTT_SPAN => (m, at),
            _ => (cur, now),
        };
        self.min.set(Some(min));
        Some(Sample { queued: sbbytes, min_rtt: min.0 })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn read(&self, _now: Instant) -> Option<Sample> {
        let _ = self.fd;
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn getsockopt(fd: RawFd, level: libc::c_int, opt: libc::c_int, b: &mut [u8]) -> Option<usize> {
    let mut len = b.len() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, level, opt, b.as_mut_ptr().cast(), &mut len) };
    (r == 0).then_some(len as usize)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(b[off..off + 4].try_into().unwrap())
}

/// The queue bulk data may build in the path, beyond its minimum round
/// trip, while someone types: an echo waits behind all of it. Small, as the
/// kernel's numbers have no client batching or jitter of our own in them.
pub(crate) const TYPING_QUEUE: Duration = Duration::from_millis(10);

/// The path may always hold this much bulk: with nothing in flight there
/// are no round trips to measure by, and the floor keeps a slow link busy.
pub(crate) const PATH_FLOOR: u64 = 4 << 10;

/// The rate a bound sized from a measured rate allows, by phase: one round
/// trip in PROBE_CYCLE a little more, the next a little less, as BBR probes.
/// Sized from a rate measured under that same bound and nothing else, a
/// bound can only ever shrink (a lossy round trip, a slow credit), never
/// find the room the link has again; the probe finds it, and the round trip
/// after drains the queue the probe built.
const PROBE_CYCLE: u32 = 8;

pub(crate) fn probe_gain(since: Duration, min_rtt: Duration) -> f64 {
    if min_rtt.is_zero() {
        return 1.0;
    }
    match (since.as_nanos() / min_rtt.as_nanos()) as u32 % PROBE_CYCLE {
        0 => 1.25,
        1 => 0.75,
        _ => 1.0,
    }
}

/// How many more bulk bytes may go to a path whose kernel holds s, while
/// waiting bytes are already queued above the kernel, for a link carrying
/// rate bytes per second: enough for its minimum round trip plus
/// TYPING_QUEUE.
pub(crate) fn room(s: Sample, rate: f64, waiting: u64) -> i64 {
    let allowed = ((rate * (s.min_rtt + TYPING_QUEUE).as_secs_f64()) as u64).max(PATH_FLOOR);
    allowed as i64 - (s.queued + waiting) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn room_is_a_round_trip_and_a_little_queue() {
        // 125 kB/s over 100 ms: 12.5 kB carried per round trip, plus 10 ms
        // of queue, 1.25 kB.
        let s = Sample { queued: 0, min_rtt: ms(100) };
        assert_eq!(room(s, 125_000.0, 0), 13_750);
        assert_eq!(room(Sample { queued: 10_000, ..s }, 125_000.0, 3_000), 750);
        assert!(room(Sample { queued: 20_000, ..s }, 125_000.0, 0) < 0);
    }

    // One round trip in eight probes above the rate, the next drains below
    // it, and the rest hold it.
    #[test]
    fn probe_cycle() {
        let rtt = ms(100);
        let gains: Vec<f64> = (0..16).map(|i| probe_gain(ms(i * 100 + 50), rtt)).collect();
        assert_eq!(&gains[..8], &[1.25, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!(&gains[8..], &gains[..8]);
        assert_eq!(probe_gain(ms(5), Duration::ZERO), 1.0);
    }

    #[test]
    fn room_keeps_a_floor() {
        let s = Sample { queued: 0, min_rtt: ms(1) };
        assert_eq!(room(s, 0.0, 0), PATH_FLOOR as i64);
        assert_eq!(room(s, 1000.0, 1000), PATH_FLOOR as i64 - 1000);
    }

    // A connected socket reads back with the kernel's numbers: nothing
    // queued once its writes are acked, and a round trip.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn probe_reads_a_socket() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut a = std::net::TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (mut b, _) = ln.accept().unwrap();
        let p = Probe::new(b.as_raw_fd());
        let mut buf = [0u8; 4];
        for _ in 0..3 {
            b.write_all(b"ping").unwrap();
            a.read_exact(&mut buf).unwrap();
            a.write_all(b"pong").unwrap();
            b.read_exact(&mut buf).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match p.sample(Instant::now()) {
                Some(s) if s.queued == 0 => {
                    assert!(s.min_rtt < Duration::from_secs(1), "{s:?}");
                    break;
                }
                // macOS reports round trips in whole ms, 0 on loopback.
                None if cfg!(target_os = "macos") => break,
                s => assert!(Instant::now() < deadline, "{s:?}"),
            }
            std::thread::sleep(ms(10));
        }
    }
}
