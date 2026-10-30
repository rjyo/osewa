//! The HTTP/1.1 upgrade, as osewa's Go server (net/http + coder/websocket
//! Accept) and client (coder/websocket Dial) do it.

use std::time::Duration;

use base64::Engine as _;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::conn::{Conn, Deflate, Error, STATUS_POLICY_VIOLATION, STATUS_PROTOCOL_ERROR};
use osewa_wire::{MAX_FRAME, PATH, SUBPROTOCOL};

const KEY_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Bounds a request's or response's head.
const MAX_HEAD: usize = 64 << 10;
const MAX_HEADERS: usize = 64;

/// The server side of the upgrade.
#[derive(Debug, Clone)]
pub struct Acceptor {
    /// Offer permessage-deflate (osewa's server unless --no-compress).
    pub compress: bool,
    /// How long the request head may take (net/http's ReadHeaderTimeout).
    pub header_timeout: Duration,
}

impl Default for Acceptor {
    fn default() -> Acceptor {
        Acceptor { compress: true, header_timeout: Duration::from_secs(10) }
    }
}

/// Accepts a WebSocket on an accepted stream (after TLS), as osewa's Go
/// server does.
pub async fn accept<S>(stream: S, compress: bool) -> Result<Conn<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    Acceptor { compress, ..Acceptor::default() }.accept(stream).await
}

/// One header line's name and value.
pub(crate) struct Headers(Vec<(String, String)>);

impl Headers {
    fn from(h: &[httparse::Header<'_>]) -> Headers {
        Headers(
            h.iter()
                .map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).into_owned()))
                .collect(),
        )
    }

    fn values(&self, name: &str) -> Vec<&str> {
        self.0.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
    }

    /// The first value, trimmed (http.Header.Get keeps none of the
    /// surrounding space: textproto trims it).
    fn get(&self, name: &str) -> &str {
        self.values(name).first().map(|v| v.trim()).unwrap_or("")
    }

    /// The comma-separated tokens across every line (coder/websocket's
    /// headerTokens).
    fn tokens(&self, name: &str) -> Vec<String> {
        self.values(name).into_iter().flat_map(|v| v.trim().split(',').map(|t| t.trim().to_string())).collect()
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.tokens(name).iter().any(|t| t.eq_ignore_ascii_case(token))
    }
}

struct Extension {
    name: String,
    params: Vec<String>,
}

fn extensions(h: &Headers) -> Vec<Extension> {
    h.tokens("Sec-WebSocket-Extensions")
        .into_iter()
        .filter(|t| !t.is_empty())
        .map(|t| {
            let mut v = t.split(';').map(|s| s.trim().to_string());
            Extension { name: v.next().unwrap_or_default(), params: v.collect() }
        })
        .collect()
}

/// coder/websocket's selectDeflate in CompressionContextTakeover mode:
/// the first permessage-deflate offer whose parameters it can honor.
pub(crate) fn select_deflate(h: &Headers) -> Option<Deflate> {
    'offers: for ext in extensions(h) {
        if ext.name != "permessage-deflate" {
            continue;
        }
        let mut d = Deflate::default();
        for p in &ext.params {
            match p.as_str() {
                "client_no_context_takeover" => d.client_no_context_takeover = true,
                "server_no_context_takeover" => d.server_no_context_takeover = true,
                // We inflate with the full window, which suits a client
                // that compresses with a smaller one.
                "client_max_window_bits" | "server_max_window_bits=15" => {}
                p if p.starts_with("client_max_window_bits=") => {}
                _ => continue 'offers,
            }
        }
        return Some(d);
    }
    None
}

fn deflate_response(d: Deflate) -> String {
    let mut s = String::from("permessage-deflate");
    if d.client_no_context_takeover {
        s += "; client_no_context_takeover";
    }
    if d.server_no_context_takeover {
        s += "; server_no_context_takeover";
    }
    s
}

pub(crate) fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(KEY_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

enum HeadError {
    TooLarge,
    Malformed,
    Io(Error),
}

/// Reads up to the end of an HTTP head, returning the bytes read and the
/// head's length.
async fn read_head<S: AsyncRead + Unpin>(
    s: &mut S,
    parse: impl Fn(&[u8]) -> Result<Option<usize>, httparse::Error>,
) -> Result<(Vec<u8>, usize), HeadError> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        if buf.len() >= MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
        buf.reserve(4096);
        let n = s.read_buf(&mut buf).await.map_err(|e| HeadError::Io(e.into()))?;
        if n == 0 {
            return Err(HeadError::Io(Error::Io(std::io::ErrorKind::UnexpectedEof.into())));
        }
        match parse(&buf) {
            Ok(Some(n)) => return Ok((buf, n)),
            Ok(None) => {}
            Err(httparse::Error::TooManyHeaders) => return Err(HeadError::TooLarge),
            Err(_) => return Err(HeadError::Malformed),
        }
    }
}

fn parse_request(b: &[u8]) -> Result<Option<usize>, httparse::Error> {
    let mut h = [httparse::EMPTY_HEADER; MAX_HEADERS];
    Ok(match httparse::Request::new(&mut h).parse(b)? {
        httparse::Status::Complete(n) => Some(n),
        httparse::Status::Partial => None,
    })
}

fn parse_response(b: &[u8]) -> Result<Option<usize>, httparse::Error> {
    let mut h = [httparse::EMPTY_HEADER; MAX_HEADERS];
    Ok(match httparse::Response::new(&mut h).parse(b)? {
        httparse::Status::Complete(n) => Some(n),
        httparse::Status::Partial => None,
    })
}

/// A refusal: status, reason, body and extra headers, as net/http's
/// http.Error writes it.
struct Refusal {
    status: u16,
    reason: &'static str,
    body: String,
    headers: Vec<(&'static str, &'static str)>,
}

fn refuse(status: u16, reason: &'static str, body: impl Into<String>) -> Refusal {
    Refusal { status, reason, body: body.into(), headers: Vec::new() }
}

fn upgrade_required(body: String) -> Refusal {
    Refusal {
        status: 426,
        reason: "Upgrade Required",
        body,
        headers: vec![("Connection", "Upgrade"), ("Upgrade", "websocket")],
    }
}

async fn respond<S: AsyncWrite + Unpin>(s: &mut S, r: &Refusal) {
    let mut out = format!("HTTP/1.1 {} {}\r\n", r.status, r.reason);
    let mut close = true;
    for (k, v) in &r.headers {
        out += &format!("{k}: {v}\r\n");
        close &= *k != "Connection";
    }
    if close {
        out += "Connection: close\r\n";
    }
    let body = format!("{}\n", r.body);
    out += &format!(
        "Content-Type: text/plain; charset=utf-8\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(out.as_bytes()).await;
    let _ = s.flush().await;
    let _ = s.shutdown().await;
}

/// What a valid upgrade request asks for.
struct Upgrade {
    key: String,
    subprotocol: String,
    deflate: Option<Deflate>,
}

/// net/http's routing and coder/websocket's verifyClientRequest and
/// authenticateOrigin, in their order.
fn check_request(req: &httparse::Request<'_, '_>, h: &Headers, compress: bool) -> Result<Upgrade, Refusal> {
    let host = h.get("Host");
    if req.version == Some(1) && h.values("Host").is_empty() {
        return Err(refuse(400, "Bad Request", "missing required Host header"));
    }
    let target = req.path.unwrap_or("");
    let path = target.split(['?', '#']).next().unwrap_or("");
    if path != PATH {
        return Err(refuse(404, "Not Found", "404 page not found"));
    }
    if req.version != Some(1) {
        return Err(refuse(
            426,
            "Upgrade Required",
            "WebSocket protocol violation: handshake request must be at least HTTP/1.1: \"HTTP/1.0\"",
        ));
    }
    if !h.has_token("Connection", "Upgrade") {
        return Err(upgrade_required(format!(
            "WebSocket protocol violation: Connection header {:?} does not contain Upgrade",
            h.get("Connection")
        )));
    }
    if !h.has_token("Upgrade", "websocket") {
        return Err(upgrade_required(format!(
            "WebSocket protocol violation: Upgrade header {:?} does not contain websocket",
            h.get("Upgrade")
        )));
    }
    let method = req.method.unwrap_or("");
    if method != "GET" {
        return Err(refuse(
            405,
            "Method Not Allowed",
            format!("WebSocket protocol violation: handshake request method is not GET but {method:?}"),
        ));
    }
    if h.get("Sec-WebSocket-Version") != "13" {
        let mut r = refuse(
            400,
            "Bad Request",
            format!(
                "unsupported WebSocket protocol version (only 13 is supported): {:?}",
                h.get("Sec-WebSocket-Version")
            ),
        );
        r.headers.push(("Sec-WebSocket-Version", "13"));
        return Err(r);
    }
    let keys = h.values("Sec-WebSocket-Key");
    match keys.len() {
        0 => return Err(refuse(400, "Bad Request", "WebSocket protocol violation: missing Sec-WebSocket-Key")),
        1 => {}
        _ => {
            return Err(refuse(
                400,
                "Bad Request",
                "WebSocket protocol violation: multiple Sec-WebSocket-Key headers",
            ))
        }
    }
    let key = keys[0].trim().to_string();
    match base64::engine::general_purpose::STANDARD.decode(&key) {
        Ok(v) if v.len() == 16 => {}
        _ => {
            return Err(refuse(
                400,
                "Bad Request",
                format!(
                    "WebSocket protocol violation: invalid Sec-WebSocket-Key {key:?}, must be a 16 byte base64 encoded string"
                ),
            ))
        }
    }
    let origin = h.get("Origin");
    if !origin.is_empty() {
        let o = origin_host(origin);
        if !o.eq_ignore_ascii_case(host) {
            return Err(refuse(
                403,
                "Forbidden",
                if o.is_empty() {
                    format!("request Origin {origin:?} is not a valid URL with a host")
                } else {
                    format!("request Origin {o:?} is not authorized for Host {host:?}")
                },
            ));
        }
    }
    // The client's spelling of ours, as selectSubprotocol returns it.
    let subprotocol = h
        .tokens("Sec-WebSocket-Protocol")
        .into_iter()
        .find(|p| p.eq_ignore_ascii_case(SUBPROTOCOL))
        .unwrap_or_default();
    let deflate = if compress { select_deflate(h) } else { None };
    Ok(Upgrade { key, subprotocol, deflate })
}

/// The host of an Origin URL ("scheme://host[:port]"), as url.Parse's
/// u.Host: "" when there is none.
fn origin_host(origin: &str) -> &str {
    let Some((_, rest)) = origin.split_once("://") else {
        return "";
    };
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    auth.rsplit_once('@').map_or(auth, |(_, h)| h)
}

impl Acceptor {
    /// Reads the upgrade request, answers it, and returns the connection.
    /// A request the Go server would refuse gets its HTTP status; one
    /// without our subprotocol is upgraded and closed with 1008, as the Go
    /// server does.
    pub async fn accept<S>(&self, mut stream: S) -> Result<Conn<S>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (buf, n) = match tokio::time::timeout(self.header_timeout, read_head(&mut stream, parse_request)).await {
            Err(_) => return Err(Error::Handshake("timed out reading the request head".into())),
            Ok(Err(HeadError::Io(e))) => return Err(e),
            Ok(Err(HeadError::TooLarge)) => {
                respond(&mut stream, &refuse(431, "Request Header Fields Too Large", "431 Request Header Fields Too Large"))
                    .await;
                return Err(Error::Handshake("request head too large".into()));
            }
            Ok(Err(HeadError::Malformed)) => {
                respond(&mut stream, &refuse(400, "Bad Request", "400 Bad Request")).await;
                return Err(Error::Handshake("malformed request".into()));
            }
            Ok(Ok(v)) => v,
        };
        let mut hs = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut hs);
        req.parse(&buf).expect("parsed once already");
        let h = Headers::from(req.headers);
        let up = match check_request(&req, &h, self.compress) {
            Ok(u) => u,
            Err(r) => {
                respond(&mut stream, &r).await;
                return Err(Error::Handshake(format!("{} {}: {}", r.status, r.reason, r.body)));
            }
        };
        let mut out = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
            accept_key(&up.key)
        );
        if !up.subprotocol.is_empty() {
            out += &format!("Sec-WebSocket-Protocol: {}\r\n", up.subprotocol);
        }
        if let Some(d) = up.deflate {
            out += &format!("Sec-WebSocket-Extensions: {}\r\n", deflate_response(d));
        }
        out += "\r\n";
        stream.write_all(out.as_bytes()).await?;
        stream.flush().await?;
        let conn = Conn::new(stream, false, up.subprotocol, up.deflate, buf[n..].to_vec(), MAX_FRAME);
        if conn.subprotocol() != SUBPROTOCOL {
            let reason = format!("subprotocol {SUBPROTOCOL} required");
            let _ = conn.close(STATUS_POLICY_VIOLATION, &reason).await;
            return Err(Error::Handshake(reason));
        }
        Ok(conn)
    }
}

/// What a client offers.
pub(crate) struct Offer<'a> {
    pub path: &'a str,
    pub subprotocol: Option<&'a str>,
    pub deflate: bool,
}

pub(crate) const OSEWA_OFFER: Offer<'static> = Offer { path: PATH, subprotocol: Some(SUBPROTOCOL), deflate: true };

/// Opens a WebSocket over a connected stream (after TLS), as osewa's Go
/// client does: offering osewa.v0 and permessage-deflate with context
/// takeover, never compressing what it sends. host is the Host header.
pub async fn dial<S>(stream: S, host: &str) -> Result<Conn<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    dial_offer(stream, host, &OSEWA_OFFER).await
}

pub(crate) async fn dial_offer<S>(mut stream: S, host: &str, offer: &Offer<'_>) -> Result<Conn<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n",
        offer.path
    );
    if let Some(p) = offer.subprotocol {
        req += &format!("Sec-WebSocket-Protocol: {p}\r\n");
    }
    if offer.deflate {
        req += "Sec-WebSocket-Extensions: permessage-deflate\r\n";
    }
    req += "\r\n";
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let (buf, n) = match read_head(&mut stream, parse_response).await {
        Ok(v) => v,
        Err(HeadError::Io(e)) => return Err(e),
        Err(HeadError::TooLarge) => return Err(Error::Handshake("response head too large".into())),
        Err(HeadError::Malformed) => return Err(Error::Handshake("malformed response".into())),
    };
    let mut hs = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut hs);
    resp.parse(&buf).expect("parsed once already");
    let h = Headers::from(resp.headers);
    let (subprotocol, deflate) = verify_response(resp.code.unwrap_or(0), &h, &key, offer)?;

    let conn = Conn::new(stream, true, subprotocol, deflate, buf[n..].to_vec(), MAX_FRAME);
    if offer.subprotocol == Some(SUBPROTOCOL) && conn.subprotocol() != SUBPROTOCOL {
        let _ = conn.close(STATUS_PROTOCOL_ERROR, "subprotocol").await;
        return Err(Error::Handshake(format!("server did not accept subprotocol {SUBPROTOCOL}")));
    }
    Ok(conn)
}

/// coder/websocket's verifyServerResponse.
fn verify_response(
    status: u16,
    h: &Headers,
    key: &str,
    offer: &Offer<'_>,
) -> Result<(String, Option<Deflate>), Error> {
    let fail = |s: String| Err(Error::Handshake(s));
    if status != 101 {
        return fail(format!("expected handshake response status code 101 but got {status}"));
    }
    if !h.has_token("Connection", "Upgrade") {
        return fail(format!(
            "WebSocket protocol violation: Connection header {:?} does not contain Upgrade",
            h.get("Connection")
        ));
    }
    if !h.has_token("Upgrade", "WebSocket") {
        return fail(format!(
            "WebSocket protocol violation: Upgrade header {:?} does not contain websocket",
            h.get("Upgrade")
        ));
    }
    if h.get("Sec-WebSocket-Accept") != accept_key(key) {
        return fail(format!(
            "WebSocket protocol violation: invalid Sec-WebSocket-Accept {:?}, key {key:?}",
            h.get("Sec-WebSocket-Accept")
        ));
    }
    let proto = h.get("Sec-WebSocket-Protocol");
    if !proto.is_empty() && !offer.subprotocol.is_some_and(|p| p.eq_ignore_ascii_case(proto)) {
        return fail(format!("WebSocket protocol violation: unexpected Sec-WebSocket-Protocol from server: {proto:?}"));
    }
    // verifyServerExtensions.
    let exts = extensions(h);
    if exts.is_empty() {
        return Ok((proto.to_string(), None));
    }
    if exts[0].name != "permessage-deflate" || exts.len() > 1 || !offer.deflate {
        let names: Vec<&str> = exts.iter().map(|e| e.name.as_str()).collect();
        return fail(format!("WebSocket protocol violation: unsupported extensions from server: {names:?}"));
    }
    let mut d = Deflate::default();
    for p in &exts[0].params {
        match p.as_str() {
            "client_no_context_takeover" => d.client_no_context_takeover = true,
            "server_no_context_takeover" => d.server_no_context_takeover = true,
            p if p.starts_with("server_max_window_bits=") => {}
            p => return fail(format!("unsupported permessage-deflate parameter: {p:?}")),
        }
    }
    Ok((proto.to_string(), Some(d)))
}

#[cfg(test)]
pub(crate) fn headers(lines: &[(&str, &str)]) -> Headers {
    Headers(lines.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())).collect())
}

#[cfg(test)]
pub(crate) fn verify_response_for_test(
    status: u16,
    h: &Headers,
    key: &str,
) -> Result<(String, Option<Deflate>), Error> {
    verify_response(status, h, key, &OSEWA_OFFER)
}
