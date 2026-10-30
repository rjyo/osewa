//! Frames on QUIC streams: each after its length (u32, big-endian), a
//! zero length being a ping; on the server's half of the main stream under
//! ALPN_DEFLATE, one raw deflate stream sync-flushed after each frame, as
//! Go's flate.Writer.Flush leaves it (the 00 00 ff ff marker kept).

use std::io;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use osewa_wire as wire;
use quinn::{Connection, RecvStream, SendStream};

/// What a stream yields.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Frame(Vec<u8>),
    Ping,
}

/// How much a read from the stream, or one step of inflating, may add to
/// what's buffered: a deflate bomb can't grow it past a frame and this.
const STEP: usize = 64 << 10;

/// Reads frames from one stream.
pub(crate) struct Reader {
    s: RecvStream,
    /// Bytes read and, with inflate, decoded, from off on.
    buf: Vec<u8>,
    off: usize,
    inflate: Option<Inflate>,
}

struct Inflate {
    d: Decompress,
    /// Compressed bytes not yet inflated, from off on.
    raw: Vec<u8>,
    off: usize,
}

impl Reader {
    pub fn new(s: RecvStream, deflated: bool) -> Reader {
        let inflate = deflated.then(|| Inflate { d: Decompress::new(false), raw: Vec::new(), off: 0 });
        Reader { s, buf: Vec::new(), off: 0, inflate }
    }

    /// The next frame or ping. The stream's end is an UnexpectedEof error.
    pub async fn next(&mut self) -> io::Result<Item> {
        loop {
            if let Some(item) = self.parse()? {
                return Ok(item);
            }
            // Even with all input taken, the inflater may hold output that
            // didn't fit last time.
            if let Some(inf) = &mut self.inflate {
                if inf.step(&mut self.buf)? {
                    continue;
                }
            }
            let chunk = self.s.read_chunk(STEP, true).await.map_err(read_err)?;
            let Some(chunk) = chunk else { return Err(io::ErrorKind::UnexpectedEof.into()) };
            match &mut self.inflate {
                Some(inf) => {
                    inf.raw.drain(..inf.off);
                    inf.off = 0;
                    inf.raw.extend_from_slice(&chunk.bytes);
                }
                None => self.buf.extend_from_slice(&chunk.bytes),
            }
        }
    }

    /// Takes a whole frame or ping off the buffer, if there is one.
    fn parse(&mut self) -> io::Result<Option<Item>> {
        let avail = &self.buf[self.off..];
        if avail.len() < 4 {
            self.compact();
            return Ok(None);
        }
        let n = u32::from_be_bytes(avail[..4].try_into().unwrap()) as usize;
        if n == 0 {
            self.off += 4;
            return Ok(Some(Item::Ping));
        }
        if n > wire::MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("osewa: {n} byte frame")));
        }
        if avail.len() < 4 + n {
            self.compact();
            return Ok(None);
        }
        let f = avail[4..4 + n].to_vec();
        self.off += 4 + n;
        Ok(Some(Item::Frame(f)))
    }

    fn compact(&mut self) {
        if self.off > 0 {
            self.buf.drain(..self.off);
            self.off = 0;
        }
        // A megabyte frame's buffer isn't kept for the keystrokes after it.
        if self.buf.is_empty() && self.buf.capacity() > 4 * STEP {
            self.buf = Vec::new();
        }
    }
}

impl Inflate {
    /// Inflates what fits in out's spare room after reserving STEP (so at
    /// most its size again); false if it made no progress (it needs more
    /// input).
    fn step(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        let (in0, out0) = (self.d.total_in(), out.len());
        out.reserve(STEP);
        let st = self
            .d
            .decompress_vec(&self.raw[self.off..], out, FlushDecompress::None)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.off += (self.d.total_in() - in0) as usize;
        if st == Status::StreamEnd {
            // Go's writer never ends the stream; a final block is the end.
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(self.d.total_in() != in0 || out.len() != out0)
    }
}

pub(crate) fn read_err(e: quinn::ReadError) -> io::Error {
    match e {
        quinn::ReadError::ConnectionLost(c) => conn_err(c),
        e => e.into(),
    }
}

pub(crate) fn write_err(e: quinn::WriteError) -> io::Error {
    match e {
        quinn::WriteError::ConnectionLost(c) => conn_err(c),
        e => e.into(),
    }
}

/// The connection's end as an io::Error that keeps it (see close_status).
pub(crate) fn conn_err(c: quinn::ConnectionError) -> io::Error {
    let kind = match c {
        quinn::ConnectionError::TimedOut => io::ErrorKind::TimedOut,
        quinn::ConnectionError::Reset => io::ErrorKind::ConnectionReset,
        _ => io::ErrorKind::ConnectionAborted,
    };
    io::Error::new(kind, c)
}

/// What quinn may hold written but not yet sent, connection-wide, past
/// what the congestion window lets go at once: TCP_NOTSENT_LOWAT's 16 KiB,
/// as the WebSocket's socket has. quic-go's writes return with about a
/// packet unsent; quinn's would return with up to its whole send window
/// (all of it unacknowledged data) buffered, where neither the server's
/// send queue nor its priorities can reach it.
pub(crate) const LOWAT: u64 = 16 << 10;

/// Bounds quinn's buffer to what is in flight or may go now plus LOWAT.
/// quinn counts written-but-unacknowledged bytes against its send window
/// and exposes neither bytes in flight nor a stream's unsent bytes, so the
/// window follows the congestion window instead: in flight is at most
/// cwnd, so unsent is at most LOWAT more than cwnd lets out at once. It is
/// refreshed before every write to quinn, so it follows cwnd as acks come.
pub(crate) fn bound_unsent(qc: &Connection) {
    qc.set_send_window(qc.stats().path.cwnd + LOWAT);
}

/// One direction of a stream: frames appended to a buffer that writes
/// drain, so a write cancelled midway leaves the rest for the next and
/// the stream never carries half a frame followed by another.
pub(crate) struct Writer {
    pub s: SendStream,
    qc: Connection,
    buf: Vec<u8>,
    off: usize,
    deflate: Option<Compress>,
}

impl Writer {
    pub fn new(qc: &Connection, s: SendStream, deflate: bool) -> Writer {
        // Level 2, as the WebSocket's deflater (ws/deflate.rs): Go's
        // BestSpeed ratio at about its speed.
        let deflate = deflate.then(|| Compress::new(Compression::new(2), false));
        Writer { s, qc: qc.clone(), buf: Vec::new(), off: 0, deflate }
    }

    /// Writes frame (empty: a ping) and whatever an earlier cancelled write
    /// left.
    pub async fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        self.push(frame);
        self.flush().await
    }

    /// Appends frame after its length, compressed and flushed if deflating.
    fn push(&mut self, frame: &[u8]) {
        let len = (frame.len() as u32).to_be_bytes();
        match &mut self.deflate {
            None => {
                self.buf.extend_from_slice(&len);
                self.buf.extend_from_slice(frame);
            }
            Some(c) => {
                compress(c, &len, &mut self.buf, FlushCompress::None);
                compress(c, frame, &mut self.buf, FlushCompress::Sync);
            }
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        while self.off < self.buf.len() {
            bound_unsent(&self.qc);
            let n = self.s.write(&self.buf[self.off..]).await.map_err(write_err)?;
            self.off += n;
        }
        self.buf.clear();
        self.off = 0;
        if self.buf.capacity() > 4 * STEP {
            self.buf = Vec::new();
        }
        Ok(())
    }
}

/// Appends p compressed to out; a Sync flush ends with the 00 00 ff ff
/// marker, so everything so far can be inflated.
fn compress(c: &mut Compress, p: &[u8], out: &mut Vec<u8>, flush: FlushCompress) {
    let in0 = c.total_in();
    loop {
        let done = (c.total_in() - in0) as usize;
        if out.capacity() - out.len() < 64 {
            out.reserve((p.len() - done) / 2 + 1024);
        }
        // Raw deflate into a Vec with room to spare can't fail.
        c.compress_vec(&p[done..], out, flush).expect("deflate");
        if (c.total_in() - in0) as usize == p.len() && out.len() < out.capacity() {
            break;
        }
    }
}
