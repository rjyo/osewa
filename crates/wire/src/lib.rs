//! osewa's frames and messages (docs/PROTOCOL.md). The holder, the server and
//! clients all use this crate; nothing else defines wire types.

use std::fmt;
use std::io::{self, Read, Write};

mod messages;
pub use messages::*;

/// The protocol version carried in HELLO and WELCOME.
pub const VERSION: i32 = 0;

/// The WebSocket subprotocol, and its URL path.
pub const SUBPROTOCOL: &str = "osewa.v0";
pub const PATH: &str = "/osewa";

// Client to server.
pub const TYPE_HELLO: u8 = 0x01;
pub const TYPE_REQUEST: u8 = 0x02;
pub const TYPE_INPUT: u8 = 0x03;
pub const TYPE_ACK: u8 = 0x04;
pub const TYPE_CREDIT: u8 = 0x05;
/// Bytes to the forwarded connection or stdin (features "forward", "exec").
pub const TYPE_DATA: u8 = 0x06;
/// No more DATA from the client.
pub const TYPE_EOF: u8 = 0x07;

// Server to client.
pub const TYPE_WELCOME: u8 = 0x81;
pub const TYPE_RESPONSE: u8 = 0x82;
pub const TYPE_INPUT_ACK: u8 = 0x83;
/// Credit for the client's DATA.
pub const TYPE_GRANT: u8 = 0x84;
pub const TYPE_SNAPSHOT_BEGIN: u8 = 0xA0;
pub const TYPE_SNAPSHOT_DATA: u8 = 0xA1;
pub const TYPE_SNAPSHOT_READY: u8 = 0xA2;
pub const TYPE_SNAPSHOT_HISTORY: u8 = 0xA3;
pub const TYPE_SNAPSHOT_END: u8 = 0xA4;
pub const TYPE_OUTPUT: u8 = 0xA5;
pub const TYPE_EVENT: u8 = 0xA6;
pub const TYPE_STREAM_END: u8 = 0xA7;
/// Channel u8, then bytes.
pub const TYPE_CHANNEL_DATA: u8 = 0xA8;
/// Channel u8: that channel has ended.
pub const TYPE_CHANNEL_EOF: u8 = 0xA9;

/// Channels of a channel stream's server-to-client data. A forward uses
/// stdout for the connection's bytes.
pub const CHANNEL_STDOUT: u8 = 1;
pub const CHANNEL_STDERR: u8 = 2;

/// Bounds the bytes in one DATA or CHANNEL_DATA frame, so a terminal frame
/// never queues behind more than one of them.
pub const MAX_CHANNEL_DATA: usize = 32 << 10;

/// The initial credit each way on a channel stream. A stream carries at most
/// one window per round trip, so it is sized for fast links far away (4 MiB
/// is 34 MB/s at 120 ms); the in-flight window, not credit, keeps queues
/// short.
pub const CHANNEL_WINDOW: u32 = 4 << 20;

/// Bounds one frame, header included.
pub const MAX_FRAME: usize = 1 << 20;

/// libghostty allocates every cell, so terminal size bounds memory per
/// session.
pub const MAX_COLS: i64 = 2000;
pub const MAX_ROWS: i64 = 1000;
/// Streams per client connection.
pub const MAX_STREAMS: usize = 64;

/// The type byte plus the stream id.
pub const HEADER_SIZE: usize = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Short,
    TooLarge,
    UnknownType(u8),
    Json(u8, String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Short => f.write_str("wire: frame too short"),
            Error::TooLarge => f.write_str("wire: frame too large"),
            Error::UnknownType(t) => write!(f, "wire: unknown frame type 0x{t:02x}"),
            Error::Json(t, e) => write!(f, "wire: frame 0x{t:02x}: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// Reports whether t is a frame type this version defines.
pub fn known(t: u8) -> bool {
    matches!(
        t,
        TYPE_HELLO
            | TYPE_REQUEST
            | TYPE_INPUT
            | TYPE_ACK
            | TYPE_CREDIT
            | TYPE_DATA
            | TYPE_EOF
            | TYPE_WELCOME
            | TYPE_RESPONSE
            | TYPE_INPUT_ACK
            | TYPE_GRANT
            | TYPE_SNAPSHOT_BEGIN
            | TYPE_SNAPSHOT_DATA
            | TYPE_SNAPSHOT_READY
            | TYPE_SNAPSHOT_HISTORY
            | TYPE_SNAPSHOT_END
            | TYPE_OUTPUT
            | TYPE_EVENT
            | TYPE_STREAM_END
            | TYPE_CHANNEL_DATA
            | TYPE_CHANNEL_EOF
    )
}

/// One protocol frame.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Frame {
    pub ty: u8,
    pub stream: u32,
    pub payload: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame(0x{:02x}, {}, {} bytes)", self.ty, self.stream, self.payload.len())
    }
}

impl Frame {
    pub fn new(ty: u8, stream: u32, payload: Vec<u8>) -> Frame {
        Frame { ty, stream, payload }
    }

    /// Encodes the frame as one WebSocket message.
    pub fn marshal(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        b.push(self.ty);
        b.extend_from_slice(&self.stream.to_le_bytes());
        b.extend_from_slice(&self.payload);
        b
    }

    /// Decodes one WebSocket message.
    pub fn unmarshal(b: &[u8]) -> Result<Frame, Error> {
        if b.len() < HEADER_SIZE {
            return Err(Error::Short);
        }
        if b.len() > MAX_FRAME {
            return Err(Error::TooLarge);
        }
        if !known(b[0]) {
            return Err(Error::UnknownType(b[0]));
        }
        Ok(Frame { ty: b[0], stream: u32::from_le_bytes(b[1..5].try_into().unwrap()), payload: b[5..].to_vec() })
    }

    /// Unmarshals a JSON payload.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, Error> {
        decode_json(&self.payload).map_err(|e| Error::Json(self.ty, e))
    }

    /// The length-prefixed encoding of the holder socket.
    pub fn encode_to(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let n = HEADER_SIZE + self.payload.len();
        if n > MAX_FRAME {
            return Err(Error::TooLarge);
        }
        out.reserve(4 + n);
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.push(self.ty);
        out.extend_from_slice(&self.stream.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(())
    }
}

/// Writes a length-prefixed frame, the holder socket encoding.
pub fn write<W: Write>(w: &mut W, f: &Frame) -> io::Result<()> {
    let mut b = Vec::new();
    f.encode_to(&mut b)?;
    w.write_all(&b)
}

/// Reads one length-prefixed frame.
pub fn read<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut lb = [0u8; 4];
    r.read_exact(&mut lb)?;
    let n = u32::from_le_bytes(lb) as usize;
    check_len(n)?;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(Frame::unmarshal(&b)?)
}

/// Validates the length prefix of a holder socket frame.
pub fn check_len(n: usize) -> Result<(), Error> {
    if n < HEADER_SIZE {
        return Err(Error::Short);
    }
    if n > MAX_FRAME {
        return Err(Error::TooLarge);
    }
    Ok(())
}

/// Splits complete length-prefixed frames off the front of buf, for readers
/// that buffer a non-blocking socket. Returns how many bytes were consumed.
pub fn parse_frames(buf: &[u8], out: &mut Vec<Frame>) -> Result<usize, Error> {
    let mut off = 0;
    while buf.len() - off >= 4 {
        let n = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        check_len(n)?;
        if buf.len() - off - 4 < n {
            break;
        }
        out.push(Frame::unmarshal(&buf[off + 4..off + 4 + n])?);
        off += 4 + n;
    }
    Ok(off)
}

/// Builds a frame whose payload is v as JSON.
pub fn json<T: serde::Serialize>(ty: u8, stream: u32, v: &T) -> Frame {
    Frame { ty, stream, payload: marshal(v) }
}

/// Encodes v as Go's encoding/json does: serde_json's output with `<`, `>`,
/// `&`, U+2028 and U+2029 escaped. Those only ever occur inside strings, so
/// the substitution can't touch the JSON's structure. Payload bytes count
/// toward log positions, so the Go and Rust holders agree on them.
pub fn marshal<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let b = serde_json::to_vec(v).expect("wire: marshal");
    if !b.iter().any(|&c| matches!(c, b'<' | b'>' | b'&' | 0xe2)) {
        return b;
    }
    let mut out = Vec::with_capacity(b.len() + 16);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => out.extend_from_slice(b"\\u003c"),
            b'>' => out.extend_from_slice(b"\\u003e"),
            b'&' => out.extend_from_slice(b"\\u0026"),
            0xe2 if b.get(i + 1) == Some(&0x80) && matches!(b.get(i + 2), Some(0xa8 | 0xa9)) => {
                out.extend_from_slice(if b[i + 2] == 0xa8 { b"\\u2028" } else { b"\\u2029" });
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Quotes s as Go's %q does, for error messages a Go peer would word the
/// same way.
pub fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\x0b' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decodes JSON as leniently as Go's encoding/json, which Go peers rely on:
/// invalid UTF-8 becomes U+FFFD, `null` leaves a field at its zero value,
/// keys match case-insensitively (every osewa key is lowercase) and the
/// last of duplicate keys wins.
pub fn decode_json<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T, String> {
    let text = String::from_utf8_lossy(b);
    let mut v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    normalize(&mut v);
    serde_json::from_value(v).map_err(|e| e.to_string())
}

fn normalize(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            let old = std::mem::take(m);
            for (k, mut x) in old {
                if x.is_null() {
                    continue;
                }
                normalize(&mut x);
                let k = if k.bytes().any(|c| c.is_ascii_uppercase()) { k.to_ascii_lowercase() } else { k };
                m.insert(k, x);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(normalize),
        _ => {}
    }
}

/// How many log positions a record with this payload occupies.
pub fn record_size(payload_len: usize) -> u64 {
    5 + payload_len as u64
}

/// Builds an OUTPUT frame: end position, then the bytes.
pub fn output(stream: u32, end: u64, data: &[u8]) -> Frame {
    Frame { ty: TYPE_OUTPUT, stream, payload: with_end(end, data) }
}

/// Builds an EVENT frame from an already-encoded event JSON.
pub fn event(stream: u32, end: u64, event: &[u8]) -> Frame {
    Frame { ty: TYPE_EVENT, stream, payload: with_end(end, event) }
}

fn with_end(end: u64, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + data.len());
    b.extend_from_slice(&end.to_le_bytes());
    b.extend_from_slice(data);
    b
}

/// Parses the end position of an OUTPUT or EVENT payload.
pub fn split_end(payload: &[u8]) -> Result<(u64, &[u8]), Error> {
    if payload.len() < 8 {
        return Err(Error::Short);
    }
    Ok((u64::from_le_bytes(payload[..8].try_into().unwrap()), &payload[8..]))
}

/// Builds an ACK frame.
pub fn ack(stream: u32, generation: u32, end: u64) -> Frame {
    let mut b = Vec::with_capacity(12);
    b.extend_from_slice(&generation.to_le_bytes());
    b.extend_from_slice(&end.to_le_bytes());
    Frame { ty: TYPE_ACK, stream, payload: b }
}

/// Parses an ACK payload.
pub fn parse_ack(payload: &[u8]) -> Result<(u32, u64), Error> {
    if payload.len() < 12 {
        return Err(Error::Short);
    }
    Ok((u32::from_le_bytes(payload[..4].try_into().unwrap()), u64::from_le_bytes(payload[4..12].try_into().unwrap())))
}

/// Builds an INPUT_ACK frame.
pub fn input_ack(stream: u32, seq: u64) -> Frame {
    Frame { ty: TYPE_INPUT_ACK, stream, payload: seq.to_le_bytes().to_vec() }
}

/// Parses an INPUT_ACK payload.
pub fn parse_u64(payload: &[u8]) -> Result<u64, Error> {
    if payload.len() < 8 {
        return Err(Error::Short);
    }
    Ok(u64::from_le_bytes(payload[..8].try_into().unwrap()))
}

/// Builds a CREDIT frame.
pub fn credit(stream: u32, n: u32) -> Frame {
    Frame { ty: TYPE_CREDIT, stream, payload: n.to_le_bytes().to_vec() }
}

/// Parses a CREDIT or GRANT payload.
pub fn parse_credit(payload: &[u8]) -> Result<u32, Error> {
    if payload.len() < 4 {
        return Err(Error::Short);
    }
    Ok(u32::from_le_bytes(payload[..4].try_into().unwrap()))
}

/// Builds a GRANT frame: the server accepts n more DATA bytes.
pub fn grant(stream: u32, n: u32) -> Frame {
    Frame { ty: TYPE_GRANT, stream, payload: n.to_le_bytes().to_vec() }
}

/// Builds a CHANNEL_DATA frame.
pub fn channel_data(stream: u32, channel: u8, data: &[u8]) -> Frame {
    let mut p = Vec::with_capacity(1 + data.len());
    p.push(channel);
    p.extend_from_slice(data);
    Frame { ty: TYPE_CHANNEL_DATA, stream, payload: p }
}

/// Builds a CHANNEL_EOF frame.
pub fn channel_eof(stream: u32, channel: u8) -> Frame {
    Frame { ty: TYPE_CHANNEL_EOF, stream, payload: vec![channel] }
}

/// Splits a CHANNEL_DATA or CHANNEL_EOF payload.
pub fn split_channel(payload: &[u8]) -> Result<(u8, &[u8]), Error> {
    if payload.is_empty() {
        return Err(Error::Short);
    }
    Ok((payload[0], &payload[1..]))
}

#[cfg(test)]
mod tests;
