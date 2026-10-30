//! The transfer pattern (Go's bench/pattern.go): deterministic
//! pseudo-random bytes that the e2e transfer tests send through forwards and
//! exec streams and check at the far end. The Go harness makes and checks
//! them too, so they are Go's math/rand/v2 ChaCha8 stream, bit for bit
//! (internal/chacha8rand: four interleaved ChaCha8 blocks per refill, a
//! reseed from the last four words every 16 blocks).

use std::fmt;
use std::io::{self, Read};

/// Fixes the pattern's bytes, so both ends of a transfer can make them
/// without sending a hash.
const SEED: &[u8; 32] = b"osewa e2e transfer pattern v1...";

/// Go's chacha8rand.State.
struct ChaCha8 {
    buf: [u64; 32],
    seed: [u64; 4],
    i: usize,
    n: usize,
    c: u32,
}

const CTR_INC: u32 = 4;
const CTR_MAX: u32 = 16;
const CHUNK: usize = 32;
const RESEED: usize = 4;

fn qr(a: u32, b: u32, c: u32, d: u32) -> (u32, u32, u32, u32) {
    let a = a.wrapping_add(b);
    let d = (d ^ a).rotate_left(16);
    let c = c.wrapping_add(d);
    let b = (b ^ c).rotate_left(12);
    let a = a.wrapping_add(b);
    let d = (d ^ a).rotate_left(8);
    let c = c.wrapping_add(d);
    let b = (b ^ c).rotate_left(7);
    (a, b, c, d)
}

/// Go's block_generic: four ChaCha8 blocks with counters counter..counter+3,
/// laid out lane by lane as Go's [16][4]uint32 view of buf.
fn block(seed: &[u64; 4], buf: &mut [u64; 32], counter: u32) {
    // Row j, lane i, as Go's [16][4]uint32: every lane the same but the
    // counter.
    let consts = [0x61707865u32, 0x3320646e, 0x79622d32, 0x6b206574];
    let input = |j: usize, i: usize| -> u32 {
        match j {
            0..=3 => consts[j],
            4..=11 => {
                let s = seed[(j - 4) / 2];
                if j.is_multiple_of(2) {
                    s as u32
                } else {
                    (s >> 32) as u32
                }
            }
            12 => counter + i as u32,
            _ => 0,
        }
    };
    let mut w = [[0u32; 4]; 16];
    for i in 0..4 {
        let mut x: [u32; 16] = std::array::from_fn(|j| input(j, i));
        for _ in 0..4 {
            (x[0], x[4], x[8], x[12]) = qr(x[0], x[4], x[8], x[12]);
            (x[1], x[5], x[9], x[13]) = qr(x[1], x[5], x[9], x[13]);
            (x[2], x[6], x[10], x[14]) = qr(x[2], x[6], x[10], x[14]);
            (x[3], x[7], x[11], x[15]) = qr(x[3], x[7], x[11], x[15]);
            (x[0], x[5], x[10], x[15]) = qr(x[0], x[5], x[10], x[15]);
            (x[1], x[6], x[11], x[12]) = qr(x[1], x[6], x[11], x[12]);
            (x[2], x[7], x[8], x[13]) = qr(x[2], x[7], x[8], x[13]);
            (x[3], x[4], x[9], x[14]) = qr(x[3], x[4], x[9], x[14]);
        }
        for (j, row) in w.iter_mut().enumerate() {
            // Only the seed rows get the input added back.
            row[i] = if (4..12).contains(&j) { input(j, i).wrapping_add(x[j]) } else { x[j] };
        }
    }
    for (k, out) in buf.iter_mut().enumerate() {
        let (lo, hi) = (2 * k, 2 * k + 1);
        *out = w[lo / 4][lo % 4] as u64 | (w[hi / 4][hi % 4] as u64) << 32;
    }
}

impl ChaCha8 {
    fn new(seed: &[u8; 32]) -> ChaCha8 {
        let seed = std::array::from_fn(|k| u64::from_le_bytes(seed[k * 8..k * 8 + 8].try_into().unwrap()));
        let mut s = ChaCha8 { buf: [0; 32], seed, i: 0, n: CHUNK, c: 0 };
        block(&s.seed, &mut s.buf, 0);
        s
    }

    fn refill(&mut self) {
        self.c += CTR_INC;
        if self.c == CTR_MAX {
            self.seed.copy_from_slice(&self.buf[CHUNK - RESEED..]);
            self.c = 0;
        }
        block(&self.seed, &mut self.buf, self.c);
        self.i = 0;
        self.n = if self.c == CTR_MAX - CTR_INC { CHUNK - RESEED } else { CHUNK };
    }

    fn next_u64(&mut self) -> u64 {
        if self.i >= self.n {
            self.refill();
        }
        self.i += 1;
        self.buf[self.i - 1]
    }
}

/// The first n bytes of the pattern, as a reader.
pub struct Pattern {
    rng: ChaCha8,
    left: u64,
    word: [u8; 8],
    /// Unread bytes at the end of word.
    have: usize,
}

/// n deterministic pseudo-random bytes (none for n <= 0).
pub fn pattern(n: i64) -> Pattern {
    Pattern { rng: ChaCha8::new(SEED), left: n.max(0) as u64, word: [0; 8], have: 0 }
}

impl Read for Pattern {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        let want = (p.len() as u64).min(self.left) as usize;
        let mut k = 0;
        while k < want {
            if self.have == 0 {
                self.word = self.rng.next_u64().to_le_bytes();
                self.have = 8;
            }
            let m = (want - k).min(self.have);
            p[k..k + m].copy_from_slice(&self.word[8 - self.have..8 - self.have + m]);
            self.have -= m;
            k += m;
        }
        self.left -= k as u64;
        Ok(k)
    }
}

/// Why a stream was not the pattern; the messages are Go's.
#[derive(Debug)]
pub enum PatternError {
    Read(u64, io::Error),
    Short(u64, i64),
    Long(i64),
    Byte(u64, u8, u8),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::Read(off, e) => write!(f, "pattern: read at {off}: {e}"),
            PatternError::Short(off, n) => write!(f, "pattern: short: {off} of {n} bytes"),
            PatternError::Long(n) => write!(f, "pattern: more than {n} bytes"),
            PatternError::Byte(off, g, w) => write!(f, "pattern: byte {off} is {g:#x}, want {w:#x}"),
        }
    }
}

impl std::error::Error for PatternError {}

/// Checks a stream fed in pieces against pattern(n).
pub struct Checker {
    want: Pattern,
    n: i64,
    off: u64,
    buf: Vec<u8>,
    err: Option<PatternError>,
}

impl Checker {
    pub fn new(n: i64) -> Checker {
        Checker { want: pattern(n), n, off: 0, buf: Vec::new(), err: None }
    }

    /// Feeds the next bytes; the first mismatch sticks.
    pub fn feed(&mut self, got: &[u8]) -> Result<(), &PatternError> {
        if self.err.is_none() {
            self.buf.resize(got.len(), 0);
            let k = self.want.read(&mut self.buf).expect("pattern reads never fail");
            if let Some(i) = (0..k).find(|&i| got[i] != self.buf[i]) {
                self.err = Some(PatternError::Byte(self.off + i as u64, got[i], self.buf[i]));
            } else if k < got.len() {
                self.err = Some(PatternError::Long(self.n));
            } else {
                self.off += k as u64;
            }
        }
        match &self.err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The stream ended: Ok if it was the whole pattern.
    pub fn finish(mut self) -> Result<(), PatternError> {
        if let Some(e) = self.err.take() {
            return Err(e);
        }
        if self.want.left > 0 {
            return Err(PatternError::Short(self.off, self.n));
        }
        Ok(())
    }

    /// The stream failed at the current offset.
    pub fn read_error(self, e: io::Error) -> PatternError {
        self.err.unwrap_or(PatternError::Read(self.off, e))
    }
}

/// Reads r to EOF and reports whether it was exactly pattern(n).
pub fn check_pattern(mut r: impl Read, n: i64) -> Result<(), PatternError> {
    let mut c = Checker::new(n);
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return c.finish(),
            Ok(k) => {
                if c.feed(&buf[..k]).is_err() {
                    return c.finish();
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(c.read_error(e)),
        }
    }
}
