//! Stamped output (Go's bench/stamp.go) carries lines "@T<unix ms>@" so
//! whoever renders the screen can tell how old it is. Containers on one host
//! share the kernel clock, so a stamp written in one compares with the clock
//! in another.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unix milliseconds of t (negative before the epoch).
pub fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// The time at unix milliseconds ms.
pub fn from_unix_ms(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

/// The stamp line for t.
pub fn stamp(t: SystemTime) -> Vec<u8> {
    format!("@T{}@\r\n", unix_ms(t)).into_bytes()
}

/// The byte offsets [start, end) of every stamp in s: Go's
/// `@T(\d{13})@`, leftmost and non-overlapping.
pub fn stamp_indexes(s: &str) -> Vec<[usize; 2]> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 16 <= b.len() {
        if b[i] == b'@' && b[i + 1] == b'T' && b[i + 2..i + 15].iter().all(u8::is_ascii_digit) && b[i + 15] == b'@' {
            out.push([i, i + 16]);
            i += 16;
        } else {
            i += 1;
        }
    }
    out
}

/// The newest stamp in s, or None.
pub fn newest_stamp(s: &str) -> Option<SystemTime> {
    let best = stamp_indexes(s).iter().map(|&[a, _]| s[a + 2..a + 15].parse::<i64>().unwrap_or(0)).max().unwrap_or(0);
    (best != 0).then(|| from_unix_ms(best))
}
