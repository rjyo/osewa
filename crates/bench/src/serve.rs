//! `osewa-bench serve` and `bytes` (Go's serve.go): the HTTP and WebSocket
//! target the forward tests reach through forward.open, and the transfer
//! pattern for exec streams.
//!
//! ```text
//! GET  /hello           a fixed line
//! GET  /bytes?n=N       pattern(N)
//! POST /check?n=N       200 when the body is pattern(N)
//!      /ws              WebSocket echo
//! ```
//!
//! Go's net/http and coder/websocket served this; here it is a small
//! HTTP/1.1 server (keep-alive, Content-Length and chunked bodies) and a
//! WebSocket echo without extensions, which is what the Go one negotiates
//! with its defaults.

use std::io::{self, Read, Write};

use base64::Engine;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::pattern::{check_pattern, pattern, Checker};
use crate::Error;

pub fn serve_main(args: &[String]) -> Result<(), Error> {
    let mut fs = osewa_flag::FlagSet::new("serve");
    fs.value("listen", "127.0.0.1:8080", "address");
    let p = crate::parse(&fs, args)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(Error::io)?;
    rt.block_on(async {
        let ln = TcpListener::bind(p.str("listen")).await.map_err(Error::io)?;
        serve(ln).await.map_err(Error::io)
    })
}

/// Serves the mux on ln until accepting fails.
pub async fn serve(ln: TcpListener) -> io::Result<()> {
    loop {
        let (c, _) = ln.accept().await?;
        let _ = c.set_nodelay(true);
        tokio::spawn(async move {
            let _ = conn(c).await;
        });
    }
}

/// Bounds a request head, as net/http's default 1 MB.
const MAX_HEAD: usize = 1 << 20;

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .any(|(_, v)| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }

    fn query_n(&self) -> Option<i64> {
        let v = self.query.split('&').find_map(|kv| kv.strip_prefix("n="))?;
        let n: i64 = v.parse().ok()?;
        (n >= 0).then_some(n)
    }
}

/// Reads a request head; None at a clean end of the connection.
async fn read_head<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> io::Result<Option<Request>> {
    let mut buf = Vec::new();
    loop {
        let mut b = [0u8; 1];
        if r.read(&mut b).await? == 0 {
            return if buf.is_empty() { Ok(None) } else { Err(io::ErrorKind::UnexpectedEof.into()) };
        }
        buf.push(b[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
    }
    let mut hs = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut hs);
    req.parse(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = req.path.unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Some(Request {
        method: req.method.unwrap_or("").to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: req.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned())).collect(),
    }))
}

/// Reads the request body, passing each piece to f.
async fn read_body<R: AsyncRead + Unpin>(r: &mut BufReader<R>, req: &Request, mut f: impl FnMut(&[u8])) -> io::Result<()> {
    let mut buf = vec![0u8; 64 << 10];
    if req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let line = read_line(r).await?;
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if size == 0 {
                // Trailers, then the blank line.
                while !read_line(r).await?.is_empty() {}
                return Ok(());
            }
            let mut left = size;
            while left > 0 {
                let k = r.read(&mut buf[..left.min(64 << 10)]).await?;
                if k == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                f(&buf[..k]);
                left -= k;
            }
            read_line(r).await?;
        }
    }
    let mut left: u64 = req.header("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    while left > 0 {
        let k = r.read(&mut buf[..left.min(64 << 10) as usize]).await?;
        if k == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        f(&buf[..k]);
        left -= k as u64;
    }
    Ok(())
}

async fn read_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let mut b = [0u8; 1];
        if r.read(&mut b).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if b[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        line.push(b[0]);
    }
}

async fn respond<W: AsyncWrite + Unpin>(w: &mut W, status: u16, reason: &str, ctype: &str, body: &[u8]) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {ctype}\r\n", body.len());
    if ctype.starts_with("text/plain") && status >= 400 {
        head += "X-Content-Type-Options: nosniff\r\n";
    }
    head += "\r\n";
    w.write_all(head.as_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

/// net/http's http.Error.
async fn http_error<W: AsyncWrite + Unpin>(w: &mut W, status: u16, reason: &str, msg: &str) -> io::Result<()> {
    respond(w, status, reason, "text/plain; charset=utf-8", format!("{msg}\n").as_bytes()).await
}

async fn conn(c: TcpStream) -> io::Result<()> {
    let (rd, mut wr) = c.into_split();
    let mut rd = BufReader::new(rd);
    loop {
        let Some(req) = read_head(&mut rd).await? else { return Ok(()) };
        let close = req.has_token("connection", "close");
        match (req.method.as_str(), req.path.as_str()) {
            (_, "/ws") => return ws_echo(rd, wr, &req).await,
            ("GET" | "HEAD", "/hello") => {
                read_body(&mut rd, &req, |_| {}).await?;
                respond(&mut wr, 200, "OK", "text/plain; charset=utf-8", b"hello from osewa-bench\n").await?;
            }
            ("GET" | "HEAD", "/bytes") => {
                read_body(&mut rd, &req, |_| {}).await?;
                match req.query_n() {
                    None => http_error(&mut wr, 400, "Bad Request", "bad n").await?,
                    Some(n) => {
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\nContent-Type: application/octet-stream\r\n\r\n");
                        wr.write_all(head.as_bytes()).await?;
                        let mut p = pattern(n);
                        let mut buf = vec![0u8; 32 << 10];
                        loop {
                            let k = p.read(&mut buf)?;
                            if k == 0 {
                                break;
                            }
                            wr.write_all(&buf[..k]).await?;
                        }
                        wr.flush().await?;
                    }
                }
            }
            ("POST", "/check") => {
                let n = req.query_n();
                let mut checker = n.map(Checker::new);
                read_body(&mut rd, &req, |b| {
                    if let Some(c) = checker.as_mut() {
                        let _ = c.feed(b);
                    }
                })
                .await?;
                match checker {
                    None => http_error(&mut wr, 400, "Bad Request", "bad n").await?,
                    Some(c) => match c.finish() {
                        Ok(()) => respond(&mut wr, 200, "OK", "text/plain; charset=utf-8", b"ok\n").await?,
                        Err(e) => http_error(&mut wr, 422, "Unprocessable Entity", &e.to_string()).await?,
                    },
                }
            }
            (_, "/hello" | "/bytes" | "/check") => {
                read_body(&mut rd, &req, |_| {}).await?;
                let allow = if req.path == "/check" { "POST" } else { "GET, HEAD" };
                let body = b"Method Not Allowed\n";
                let head = format!(
                    "HTTP/1.1 405 Method Not Allowed\r\nAllow: {allow}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nX-Content-Type-Options: nosniff\r\n\r\n",
                    body.len()
                );
                wr.write_all(head.as_bytes()).await?;
                wr.write_all(body).await?;
            }
            _ => {
                read_body(&mut rd, &req, |_| {}).await?;
                http_error(&mut wr, 404, "Not Found", "404 page not found").await?;
            }
        }
        if close {
            return Ok(());
        }
    }
}

/// Bounds an echoed message, as the Go target's SetReadLimit.
const WS_READ_LIMIT: usize = 16 << 20;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

async fn ws_echo<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(mut rd: BufReader<R>, mut wr: W, req: &Request) -> io::Result<()> {
    if !req.has_token("connection", "upgrade") {
        let msg = format!(
            "WebSocket protocol violation: Connection header {:?} does not contain Upgrade",
            req.header("connection").unwrap_or("")
        );
        return http_error(&mut wr, 426, "Upgrade Required", &msg).await;
    }
    if !req.has_token("upgrade", "websocket") {
        let msg = format!(
            "WebSocket protocol violation: Upgrade header {:?} does not contain websocket",
            req.header("upgrade").unwrap_or("")
        );
        return http_error(&mut wr, 426, "Upgrade Required", &msg).await;
    }
    if req.method != "GET" {
        return http_error(&mut wr, 405, "Method Not Allowed", &format!("WebSocket protocol violation: handshake request method is not GET but {:?}", req.method)).await;
    }
    if req.header("sec-websocket-version") != Some("13") {
        return http_error(&mut wr, 400, "Bad Request", "unsupported WebSocket protocol version (only 13 is supported)").await;
    }
    let Some(key) = req.header("sec-websocket-key").map(str::trim).filter(|k| !k.is_empty()) else {
        return http_error(&mut wr, 400, "Bad Request", "WebSocket protocol violation: missing Sec-WebSocket-Key").await;
    };
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(WS_GUID.as_bytes());
    let accept = base64::engine::general_purpose::STANDARD.encode(h.finalize());
    let head = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
    wr.write_all(head.as_bytes()).await?;
    wr.flush().await?;
    let mut msg = Vec::new();
    let mut msg_op = 0u8;
    loop {
        let (fin, op, payload) = read_ws_frame(&mut rd).await?;
        match op {
            0x8 => {
                // Close: answer with the same status and hang up.
                let _ = write_ws_frame(&mut wr, 0x8, &payload[..payload.len().min(2)]).await;
                return Ok(());
            }
            0x9 => write_ws_frame(&mut wr, 0xA, &payload).await?,
            0xA => {}
            0x0..=0x2 => {
                if op != 0 {
                    msg_op = op;
                    msg.clear();
                }
                msg.extend_from_slice(&payload);
                if msg.len() > WS_READ_LIMIT {
                    let mut b = 1009u16.to_be_bytes().to_vec();
                    b.extend_from_slice(format!("read limited at {} bytes", WS_READ_LIMIT + 1).as_bytes());
                    let _ = write_ws_frame(&mut wr, 0x8, &b).await;
                    return Ok(());
                }
                if fin {
                    write_ws_frame(&mut wr, msg_op, &msg).await?;
                    msg.clear();
                }
            }
            _ => return Ok(()),
        }
    }
}

async fn read_ws_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<(bool, u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h).await?;
    let (fin, op, masked) = (h[0] & 0x80 != 0, h[0] & 0x0f, h[1] & 0x80 != 0);
    let mut len = (h[1] & 0x7f) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b).await?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).await?;
        len = u64::from_be_bytes(b);
    }
    if len > WS_READ_LIMIT as u64 + 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).await?;
    }
    let mut p = vec![0u8; len as usize];
    r.read_exact(&mut p).await?;
    if masked {
        for (i, b) in p.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((fin, op, p))
}

async fn write_ws_frame<W: AsyncWrite + Unpin>(w: &mut W, op: u8, p: &[u8]) -> io::Result<()> {
    let mut b = vec![0x80 | op];
    match p.len() {
        n if n < 126 => b.push(n as u8),
        n if n <= 0xffff => {
            b.push(126);
            b.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            b.push(127);
            b.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    b.extend_from_slice(p);
    w.write_all(&b).await?;
    w.flush().await
}

/// `osewa-bench bytes`: writes pattern(n) to w, or checks r is it.
pub fn bytes_main(args: &[String], r: impl Read, w: &mut dyn Write) -> Result<(), Error> {
    let mut fs = osewa_flag::FlagSet::new("bytes");
    fs.value("n", "0", "bytes to write").bool("check", false, "instead read stdin and check it is the pattern");
    let p = crate::parse(&fs, args)?;
    let n = crate::int(&p, "n")?;
    if p.bool("check") {
        check_pattern(r, n).map_err(|e| Error(e.to_string()))?;
        writeln!(w, "ok").map_err(Error::io)?;
        return Ok(());
    }
    io::copy(&mut pattern(n), w).map_err(Error::io)?;
    Ok(())
}
