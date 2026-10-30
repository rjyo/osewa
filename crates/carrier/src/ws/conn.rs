//! A WebSocket connection carrying whole binary messages, with the
//! behavior of coder/websocket's Conn as osewa uses it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{oneshot, Mutex, MutexGuard, Notify};

use super::deflate::{Deflater, InflateError, Inflater, TAIL};
use super::frame::{self, *};

/// Status codes (RFC 6455 7.4.1).
pub const STATUS_NORMAL: u16 = 1000;
pub const STATUS_GOING_AWAY: u16 = 1001;
pub const STATUS_PROTOCOL_ERROR: u16 = 1002;
pub const STATUS_UNSUPPORTED_DATA: u16 = 1003;
/// A close frame without a status; never sent as a code.
pub const STATUS_NO_STATUS: u16 = 1005;
pub const STATUS_INVALID_PAYLOAD: u16 = 1007;
pub const STATUS_POLICY_VIOLATION: u16 = 1008;
pub const STATUS_MESSAGE_TOO_BIG: u16 = 1009;
pub const STATUS_INTERNAL_ERROR: u16 = 1011;

/// How long a control frame's write, and the wait for the peer's close
/// frame, may take (coder/websocket's 5 s).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_CLOSE_REASON: usize = MAX_CONTROL_PAYLOAD - 2;

/// Keeps idle connections from holding on to a large message's buffers.
const KEEP_BUFFER: usize = 256 << 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: String,
}

#[derive(Debug)]
pub enum Error {
    /// The connection is closed, or a close frame has been sent.
    Closed,
    /// The peer sent a close frame (code 1005 when it carried none).
    Close(CloseFrame),
    /// A text message, which no osewa peer sends; answered with 1003.
    NotBinary,
    /// A message over the read limit; answered with 1009. Holds the limit
    /// plus one, as coder/websocket reports it.
    TooBig(usize),
    /// The peer broke the protocol.
    Protocol(String),
    /// The upgrade failed.
    Handshake(String),
    /// A control frame's write or the close handshake took too long.
    Timeout,
    Io(io::Error),
}

impl Error {
    /// The status of the peer's close frame, if that is what ended the
    /// connection (coder/websocket's CloseStatus).
    pub fn close_status(&self) -> Option<u16> {
        match self {
            Error::Close(c) => Some(c.code),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Closed => f.write_str("websocket: closed"),
            Error::Close(c) => write!(f, "received close frame: status = {} and reason = {:?}", c.code, c.reason),
            Error::NotBinary => f.write_str("osewa: text message"),
            Error::TooBig(n) => write!(f, "read limited at {n} bytes"),
            Error::Protocol(s) => f.write_str(s),
            Error::Handshake(s) => write!(f, "websocket handshake: {s}"),
            Error::Timeout => f.write_str("websocket: timed out"),
            Error::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

/// The negotiated permessage-deflate parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Deflate {
    pub client_no_context_takeover: bool,
    pub server_no_context_takeover: bool,
}

/// One WebSocket connection. Clones share it: one task may read while
/// others write, ping or close. Every method is cancel-safe in that
/// dropping its future leaves the connection usable, though a dropped
/// write may still go out ahead of the next one.
pub struct Conn<S> {
    inner: Rc<Inner<S>>,
}

impl<S> fmt::Debug for Conn<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("client", &self.inner.client)
            .field("subprotocol", &self.inner.subprotocol)
            .field("deflate", &self.inner.deflate)
            .field("closed", &self.inner.closed.get())
            .finish()
    }
}

impl<S> Clone for Conn<S> {
    fn clone(&self) -> Self {
        Conn { inner: self.inner.clone() }
    }
}

struct Inner<S> {
    client: bool,
    subprotocol: String,
    deflate: Option<Deflate>,
    /// Messages at least this long are compressed (deflate negotiated and
    /// we compress at all).
    threshold: Option<usize>,
    read_limit: Cell<usize>,

    rd: Mutex<Reader<S>>,
    wr: Mutex<Writer<S>>,

    /// A close has started: ours, the peer's, or close_now.
    closing: Cell<bool>,
    /// The stream is gone; closed_notify woke every waiter.
    closed: Cell<bool>,
    closed_notify: Notify,
    close_sent: Cell<bool>,
    close_received: RefCell<Option<CloseFrame>>,

    pings: RefCell<HashMap<Vec<u8>, oneshot::Sender<()>>>,
    ping_count: Cell<u64>,
}

struct Reader<S> {
    io: Option<ReadHalf<S>>,
    buf: Vec<u8>,
    pos: usize,
    /// Bytes the frame at pos needs.
    need: usize,
    inflate: Option<Inflater>,
    reset_inflate: bool,
    msg: Option<Partial>,
}

/// A data message in fragments.
struct Partial {
    opcode: u8,
    compressed: bool,
    /// Payload bytes as they came on the wire.
    wire: usize,
    data: Vec<u8>,
}

struct Writer<S> {
    io: Option<WriteHalf<S>>,
    /// Encoded frames not yet written; a write cancelled midway leaves its
    /// rest here for the next.
    out: Vec<u8>,
    sent: usize,
    deflate: Option<Deflater>,
    reset_deflate: bool,
    body: Vec<u8>,
}

enum Event {
    Message(u8, Vec<u8>),
    Control(u8, Vec<u8>),
    /// Need more bytes.
    More,
    /// A fragment went into the message.
    Fragment,
}

/// A protocol violation: the close status to send (None: just drop the
/// connection, as coder/websocket does for an unmasked client frame) and
/// the error to return.
struct Violation(Option<u16>, Error);

fn protocol(code: u16, msg: String) -> Violation {
    Violation(Some(code), Error::Protocol(msg))
}

fn valid_wire_code(code: u16) -> bool {
    match code {
        1004 | 1005 | 1006 | 1015 => false,
        1000..=1014 | 3000..=4999 => true,
        _ => false,
    }
}

fn close_payload(code: u16, reason: &str) -> Result<Vec<u8>, Error> {
    if code == STATUS_NO_STATUS {
        return Ok(Vec::new());
    }
    if reason.len() > MAX_CLOSE_REASON {
        return Err(Error::Protocol(format!("reason string max is {MAX_CLOSE_REASON} but got {}", reason.len())));
    }
    if !valid_wire_code(code) {
        return Err(Error::Protocol(format!("status code {code} cannot be set")));
    }
    let mut p = code.to_be_bytes().to_vec();
    p.extend_from_slice(reason.as_bytes());
    Ok(p)
}

fn parse_close(p: &[u8]) -> Result<CloseFrame, String> {
    if p.is_empty() {
        return Ok(CloseFrame { code: STATUS_NO_STATUS, reason: String::new() });
    }
    if p.len() < 2 {
        return Err(format!("close payload {p:?} too small, cannot even contain the 2 byte status code"));
    }
    let code = u16::from_be_bytes([p[0], p[1]]);
    if !valid_wire_code(code) {
        return Err(format!("invalid status code {code}"));
    }
    Ok(CloseFrame { code, reason: String::from_utf8_lossy(&p[2..]).into_owned() })
}

/// Cuts s to fit a close frame, on a character boundary.
fn fit_reason(s: &str) -> &str {
    if s.len() <= MAX_CLOSE_REASON {
        return s;
    }
    let mut n = MAX_CLOSE_REASON;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

impl<S: AsyncRead + AsyncWrite> Conn<S> {
    /// Wraps an upgraded stream. leftover is what was read past the
    /// handshake.
    pub(crate) fn new(
        stream: S,
        client: bool,
        subprotocol: String,
        deflate: Option<Deflate>,
        leftover: Vec<u8>,
        read_limit: usize,
    ) -> Conn<S> {
        let (r, w) = tokio::io::split(stream);
        // osewa clients never compress (PROTOCOL.md "Auth"); servers do
        // whenever the client accepts it.
        let (threshold, deflater, reset_deflate) = match deflate {
            Some(d) if !client => {
                // coder/websocket's defaults: 128 bytes when the context
                // carries over, 512 when every message starts afresh.
                let t = if d.server_no_context_takeover { 512 } else { 128 };
                (Some(t), Some(Deflater::new()), d.server_no_context_takeover)
            }
            _ => (None, None, false),
        };
        let reset_inflate = match deflate {
            Some(d) if client => d.server_no_context_takeover,
            Some(d) => d.client_no_context_takeover,
            None => false,
        };
        Conn {
            inner: Rc::new(Inner {
                client,
                subprotocol,
                deflate,
                threshold,
                read_limit: Cell::new(read_limit),
                rd: Mutex::new(Reader {
                    io: Some(r),
                    buf: leftover,
                    pos: 0,
                    need: 0,
                    inflate: deflate.map(|_| Inflater::new()),
                    reset_inflate,
                    msg: None,
                }),
                wr: Mutex::new(Writer {
                    io: Some(w),
                    out: Vec::new(),
                    sent: 0,
                    deflate: deflater,
                    reset_deflate,
                    body: Vec::new(),
                }),
                closing: Cell::new(false),
                closed: Cell::new(false),
                closed_notify: Notify::new(),
                close_sent: Cell::new(false),
                close_received: RefCell::new(None),
                pings: RefCell::new(HashMap::new()),
                ping_count: Cell::new(0),
            }),
        }
    }

    /// The negotiated subprotocol ("" for none).
    pub fn subprotocol(&self) -> &str {
        &self.inner.subprotocol
    }

    /// Whether permessage-deflate was negotiated.
    pub fn compressed(&self) -> bool {
        self.inner.deflate.is_some()
    }

    /// Bounds a message read, after decompression (default
    /// osewa_wire::MAX_FRAME). A longer one is answered with 1009.
    pub fn set_read_limit(&self, n: usize) {
        self.inner.read_limit.set(n);
    }

    /// Reads one whole binary message. A text message gets the connection
    /// closed with 1003 and Error::NotBinary. Pings are answered and close
    /// frames honored while reading. Any error ends the connection.
    pub async fn read(&self) -> Result<Vec<u8>, Error> {
        let c = &*self.inner;
        let mut r = c.lock_rd().await?;
        let res = c.read_message(&mut r).await;
        let res = match res {
            Ok((OP_BINARY, b)) => Ok(b),
            Ok(_) => {
                if !c.closing.replace(true) {
                    let _ = c.close_handshake(&mut r, STATUS_UNSUPPORTED_DATA, "binary frames only").await;
                    c.release_rd(&mut r);
                    drop(r);
                    c.finish_close().await;
                    return Err(Error::NotBinary);
                }
                Err(Error::NotBinary)
            }
            // A close frame was handled where it came in.
            Err(e @ Error::Close(_)) => Err(e),
            Err(e) => {
                c.shutdown();
                Err(e)
            }
        };
        c.release_rd(&mut r);
        res
    }

    /// Sends one binary message: compressed when deflate was negotiated, this
    /// is the server, and it is at least the threshold.
    pub async fn write(&self, msg: &[u8]) -> Result<(), Error> {
        self.inner.send(OP_BINARY, msg).await
    }

    /// Pings the peer and waits for its pong. The pong is only seen while
    /// some task is reading.
    pub async fn ping(&self) -> Result<(), Error> {
        let c = &*self.inner;
        let n = c.ping_count.get() + 1;
        c.ping_count.set(n);
        let key = n.to_string().into_bytes();
        let (tx, rx) = oneshot::channel();
        c.pings.borrow_mut().insert(key.clone(), tx);
        struct Forget<'a>(&'a RefCell<HashMap<Vec<u8>, oneshot::Sender<()>>>, Vec<u8>);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().remove(&self.1);
            }
        }
        let _forget = Forget(&c.pings, key.clone());
        c.send_control(OP_PING, &key).await?;
        match c.until_closed(rx).await? {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::Closed),
        }
    }

    /// Closes with a status and reason: sends the close frame, waits up to
    /// 5 s for the peer's, then closes the stream. Err(Closed) if a close
    /// had already begun.
    pub async fn close(&self, code: u16, reason: &str) -> Result<(), Error> {
        let c = &*self.inner;
        if c.closing.replace(true) {
            return Err(Error::Closed);
        }
        let res = async {
            c.send_close(code, reason).await?;
            tokio::time::timeout(CONTROL_TIMEOUT, async {
                let mut r = c.lock_rd().await?;
                let res = c.await_close(&mut r).await;
                c.release_rd(&mut r);
                res
            })
            .await
            .unwrap_or(Err(Error::Timeout))
        }
        .await;
        c.finish_close().await;
        res
    }

    /// Closes the stream at once, without a close frame. Blocked reads,
    /// writes and pings return Error::Closed.
    pub fn close_now(&self) -> Result<(), Error> {
        let c = &*self.inner;
        let was = c.closing.replace(true);
        c.shutdown();
        if was {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
}

impl<S: AsyncRead + AsyncWrite> Inner<S> {
    /// Runs f unless and until the connection closes.
    async fn until_closed<F: Future>(&self, f: F) -> Result<F::Output, Error> {
        let closed = self.closed_notify.notified();
        if self.closed.get() {
            return Err(Error::Closed);
        }
        tokio::select! {
            biased;
            _ = closed => Err(Error::Closed),
            v = f => Ok(v),
        }
    }

    async fn lock_rd(&self) -> Result<MutexGuard<'_, Reader<S>>, Error> {
        self.until_closed(self.rd.lock()).await
    }

    async fn lock_wr(&self) -> Result<MutexGuard<'_, Writer<S>>, Error> {
        self.until_closed(self.wr.lock()).await
    }

    /// Drops the read half if the connection closed while we held it.
    fn release_rd(&self, r: &mut Reader<S>) {
        if self.closed.get() {
            r.io = None;
        } else if r.pos == r.buf.len() && r.buf.capacity() > KEEP_BUFFER {
            r.buf = Vec::new();
            r.pos = 0;
        }
    }

    fn release_wr(&self, w: &mut Writer<S>) {
        if self.closed.get() {
            w.io = None;
        }
        if w.out.is_empty() && w.out.capacity() > KEEP_BUFFER {
            w.out = Vec::new();
        }
        if w.body.capacity() > KEEP_BUFFER {
            w.body = Vec::new();
        }
    }

    /// Ends the connection: wakes every waiter and drops the stream (each
    /// half now, or as its holder lets go).
    fn shutdown(&self) {
        self.closing.set(true);
        if self.closed.replace(true) {
            return;
        }
        self.closed_notify.notify_waiters();
        self.pings.borrow_mut().clear();
        if let Ok(mut r) = self.rd.try_lock() {
            r.io = None;
        }
        if let Ok(mut w) = self.wr.try_lock() {
            w.io = None;
        }
    }

    /// Shuts the write side down cleanly (TLS close_notify, then FIN), then
    /// the rest.
    async fn finish_close(&self) {
        let _ = tokio::time::timeout(CONTROL_TIMEOUT, async {
            if let Ok(mut w) = self.lock_wr().await {
                if let Some(io) = w.io.as_mut() {
                    let _ = self.until_closed(io.shutdown()).await;
                }
            }
        })
        .await;
        self.shutdown();
    }

    async fn read_message(&self, r: &mut Reader<S>) -> Result<(u8, Vec<u8>), Error> {
        loop {
            if self.closed.get() {
                return Err(Error::Closed);
            }
            if let Some(ce) = self.close_received.borrow().clone() {
                return Err(Error::Close(ce));
            }
            match self.next_frame(r) {
                Ok(Event::Message(op, b)) => return Ok((op, b)),
                Ok(Event::Fragment) => {}
                Ok(Event::More) => self.fill(r).await?,
                Ok(Event::Control(op, p)) => self.control(op, p).await?,
                Err(Violation(code, err)) => {
                    if let Some(code) = code {
                        let _ = self.send_close(code, fit_reason(&err.to_string())).await;
                    }
                    return Err(err);
                }
            }
        }
    }

    async fn fill(&self, r: &mut Reader<S>) -> Result<(), Error> {
        if r.pos > 0 {
            r.buf.drain(..r.pos);
            r.pos = 0;
        }
        let want = r.need.saturating_sub(r.buf.len()).max(16 << 10);
        r.buf.reserve(want);
        let Reader { io, buf, .. } = r;
        let io = io.as_mut().ok_or(Error::Closed)?;
        let n = self.until_closed(io.read_buf(buf)).await??;
        if n == 0 {
            return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        Ok(())
    }

    /// Takes the next frame off the buffer, checking it as coder/websocket
    /// does, and adds a data frame to the message.
    fn next_frame(&self, r: &mut Reader<S>) -> Result<Event, Violation> {
        let limit = self.read_limit.get();
        let avail = &r.buf[r.pos..];
        let (h, hlen) = match frame::parse_header(avail) {
            Ok(Some(v)) => v,
            Ok(None) => {
                r.need = 14;
                return Ok(Event::More);
            }
            Err(e) => {
                return Err(Violation(None, Error::Protocol(format!("received negative payload length: {}", e.0 as i64))))
            }
        };
        let data = matches!(h.opcode, OP_CONTINUATION | OP_TEXT | OP_BINARY);
        if h.rsv1 && (self.deflate.is_none() || !matches!(h.opcode, OP_TEXT | OP_BINARY)) || h.rsv2 || h.rsv3 {
            return Err(protocol(
                STATUS_PROTOCOL_ERROR,
                format!("received header with unexpected rsv bits set: {}:{}:{}", h.rsv1, h.rsv2, h.rsv3),
            ));
        }
        if !self.client && h.mask.is_none() {
            return Err(Violation(None, Error::Protocol("received unmasked frame from client".into())));
        }
        if self.client && h.mask.is_some() {
            return Err(protocol(STATUS_PROTOCOL_ERROR, "received masked frame from server".into()));
        }
        match h.opcode {
            OP_CLOSE | OP_PING | OP_PONG => {
                if h.len > MAX_CONTROL_PAYLOAD as u64 {
                    return Err(protocol(
                        STATUS_PROTOCOL_ERROR,
                        format!("received control frame payload with invalid length: {}", h.len),
                    ));
                }
                if !h.fin {
                    return Err(protocol(STATUS_PROTOCOL_ERROR, "received fragmented control frame".into()));
                }
            }
            OP_CONTINUATION if r.msg.is_none() => {
                return Err(protocol(
                    STATUS_PROTOCOL_ERROR,
                    "received continuation frame without text or binary frame".into(),
                ));
            }
            OP_TEXT | OP_BINARY if r.msg.is_some() => {
                return Err(protocol(
                    STATUS_PROTOCOL_ERROR,
                    "received new data message without finishing the previous message".into(),
                ));
            }
            OP_CONTINUATION | OP_TEXT | OP_BINARY => {}
            op => return Err(protocol(STATUS_PROTOCOL_ERROR, format!("received unknown opcode {op}"))),
        }
        if data {
            let (compressed, wire) = match &r.msg {
                Some(m) => (m.compressed, m.wire as u64),
                None => (h.rsv1, 0),
            };
            // Plain bytes count against the limit as they are; compressed
            // ones are checked as they inflate, with the wire size bounded
            // by deflate's expansion at worst (fixed Huffman, 9 bits a
            // byte), so a frame is never buffered whole before it can be
            // refused.
            let max = if compressed { limit as u64 + limit as u64 / 8 + 1024 } else { limit as u64 };
            if wire + h.len > max {
                return Err(too_big(limit));
            }
        }
        let total = hlen + h.len as usize;
        if avail.len() < total {
            r.need = total;
            return Ok(Event::More);
        }
        let start = r.pos + hlen;
        r.pos += total;
        r.need = 0;
        let Reader { buf, inflate, reset_inflate, msg, .. } = r;
        let payload = &mut buf[start..start + h.len as usize];
        if let Some(k) = h.mask {
            frame::mask(payload, k);
        }
        if !data {
            return Ok(Event::Control(h.opcode, payload.to_vec()));
        }
        let m = msg.get_or_insert_with(|| Partial { opcode: h.opcode, compressed: h.rsv1, wire: 0, data: Vec::new() });
        m.wire += payload.len();
        if m.compressed {
            let inf = inflate.as_mut().expect("rsv1 only with deflate");
            inf.inflate(payload, &mut m.data, limit).map_err(|e| inflate_violation(e, limit))?;
            if h.fin {
                inf.inflate(&TAIL, &mut m.data, limit).map_err(|e| inflate_violation(e, limit))?;
                if *reset_inflate {
                    inf.reset();
                }
            }
        } else {
            m.data.extend_from_slice(payload);
        }
        if !h.fin {
            return Ok(Event::Fragment);
        }
        let m = msg.take().unwrap();
        Ok(Event::Message(m.opcode, m.data))
    }

    async fn control(&self, op: u8, p: Vec<u8>) -> Result<(), Error> {
        match op {
            OP_PING => self.send_control(OP_PONG, &p).await,
            OP_PONG => {
                if let Some(tx) = self.pings.borrow_mut().remove(&p) {
                    let _ = tx.send(());
                }
                Ok(())
            }
            _ => {
                let ce = match parse_close(&p) {
                    Ok(ce) => ce,
                    Err(e) => {
                        let e = format!("received invalid close payload: {e}");
                        let _ = self.send_close(STATUS_PROTOCOL_ERROR, fit_reason(&e)).await;
                        return Err(Error::Protocol(e));
                    }
                };
                *self.close_received.borrow_mut() = Some(ce.clone());
                if !self.close_sent.get() {
                    let _ = self.send_close(ce.code, &ce.reason).await;
                }
                // The peer began it: we're done. Ours: close() finishes.
                if !self.closing.replace(true) {
                    self.finish_close().await;
                }
                Err(Error::Close(ce))
            }
        }
    }

    /// Sends our close frame and reads until the peer's, discarding data
    /// (coder/websocket's closeHandshake on a reader it holds).
    async fn close_handshake(&self, r: &mut Reader<S>, code: u16, reason: &str) -> Result<(), Error> {
        self.send_close(code, reason).await?;
        tokio::time::timeout(CONTROL_TIMEOUT, self.await_close(r)).await.unwrap_or(Err(Error::Timeout))
    }

    async fn await_close(&self, r: &mut Reader<S>) -> Result<(), Error> {
        loop {
            match self.read_message(r).await {
                Ok(_) => {}
                Err(Error::Close(_)) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    async fn send_close(&self, code: u16, reason: &str) -> Result<(), Error> {
        let p = close_payload(code, reason)?;
        self.send_control(OP_CLOSE, &p).await
    }

    async fn send_control(&self, op: u8, p: &[u8]) -> Result<(), Error> {
        match tokio::time::timeout(CONTROL_TIMEOUT, self.send(op, p)).await {
            Ok(r) => r,
            Err(_) => {
                self.shutdown();
                Err(Error::Timeout)
            }
        }
    }

    async fn send(&self, op: u8, p: &[u8]) -> Result<(), Error> {
        let mut w = self.lock_wr().await?;
        let res = self.send_locked(&mut w, op, p).await;
        if let Err(Error::Io(_)) = res {
            self.shutdown();
        }
        self.release_wr(&mut w);
        res
    }

    async fn send_locked(&self, w: &mut Writer<S>, op: u8, p: &[u8]) -> Result<(), Error> {
        if self.close_sent.get() || self.closed.get() {
            return Err(Error::Closed);
        }
        let Writer { out, deflate, reset_deflate, body, .. } = w;
        let compress = op == OP_BINARY && self.threshold.is_some_and(|t| p.len() >= t);
        let payload = if compress {
            let d = deflate.as_mut().expect("threshold only with a deflater");
            body.clear();
            d.compress(p, body);
            if *reset_deflate {
                d.reset();
            }
            &body[..]
        } else {
            p
        };
        let mask = self.client.then(rand::random::<[u8; 4]>);
        out.reserve(14 + payload.len());
        frame::put_header(out, compress, op, payload.len(), mask);
        let at = out.len();
        out.extend_from_slice(payload);
        if let Some(k) = mask {
            frame::mask(&mut out[at..], k);
        }
        if op == OP_CLOSE {
            self.close_sent.set(true);
        }
        self.flush(w).await
    }

    async fn flush(&self, w: &mut Writer<S>) -> Result<(), Error> {
        let Writer { io, out, sent, .. } = w;
        let io = io.as_mut().ok_or(Error::Closed)?;
        while *sent < out.len() {
            let n = self.until_closed(io.write(&out[*sent..])).await??;
            if n == 0 {
                return Err(Error::Io(io::ErrorKind::WriteZero.into()));
            }
            *sent += n;
        }
        out.clear();
        *sent = 0;
        self.until_closed(io.flush()).await??;
        Ok(())
    }
}

fn too_big(limit: usize) -> Violation {
    Violation(Some(STATUS_MESSAGE_TOO_BIG), Error::TooBig(limit + 1))
}

fn inflate_violation(e: InflateError, limit: usize) -> Violation {
    match e {
        InflateError::TooBig => too_big(limit),
        InflateError::Corrupt(s) => protocol(STATUS_INVALID_PAYLOAD, format!("failed to inflate: {s}")),
    }
}
