use std::time::Duration;

use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::{spawn_local, JoinHandle, LocalSet};

use super::conn::Deflate;
use super::deflate::{Deflater, Inflater, TAIL};
use super::frame::{self, Header, OP_BINARY, OP_CLOSE, OP_CONTINUATION, OP_PING, OP_PONG, OP_TEXT};
use super::handshake::{self, accept_key, dial_offer, select_deflate, Offer};
use super::*;
use osewa_wire::MAX_FRAME;

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

async fn local<F: std::future::Future>(f: F) -> F::Output {
    LocalSet::new().run_until(f).await
}

fn upgrade(extra: &str) -> String {
    format!(
        "GET /osewa HTTP/1.1\r\nHost: h:1\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {KEY}\r\n{extra}\r\n"
    )
}

const OSEWA: &str = "Sec-WebSocket-Protocol: osewa.v0\r\n";
const DEFLATE: &str = "Sec-WebSocket-Extensions: permessage-deflate\r\n";

/// A peer that speaks raw frames.
struct Raw {
    s: DuplexStream,
    buf: Vec<u8>,
    /// Mask what we send (we are the client).
    client: bool,
}

impl Raw {
    async fn more(&mut self) -> bool {
        let mut b = [0u8; 8192];
        let n = self.s.read(&mut b).await.unwrap_or(0);
        self.buf.extend_from_slice(&b[..n]);
        n > 0
    }

    /// Reads an HTTP head, returning it.
    async fn head(&mut self) -> String {
        loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let h = String::from_utf8(self.buf[..i + 4].to_vec()).unwrap();
                self.buf.drain(..i + 4);
                return h;
            }
            assert!(self.more().await, "EOF in head: {:?}", String::from_utf8_lossy(&self.buf));
        }
    }

    /// Sends a frame with first byte b0 (FIN, RSV, opcode).
    async fn send(&mut self, b0: u8, payload: &[u8]) {
        let mut out = vec![b0];
        let key = [9, 8, 7, 6];
        let mut hdr = Vec::new();
        frame::put_header(&mut hdr, false, 0, payload.len(), self.client.then_some(key));
        out.extend_from_slice(&hdr[1..]);
        let at = out.len();
        out.extend_from_slice(payload);
        if self.client {
            frame::mask(&mut out[at..], key);
        }
        // The other end may have hung up already, which is what some
        // tests are after.
        let _ = self.s.write_all(&out).await;
    }

    async fn send_unmasked(&mut self, b0: u8, payload: &[u8]) {
        let c = self.client;
        self.client = false;
        self.send(b0, payload).await;
        self.client = c;
    }

    /// The next frame, None at EOF.
    async fn recv(&mut self) -> Option<(Header, Vec<u8>)> {
        loop {
            if let Some((h, n)) = frame::parse_header(&self.buf).unwrap() {
                let total = n + h.len as usize;
                if self.buf.len() >= total {
                    let mut p = self.buf[n..total].to_vec();
                    if let Some(k) = h.mask {
                        frame::mask(&mut p, k);
                    }
                    self.buf.drain(..total);
                    return Some((h, p));
                }
            }
            if !self.more().await {
                return None;
            }
        }
    }

    /// The next close frame's status and reason, skipping other frames.
    async fn close_frame(&mut self) -> Option<(u16, String)> {
        loop {
            let (h, p) = self.recv().await?;
            if h.opcode == OP_CLOSE {
                if p.is_empty() {
                    return Some((1005, String::new()));
                }
                return Some((u16::from_be_bytes([p[0], p[1]]), String::from_utf8(p[2..].to_vec()).unwrap()));
            }
        }
    }
}

/// Starts accept on one end of a pipe, sends req on the other.
async fn raw_accept(req: &str, a: Acceptor) -> (Raw, JoinHandle<Result<Conn<DuplexStream>, Error>>) {
    let (c, s) = duplex(4 << 20);
    let task = spawn_local(async move { a.accept(s).await });
    let mut raw = Raw { s: c, buf: Vec::new(), client: true };
    raw.s.write_all(req.as_bytes()).await.unwrap();
    (raw, task)
}

/// An accepted server conn and a raw client, after a successful upgrade.
async fn raw_server(extra: &str) -> (Raw, Conn<DuplexStream>, String) {
    let (mut raw, task) = raw_accept(&upgrade(extra), Acceptor::default()).await;
    let head = raw.head().await;
    (raw, task.await.unwrap().unwrap(), head)
}

/// A dialing client conn and a raw server that answered with resp_extra.
async fn raw_client(resp_extra: &str) -> (Raw, Result<Conn<DuplexStream>, Error>, String) {
    let (c, s) = duplex(4 << 20);
    let task = spawn_local(async move { dial(c, "h:1").await });
    let mut raw = Raw { s, buf: Vec::new(), client: false };
    let req = raw.head().await;
    let key = req
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .unwrap()
        .to_string();
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n{resp_extra}\r\n",
        accept_key(&key)
    );
    raw.s.write_all(resp.as_bytes()).await.unwrap();
    (raw, task.await.unwrap(), req)
}

async fn pair(compress: bool) -> (Conn<DuplexStream>, Conn<DuplexStream>) {
    let (c, s) = duplex(4 << 20);
    let server = spawn_local(async move { accept(s, compress).await });
    let client = dial(c, "h:1").await.unwrap();
    (server.await.unwrap().unwrap(), client)
}

fn text(n: usize, seed: usize) -> Vec<u8> {
    let line = format!("drwxr-xr-x  5 osewa staff  160 Oct 30 12:{:02} \x1b[1;34msrc{}\x1b[0m\r\n", seed % 60, seed);
    line.bytes().cycle().take(n).collect()
}

fn noise(n: usize, mut x: u64) -> Vec<u8> {
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn accept_key_rfc_example() {
    assert_eq!(accept_key(KEY), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

#[tokio::test]
async fn round_trip() {
    local(async {
        for compress in [true, false] {
            let (server, client) = pair(compress).await;
            assert_eq!(server.compressed(), compress);
            assert_eq!(client.compressed(), compress);
            assert_eq!(server.subprotocol(), "osewa.v0");
            let mut msgs: Vec<Vec<u8>> =
                [0, 1, 127, 128, 129, 4096, 65535, 65536, 300_000].iter().map(|&n| text(n, n)).collect();
            msgs.push(noise(200_000, 7));
            msgs.push(text(MAX_FRAME, 1));
            msgs.extend((0..500).map(|i| text(200 + i % 50, i)));
            for m in &msgs {
                server.write(m).await.unwrap();
                assert_eq!(&client.read().await.unwrap(), m);
                client.write(m).await.unwrap();
                assert_eq!(&server.read().await.unwrap(), m);
            }
        }
    })
    .await;
}

#[tokio::test]
async fn concurrent_read_write_ping_close() {
    local(async {
        let (server, client) = pair(true).await;
        let n = 2000;
        let sr = server.clone();
        let reader = spawn_local(async move {
            let mut got = 0;
            loop {
                match sr.read().await {
                    Ok(m) => {
                        assert_eq!(m, text(100 + got % 300, got));
                        got += 1;
                    }
                    Err(e) => return (got, e),
                }
            }
        });
        let cr = client.clone();
        let echo = spawn_local(async move {
            while let Ok(m) = cr.read().await {
                if cr.write(&m).await.is_err() {
                    break;
                }
            }
        });
        let pinger = server.clone();
        let pings = spawn_local(async move {
            for _ in 0..20 {
                pinger.ping().await.unwrap();
            }
        });
        for i in 0..n {
            server.write(&text(100 + i % 300, i)).await.unwrap();
            if i % 100 == 0 {
                client.ping().await.unwrap_or(());
            }
        }
        pings.await.unwrap();
        while client.ping().await.is_err() {}
        // The echo task may still be catching up: give it the reads.
        tokio::time::sleep(Duration::from_millis(100)).await;
        server.close(STATUS_GOING_AWAY, "server closing").await.unwrap();
        let (got, e) = reader.await.unwrap();
        assert_eq!(got, n);
        assert!(matches!(e, Error::Close(ref c) if c.code == STATUS_GOING_AWAY), "{e}");
        echo.await.unwrap();
        assert!(matches!(client.read().await, Err(Error::Closed)));
        assert!(matches!(server.write(b"x").await, Err(Error::Closed)));
    })
    .await;
}

#[tokio::test]
async fn close_handshake_both_ways() {
    local(async {
        let (server, client) = pair(true).await;
        let c2 = client.clone();
        let r = spawn_local(async move { c2.read().await });
        server.close(4000, "rust bye").await.unwrap();
        let e = r.await.unwrap().unwrap_err();
        assert_eq!(e.close_status(), Some(4000));
        assert!(matches!(e, Error::Close(CloseFrame { ref reason, .. }) if reason == "rust bye"));
        assert!(matches!(server.close(1000, "").await, Err(Error::Closed)));
        assert!(matches!(client.close(1000, "").await, Err(Error::Closed)));

        let (server, client) = pair(true).await;
        let s2 = server.clone();
        let r = spawn_local(async move { s2.read().await });
        client.close(STATUS_NORMAL, "").await.unwrap();
        assert_eq!(r.await.unwrap().unwrap_err().close_status(), Some(STATUS_NORMAL));
        assert!(matches!(server.write(b"late").await, Err(Error::Closed)));
    })
    .await;
}

#[tokio::test]
async fn close_without_status() {
    local(async {
        let (mut raw, server, _) = raw_server(OSEWA).await;
        let r = spawn_local(async move { server.read().await });
        raw.send(0x80 | OP_CLOSE, &[]).await;
        assert_eq!(raw.close_frame().await, Some((1005, String::new())));
        assert_eq!(r.await.unwrap().unwrap_err().close_status(), Some(STATUS_NO_STATUS));
    })
    .await;
}

#[tokio::test]
async fn close_now_wakes_everyone() {
    local(async {
        let (server, client) = pair(true).await;
        let s2 = server.clone();
        let r = spawn_local(async move { s2.read().await });
        let s3 = server.clone();
        let p = spawn_local(async move { s3.ping().await });
        tokio::task::yield_now().await;
        server.close_now().unwrap();
        assert!(matches!(r.await.unwrap(), Err(Error::Closed)));
        assert!(matches!(p.await.unwrap(), Err(Error::Closed)));
        assert!(matches!(server.close_now(), Err(Error::Closed)));
        // The peer sees the stream end.
        assert!(matches!(client.read().await, Err(Error::Io(_))));
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn close_times_out_without_an_answer() {
    local(async {
        let (mut raw, server, _) = raw_server(OSEWA).await;
        let start = tokio::time::Instant::now();
        let closing = spawn_local(async move { server.close(STATUS_NORMAL, "bye").await });
        assert_eq!(raw.close_frame().await, Some((1000, "bye".into())));
        assert!(matches!(closing.await.unwrap(), Err(Error::Timeout)));
        assert!(start.elapsed() >= Duration::from_secs(5));
        assert_eq!(raw.recv().await, None);
    })
    .await;
}

#[tokio::test]
async fn cancelled_read_resumes() {
    local(async {
        let (server, client) = pair(true).await;
        assert!(tokio::time::timeout(Duration::from_millis(20), client.read()).await.is_err());
        server.write(&text(5000, 1)).await.unwrap();
        assert_eq!(client.read().await.unwrap(), text(5000, 1));
    })
    .await;
}

#[tokio::test]
async fn invalid_close_code_is_refused() {
    local(async {
        let (server, _client) = pair(true).await;
        assert!(matches!(server.close(1005 + 1, "").await, Err(Error::Protocol(_))));
        let (server, _client) = pair(true).await;
        assert!(matches!(server.close(1000, &"x".repeat(124)).await, Err(Error::Protocol(_))));
    })
    .await;
}

// Negotiation, server side.

async fn refused(req: &str) -> String {
    let (mut raw, task) = raw_accept(req, Acceptor::default()).await;
    let head = raw.head().await;
    assert!(matches!(task.await.unwrap(), Err(Error::Handshake(_))));
    head
}

#[tokio::test]
async fn http_refusals() {
    local(async {
        let req = upgrade(OSEWA);
        let cases = [
            (req.replace("GET /osewa ", "GET /other "), "404 Not Found"),
            (req.replace("GET /osewa ", "GET /osewa/ "), "404 Not Found"),
            (req.replace("Upgrade: websocket\r\n", ""), "426 Upgrade Required"),
            (req.replace("Connection: keep-alive, Upgrade", "Connection: keep-alive"), "426 Upgrade Required"),
            (req.replace("HTTP/1.1", "HTTP/1.0"), "426 Upgrade Required"),
            (req.replace("GET ", "POST "), "405 Method Not Allowed"),
            (req.replace("Version: 13", "Version: 12"), "400 Bad Request"),
            (req.replace(&format!("Sec-WebSocket-Key: {KEY}\r\n"), ""), "400 Bad Request"),
            (req.replace(KEY, "c2hvcnQ="), "400 Bad Request"),
            (req.replace(OSEWA, &format!("Sec-WebSocket-Key: {KEY}\r\n")), "400 Bad Request"),
            (req.replace("Host: h:1\r\n", ""), "400 Bad Request"),
            (upgrade("Origin: https://evil.example\r\n"), "403 Forbidden"),
            (upgrade("Origin: null\r\n"), "403 Forbidden"),
            ("GARBAGE\r\n\r\n".to_string(), "400 Bad Request"),
            (upgrade(&"X-Pad: 0123456789abcdef\r\n".repeat(80)), "431 Request Header Fields Too Large"),
        ];
        for (req, want) in cases {
            let head = refused(&req).await;
            assert!(head.starts_with(&format!("HTTP/1.1 {want}\r\n")), "{req:?}\n=> {head}");
        }
        let head = refused(&req.replace("GET ", "POST ").replace("Upgrade: websocket\r\n", "")).await;
        assert!(head.starts_with("HTTP/1.1 426"), "Upgrade is checked before the method, as coder/websocket does");
    })
    .await;
}

#[tokio::test]
async fn origin_of_the_same_host_is_fine() {
    local(async {
        let (_, server, head) = raw_server(&format!("{OSEWA}Origin: https://H:1\r\n")).await;
        assert!(head.starts_with("HTTP/1.1 101"));
        assert_eq!(server.subprotocol(), "osewa.v0");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn header_timeout() {
    local(async {
        let (c, s) = duplex(1024);
        let a = Acceptor { header_timeout: Duration::from_secs(10), ..Acceptor::default() };
        let task = spawn_local(async move { a.accept(s).await });
        let mut c = c;
        c.write_all(b"GET /osewa HTTP/1.1\r\n").await.unwrap();
        let start = tokio::time::Instant::now();
        assert!(matches!(task.await.unwrap(), Err(Error::Handshake(_))));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    })
    .await;
}

#[tokio::test]
async fn upgrade_response() {
    local(async {
        let (_, server, head) = raw_server(&format!("{OSEWA}{DEFLATE}")).await;
        assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(head.contains("Sec-WebSocket-Protocol: osewa.v0\r\n"));
        assert!(head.contains("Sec-WebSocket-Extensions: permessage-deflate\r\n"));
        assert!(server.compressed());

        let (_, server, head) = raw_server("Sec-WebSocket-Protocol: x, osewa.v0\r\n").await;
        assert!(head.contains("Sec-WebSocket-Protocol: osewa.v0\r\n"));
        assert!(!head.contains("Extensions"));
        assert!(!server.compressed());
    })
    .await;
}

#[tokio::test]
async fn no_subprotocol_is_upgraded_then_closed_1008() {
    local(async {
        for extra in ["", "Sec-WebSocket-Protocol: chat\r\n", "Sec-WebSocket-Protocol: OSEWA.V0\r\n"] {
            let (mut raw, task) = raw_accept(&upgrade(extra), Acceptor::default()).await;
            let head = raw.head().await;
            assert!(head.starts_with("HTTP/1.1 101"));
            // selectSubprotocol echoes the client's spelling; the server
            // then wants it exactly.
            assert_eq!(head.contains("Sec-WebSocket-Protocol: OSEWA.V0"), extra.contains("OSEWA"));
            assert_eq!(raw.close_frame().await, Some((1008, "subprotocol osewa.v0 required".into())));
            raw.send(0x80 | OP_CLOSE, &1008u16.to_be_bytes()).await;
            assert!(matches!(task.await.unwrap(), Err(Error::Handshake(_))));
        }
    })
    .await;
}

#[test]
fn deflate_offers() {
    let sel = |v: &str| select_deflate(&handshake::headers(&[("Sec-WebSocket-Extensions", v)]));
    let d = |c, s| Some(Deflate { client_no_context_takeover: c, server_no_context_takeover: s });
    assert_eq!(sel("permessage-deflate"), d(false, false));
    // What coder/websocket's own client offers.
    assert_eq!(sel("permessage-deflate; client_no_context_takeover; server_no_context_takeover"), d(true, true));
    // What browsers offer.
    assert_eq!(sel("permessage-deflate; client_max_window_bits"), d(false, false));
    assert_eq!(sel("permessage-deflate; client_max_window_bits=10"), d(false, false));
    assert_eq!(sel("permessage-deflate; server_max_window_bits=15"), d(false, false));
    // A smaller server window: that offer is skipped, the next taken.
    assert_eq!(sel("permessage-deflate; server_max_window_bits=10"), None);
    assert_eq!(
        sel("permessage-deflate; server_max_window_bits=10, permessage-deflate; client_no_context_takeover"),
        d(true, false)
    );
    assert_eq!(sel("x-webkit-deflate-frame"), None);
    assert_eq!(sel("permessage-deflate; foo"), None);
    assert_eq!(sel(""), None);
}

#[tokio::test]
async fn no_compress_never_negotiates() {
    local(async {
        let a = Acceptor { compress: false, ..Acceptor::default() };
        let (mut raw, task) = raw_accept(&upgrade(&format!("{OSEWA}{DEFLATE}")), a).await;
        assert!(!raw.head().await.contains("Extensions"));
        assert!(!task.await.unwrap().unwrap().compressed());
    })
    .await;
}

// What the server sends.

#[tokio::test]
async fn server_compresses_from_128_bytes_with_context() {
    local(async {
        let (mut raw, server, _) = raw_server(&format!("{OSEWA}{DEFLATE}")).await;
        let mut inf = Inflater::new();
        let mut sizes = Vec::new();
        for (n, want_rsv1) in [(127, false), (128, true), (1000, true), (1000, true), (1000, true)] {
            let m = text(n, 3);
            server.write(&m).await.unwrap();
            let (h, mut p) = raw.recv().await.unwrap();
            assert!(h.fin && h.mask.is_none() && h.opcode == OP_BINARY);
            assert_eq!(h.rsv1, want_rsv1, "{n}");
            if h.rsv1 {
                sizes.push(p.len());
                p.extend_from_slice(&TAIL);
                let mut out = Vec::new();
                inf.inflate(&p, &mut out, 1 << 20).unwrap();
                p = out;
            }
            assert_eq!(p, m);
        }
        // Repeats compress to a back-reference.
        assert!(sizes[2] < 40 && sizes[3] < 40, "{sizes:?}");
    })
    .await;
}

#[tokio::test]
async fn server_without_context_takeover() {
    local(async {
        let ext = "Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; server_no_context_takeover\r\n";
        let (mut raw, server, head) = raw_server(&format!("{OSEWA}{ext}")).await;
        assert!(head.contains(
            "Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; server_no_context_takeover\r\n"
        ));
        server.write(&text(511, 0)).await.unwrap();
        assert!(!raw.recv().await.unwrap().0.rsv1, "the threshold is 512 then");
        let m = text(2000, 0);
        server.write(&m).await.unwrap();
        server.write(&m).await.unwrap();
        let (h1, p1) = raw.recv().await.unwrap();
        let (h2, p2) = raw.recv().await.unwrap();
        assert!(h1.rsv1 && h2.rsv1);
        assert_eq!(p1, p2, "each message starts afresh");

        // And it inflates each client message afresh.
        let mut d = Deflater::new();
        for _ in 0..2 {
            let mut body = Vec::new();
            d.compress(&m, &mut body);
            d.reset();
            raw.send(0xc0 | OP_BINARY, &body).await;
            assert_eq!(server.read().await.unwrap(), m);
        }
    })
    .await;
}

// What the server reads.

#[tokio::test]
async fn server_reads_compressed_fragments_and_pings_between() {
    local(async {
        let (mut raw, server, _) = raw_server(&format!("{OSEWA}{DEFLATE}")).await;
        let mut d = Deflater::new();
        for i in 0..3 {
            let m = text(10_000, i);
            let mut body = Vec::new();
            d.compress(&m, &mut body);
            let (a, b) = body.split_at(body.len() / 2);
            raw.send(0x40 | OP_BINARY, a).await;
            raw.send(0x80 | OP_PING, b"hi").await;
            raw.send(0x80 | OP_CONTINUATION, b).await;
            assert_eq!(server.read().await.unwrap(), m);
            let (h, p) = raw.recv().await.unwrap();
            assert_eq!((h.opcode, p), (OP_PONG, b"hi".to_vec()));
        }
        // Plain fragments too.
        raw.send(OP_BINARY, b"ab").await;
        raw.send(OP_CONTINUATION, b"cd").await;
        raw.send(0x80 | OP_CONTINUATION, b"ef").await;
        assert_eq!(server.read().await.unwrap(), b"abcdef");
    })
    .await;
}

type Frames = Vec<(u8, Vec<u8>)>;

/// Sends a bad frame to a server and returns its read error and the close
/// frame it sent (None: it just hung up).
async fn violate(extra: &str, frames: &[(u8, Vec<u8>)], masked: bool) -> (Error, Option<(u16, String)>) {
    let (mut raw, server, _) = raw_server(&format!("{OSEWA}{extra}")).await;
    let r = spawn_local(async move { server.read().await });
    for (b0, p) in frames {
        if masked {
            raw.send(*b0, p).await;
        } else {
            raw.send_unmasked(*b0, p).await;
        }
    }
    let close = raw.close_frame().await;
    if close.is_some() {
        raw.send(0x80 | OP_CLOSE, &[]).await;
        // The server drains until we hang up.
        raw.s.shutdown().await.unwrap();
    }
    (r.await.unwrap().unwrap_err(), close)
}

#[tokio::test]
async fn server_protocol_violations() {
    local(async {
        let bin = 0x80 | OP_BINARY;
        let cases: Vec<(&str, Frames, u16)> = vec![
            ("", vec![(bin | 0x20, b"x".to_vec())], 1002),
            ("", vec![(bin | 0x40, b"x".to_vec())], 1002), // rsv1 without deflate
            (DEFLATE, vec![(0x80 | OP_PING | 0x40, vec![])], 1002),
            ("", vec![(0x80 | 3, vec![])], 1002),
            ("", vec![(OP_PING, vec![])], 1002),
            ("", vec![(0x80 | OP_PING, vec![0; 126])], 1002),
            ("", vec![(0x80 | OP_CONTINUATION, b"x".to_vec())], 1002),
            ("", vec![(OP_BINARY, b"x".to_vec()), (bin, b"y".to_vec())], 1002),
            ("", vec![(0x80 | OP_CLOSE, vec![3])], 1002),
            ("", vec![(0x80 | OP_CLOSE, 1006u16.to_be_bytes().to_vec())], 1002),
            (DEFLATE, vec![(bin | 0x40, vec![0xff; 8])], 1007),
            ("", vec![(bin, vec![0; MAX_FRAME + 1])], 1009),
            ("", vec![(OP_BINARY, vec![0; MAX_FRAME]), (0x80, vec![0])], 1009),
        ];
        for (extra, frames, code) in cases {
            let (e, close) = violate(extra, &frames, true).await;
            assert_eq!(close.as_ref().map(|c| c.0), Some(code), "{frames:?} => {e}");
            if code == 1009 {
                assert!(matches!(e, Error::TooBig(1048577)));
                assert_eq!(close.unwrap().1, "read limited at 1048577 bytes");
            }
        }
        // coder/websocket just hangs up on an unmasked client frame.
        let (e, close) = violate("", &[(bin, b"x".to_vec())], false).await;
        assert!(matches!(e, Error::Protocol(_)));
        assert_eq!(close, None);
    })
    .await;
}

#[tokio::test]
async fn compressed_bomb_hits_the_limit() {
    local(async {
        let mut d = Deflater::new();
        let mut body = Vec::new();
        d.compress(&vec![0; MAX_FRAME + 1], &mut body);
        let (e, close) = violate(DEFLATE, &[(0xc0 | OP_BINARY, body)], true).await;
        assert!(matches!(e, Error::TooBig(_)));
        assert_eq!(close.unwrap().0, 1009);
    })
    .await;
}

#[tokio::test]
async fn text_is_closed_1003() {
    local(async {
        let (mut raw, server, _) = raw_server(OSEWA).await;
        let r = spawn_local(async move { server.read().await });
        raw.send(0x80 | OP_TEXT, b"hello").await;
        assert_eq!(raw.close_frame().await, Some((1003, "binary frames only".into())));
        raw.send(0x80 | OP_CLOSE, &1003u16.to_be_bytes()).await;
        assert!(matches!(r.await.unwrap(), Err(Error::NotBinary)));
        assert_eq!(raw.recv().await, None);
    })
    .await;
}

#[tokio::test]
async fn set_read_limit() {
    local(async {
        let (server, client) = pair(true).await;
        client.set_read_limit(1000);
        server.write(&text(1000, 0)).await.unwrap();
        assert_eq!(client.read().await.unwrap().len(), 1000);
        server.write(&text(1001, 0)).await.unwrap();
        let r = spawn_local(async move { server.read().await });
        assert!(matches!(client.read().await, Err(Error::TooBig(1001))));
        assert_eq!(r.await.unwrap().unwrap_err().close_status(), Some(STATUS_MESSAGE_TOO_BIG));
    })
    .await;
}

// The client side.

#[tokio::test]
async fn client_request_and_frames() {
    local(async {
        let (mut raw, conn, req) =
            raw_client("Sec-WebSocket-Protocol: osewa.v0\r\nSec-WebSocket-Extensions: permessage-deflate\r\n").await;
        assert!(req.starts_with("GET /osewa HTTP/1.1\r\nHost: h:1\r\n"));
        assert!(req.contains("Sec-WebSocket-Protocol: osewa.v0\r\n"));
        assert!(req.contains("Sec-WebSocket-Extensions: permessage-deflate\r\n"));
        assert!(req.contains("Sec-WebSocket-Version: 13\r\n"));
        let conn = conn.unwrap();
        assert!(conn.compressed());
        // Never compressed, always masked.
        for n in [0, 10, 5000, 200_000] {
            let m = text(n, 1);
            conn.write(&m).await.unwrap();
            let (h, p) = raw.recv().await.unwrap();
            assert!(!h.rsv1 && h.mask.is_some() && h.fin && h.opcode == OP_BINARY);
            assert_eq!(p, m);
        }
        // Inflates what the server compressed, across messages.
        let mut d = Deflater::new();
        for i in 0..3 {
            let m = text(3000, i);
            let mut body = Vec::new();
            d.compress(&m, &mut body);
            raw.send(0xc0 | OP_BINARY, &body).await;
            assert_eq!(conn.read().await.unwrap(), m);
        }
        // Answers pings while reading.
        raw.send(0x80 | OP_PING, b"7").await;
        raw.send(0x80 | OP_BINARY, b"after").await;
        assert_eq!(conn.read().await.unwrap(), b"after");
        let (h, p) = raw.recv().await.unwrap();
        assert_eq!((h.opcode, h.mask.is_some(), p), (OP_PONG, true, b"7".to_vec()));
    })
    .await;
}

#[tokio::test]
async fn client_rejects_masked_server_frames() {
    local(async {
        let (mut raw, conn, _) = raw_client(OSEWA).await;
        let conn = conn.unwrap();
        raw.client = true;
        raw.send(0x80 | OP_BINARY, b"x").await;
        let r = spawn_local(async move { conn.read().await });
        assert_eq!(raw.close_frame().await.unwrap().0, 1002);
        raw.s.shutdown().await.unwrap();
        assert!(matches!(r.await.unwrap(), Err(Error::Protocol(_))));
    })
    .await;
}

#[tokio::test]
async fn client_without_subprotocol_closes_1002() {
    local(async {
        let (c, s) = duplex(1 << 16);
        let task = spawn_local(async move { dial(c, "h").await });
        let mut raw = Raw { s, buf: Vec::new(), client: false };
        let req = raw.head().await;
        let key = req.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().to_string();
        raw.s
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                    accept_key(&key)
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(raw.close_frame().await, Some((1002, "subprotocol".into())));
        raw.send(0x80 | OP_CLOSE, &1002u16.to_be_bytes()).await;
        assert!(matches!(task.await.unwrap(), Err(Error::Handshake(_))));
    })
    .await;
}

#[test]
fn client_verifies_the_response() {
    let key = "AQIDBAUGBwgJCgsMDQ4PEA==";
    let ok = accept_key(key);
    let base = |extra: &[(&'static str, &'static str)]| {
        let mut v = vec![("Upgrade", "websocket"), ("Connection", "Upgrade")];
        v.extend_from_slice(extra);
        v
    };
    let verify = |status, lines: Vec<(&str, &str)>, accept: &str| {
        let mut lines = lines;
        lines.push(("Sec-WebSocket-Accept", accept));
        handshake::verify_response_for_test(status, &handshake::headers(&lines), key)
    };
    let d = |c, s| Some(Deflate { client_no_context_takeover: c, server_no_context_takeover: s });

    assert_eq!(verify(101, base(&[("Sec-WebSocket-Protocol", "osewa.v0")]), &ok).unwrap(), ("osewa.v0".into(), None));
    assert!(verify(200, base(&[]), &ok).is_err());
    assert!(verify(101, vec![("Connection", "Upgrade")], &ok).is_err());
    assert!(verify(101, vec![("Upgrade", "websocket")], &ok).is_err());
    assert!(verify(101, base(&[]), "wrong").is_err());
    assert!(verify(101, base(&[("Sec-WebSocket-Protocol", "chat")]), &ok).is_err());
    assert_eq!(verify(101, base(&[("Sec-WebSocket-Protocol", "OSEWA.v0")]), &ok).unwrap().0, "OSEWA.v0");
    let ext = |v: &'static str| base(&[("Sec-WebSocket-Extensions", v)]);
    assert_eq!(verify(101, ext("permessage-deflate"), &ok).unwrap().1, d(false, false));
    assert_eq!(
        verify(101, ext("permessage-deflate; server_no_context_takeover; client_no_context_takeover"), &ok).unwrap().1,
        d(true, true)
    );
    assert_eq!(verify(101, ext("permessage-deflate; server_max_window_bits=10"), &ok).unwrap().1, d(false, false));
    assert!(verify(101, ext("permessage-deflate; client_max_window_bits=10"), &ok).is_err());
    assert!(verify(101, ext("x-webkit-deflate-frame"), &ok).is_err());
    assert!(verify(101, ext("permessage-deflate, permessage-deflate"), &ok).is_err());
}

#[tokio::test]
async fn client_refused() {
    local(async {
        let (c, s) = duplex(1 << 16);
        let task = spawn_local(async move { dial(c, "h").await });
        let mut raw = Raw { s, buf: Vec::new(), client: false };
        raw.head().await;
        raw.s
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 19\r\n\r\n404 page not found\n")
            .await
            .unwrap();
        let e = task.await.unwrap().unwrap_err();
        assert!(e.to_string().contains("101 but got 404"), "{e}");
    })
    .await;
}

#[tokio::test]
async fn client_server_no_context_takeover() {
    local(async {
        let (c, s) = duplex(1 << 20);
        let offer = Offer { path: "/osewa", subprotocol: Some("osewa.v0"), deflate: true };
        let task = spawn_local(async move { dial_offer(c, "h", &offer).await });
        let mut raw = Raw { s, buf: Vec::new(), client: false };
        let req = raw.head().await;
        let key = req.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().to_string();
        raw.s
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\
                     Sec-WebSocket-Protocol: osewa.v0\r\nSec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover\r\n\r\n",
                    accept_key(&key)
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let conn = task.await.unwrap().unwrap();
        // Each message from a fresh deflater: the client must reset too.
        for i in 0..3 {
            let m = text(3000, i % 2);
            let mut body = Vec::new();
            Deflater::new().compress(&m, &mut body);
            raw.send(0xc0 | OP_BINARY, &body).await;
            assert_eq!(conn.read().await.unwrap(), m);
        }
    })
    .await;
}
