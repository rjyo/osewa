//! Bounds on what one client connection may ask for. Every enrolled device
//! can already run anything as this user, so these guard the host against a
//! runaway or buggy client, not against the device.

use std::io::Write;
use std::time::{Duration, Instant};

use osewa_wire as wire;

pub(crate) const MAX_CLIENT_ID: usize = 128;

/// Rejects ids that would be unsafe to show: holders report the owner's id
/// to every client, and `osewa ls` prints it.
pub(crate) fn valid_client_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_CLIENT_ID && !id.chars().any(char::is_control)
}

/// Reports whether cols x rows is within wire's bounds.
pub(crate) fn valid_size(cols: i64, rows: i64) -> bool {
    cols > 0 && rows > 0 && cols <= wire::MAX_COLS && rows <= wire::MAX_ROWS
}

/// The message for a size valid_size refuses.
pub(crate) fn err_size() -> String {
    format!("size must be 1..{} by 1..{}", wire::MAX_COLS, wire::MAX_ROWS)
}

/// Passes at most burst lines per interval and then one line saying how
/// many it dropped: anyone who can reach the port can fail a handshake, and
/// each failure is a log line.
pub(crate) struct LimitedLog<W: Write> {
    w: W,
    burst: usize,
    every: Duration,
    start: Option<Instant>,
    n: usize,
    dropped: usize,
}

impl<W: Write> LimitedLog<W> {
    pub fn new(w: W, burst: usize, every: Duration) -> LimitedLog<W> {
        LimitedLog { w, burst, every, start: None, n: 0, dropped: 0 }
    }

    pub fn line_at(&mut self, line: &str, now: Instant) {
        if self.start.is_none_or(|s| now.saturating_duration_since(s) >= self.every) {
            if self.dropped > 0 {
                let _ = writeln!(self.w, "osewa server: {} more log lines dropped", self.dropped);
            }
            self.start = Some(now);
            self.n = 0;
            self.dropped = 0;
        }
        if self.n >= self.burst {
            self.dropped += 1;
            return;
        }
        self.n += 1;
        let _ = writeln!(self.w, "{line}");
    }

    pub fn line(&mut self, line: &str) {
        self.line_at(line, Instant::now())
    }

    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_log() {
        let mut now = Instant::now();
        let mut l = LimitedLog::new(Vec::new(), 2, Duration::from_secs(60));
        for _ in 0..5 {
            l.line_at("handshake error", now);
        }
        now += Duration::from_secs(60);
        l.line_at("later", now);
        let out = String::from_utf8(l.into_inner()).unwrap();
        assert_eq!(out, "handshake error\nhandshake error\nosewa server: 3 more log lines dropped\nlater\n");
    }

    #[test]
    fn client_ids() {
        for id in ["evil\x1b]52;c;aGk=\x07", "a\u{9b}31mb", "tab\there", &"x".repeat(129), ""] {
            assert!(!valid_client_id(id), "{id:?}");
        }
        assert!(valid_client_id("ノートPC-3f2a"));
        assert!(valid_client_id(&"x".repeat(128)));
    }
}
