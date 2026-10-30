//! permessage-deflate (RFC 7692) bodies: raw deflate, each message
//! sync-flushed with the empty stored block's 00 00 ff ff left off.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

pub(crate) const TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// Compresses messages in one deflate stream, so each can refer back to
/// the ones before (context takeover).
pub(crate) struct Deflater(Compress);

impl Deflater {
    /// coder/websocket compresses with flate.BestSpeed: a frame waits on
    /// its compression before it goes. miniz_oxide's level 2 matches its
    /// ratio on terminal output (level 1 falls 11% short) at about the
    /// same speed (tests/compress.rs).
    pub fn new() -> Deflater {
        Deflater(Compress::new(Compression::new(2), false))
    }

    pub fn reset(&mut self) {
        self.0.reset();
    }

    /// Appends p's compressed body to out.
    pub fn compress(&mut self, p: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        let in0 = self.0.total_in();
        loop {
            let done = (self.0.total_in() - in0) as usize;
            if out.capacity() - out.len() < 64 {
                out.reserve((p.len() - done) / 2 + 1024);
            }
            // Raw deflate into a Vec with room to spare can't fail.
            self.0
                .compress_vec(&p[done..], out, FlushCompress::Sync)
                .expect("deflate");
            if (self.0.total_in() - in0) as usize == p.len() && out.len() < out.capacity() {
                break;
            }
        }
        debug_assert!(out[start..].ends_with(&TAIL));
        if out[start..].ends_with(&TAIL) {
            out.truncate(out.len() - TAIL.len());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InflateError {
    TooBig,
    Corrupt(String),
}

/// Inflates a peer's messages. Its window carries over from one message to
/// the next unless reset.
pub(crate) struct Inflater(Decompress);

impl Inflater {
    pub fn new() -> Inflater {
        Inflater(Decompress::new(false))
    }

    pub fn reset(&mut self) {
        self.0.reset(false);
    }

    /// Appends what p inflates to to out, failing once out holds more than
    /// limit bytes.
    pub fn inflate(&mut self, p: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<(), InflateError> {
        let in0 = self.0.total_in();
        loop {
            if out.len() > limit {
                return Err(InflateError::TooBig);
            }
            let done = (self.0.total_in() - in0) as usize;
            if out.capacity() - out.len() < 16 << 10 {
                out.reserve(32 << 10);
            }
            let before = out.len();
            let status = self
                .0
                .decompress_vec(&p[done..], out, FlushDecompress::None)
                .map_err(|e| InflateError::Corrupt(e.to_string()))?;
            let consumed = (self.0.total_in() - in0) as usize;
            if status == Status::StreamEnd {
                // A final block ends the stream; a peer that sends one
                // starts the next message afresh (we can't preset a window).
                self.reset();
                if consumed < p.len() {
                    continue;
                }
            }
            if consumed == p.len() && out.len() < out.capacity() {
                break;
            }
            if consumed == done && out.len() == before {
                break;
            }
        }
        if out.len() > limit {
            return Err(InflateError::TooBig);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> Vec<u8> {
        let line = b"the quick brown fox \x1b[1;32mjumps\x1b[0m\r\n";
        (0..n).map(|i| line[i % line.len()]).collect()
    }

    #[test]
    fn round_trip_with_context() {
        let mut d = Deflater::new();
        let mut i = Inflater::new();
        let msg = text(1000);
        let mut sizes = Vec::new();
        for _ in 0..5 {
            let mut body = Vec::new();
            d.compress(&msg, &mut body);
            sizes.push(body.len());
            body.extend_from_slice(&TAIL);
            let mut out = Vec::new();
            i.inflate(&body, &mut out, 1 << 20).unwrap();
            assert_eq!(out, msg);
        }
        // Later copies refer back to the first.
        assert!(sizes[1] < sizes[0] / 2, "{sizes:?}");
    }

    #[test]
    fn reset_drops_context() {
        let mut d = Deflater::new();
        let msg = text(1000);
        let mut first = Vec::new();
        d.compress(&msg, &mut first);
        d.reset();
        let mut again = Vec::new();
        d.compress(&msg, &mut again);
        assert_eq!(first, again);
    }

    #[test]
    fn large_and_incompressible() {
        let mut d = Deflater::new();
        let mut i = Inflater::new();
        let mut x = 0x9e3779b97f4a7c15u64;
        let noise: Vec<u8> = (0..300_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for msg in [noise, text(1 << 20), Vec::new()] {
            let mut body = Vec::new();
            d.compress(&msg, &mut body);
            body.extend_from_slice(&TAIL);
            let mut out = Vec::new();
            i.inflate(&body, &mut out, 1 << 20).unwrap();
            assert_eq!(out, msg);
        }
    }

    #[test]
    fn inflate_limit() {
        let mut d = Deflater::new();
        let msg = text(100_000);
        let mut body = Vec::new();
        d.compress(&msg, &mut body);
        body.extend_from_slice(&TAIL);
        let mut out = Vec::new();
        assert_eq!(Inflater::new().inflate(&body, &mut out, 99_999), Err(InflateError::TooBig));
        out.clear();
        assert_eq!(Inflater::new().inflate(&body, &mut out, 100_000), Ok(()));
    }

    #[test]
    fn inflate_corrupt() {
        let mut out = Vec::new();
        assert!(matches!(
            Inflater::new().inflate(&[0xff, 0xff, 0xff, 0xff], &mut out, 1 << 20),
            Err(InflateError::Corrupt(_))
        ));
    }

    #[test]
    fn inflate_in_pieces() {
        let mut d = Deflater::new();
        let msg = text(50_000);
        let mut body = Vec::new();
        d.compress(&msg, &mut body);
        body.extend_from_slice(&TAIL);
        let mut i = Inflater::new();
        let mut out = Vec::new();
        for piece in body.chunks(7) {
            i.inflate(piece, &mut out, 1 << 20).unwrap();
        }
        assert_eq!(out, msg);
    }
}
