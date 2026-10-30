//! Dialing (Go's client/dial.go and the carrier choice in client.go): TCP
//! with a raced second attempt, pinned TLS, the WebSocket upgrade, and the
//! seam where QUIC races the WebSocket.

use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use osewa_carrier::ws;
use osewa_wire as wire;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_rustls::client::TlsStream;

use crate::{Config, Error, Result};

/// When a dial whose TCP connect hasn't completed gets a second attempt in
/// parallel. A lost SYN or SYN-ACK otherwise waits out TCP's 1 s initial
/// retransmit timeout, which alone breaks the cold-resume target on a lossy
/// link; a fresh attempt's handshake gets through in one RTT.
pub(crate) const RACE_AFTER: Duration = Duration::from_millis(250);

/// How long a dial to a host that advertised QUIC waits for it before
/// starting the WebSocket beside it. A QUIC handshake is one round trip,
/// and wins on any path that carries UDP; one that doesn't costs this
/// much, once per connection.
const QUIC_RACE_AFTER: Duration = Duration::from_millis(300);

/// Bounds unsent bytes in the kernel's send buffer.
pub(crate) const NOT_SENT_LOWAT: libc::c_int = 16 << 10;

#[cfg(target_os = "linux")]
pub(crate) const TCP_NOTSENT_LOWAT: libc::c_int = 25;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;

/// A byte stream TLS runs over: a TCP connection, or a test's stand-in.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + 'static> Io for T {}

pub(crate) type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Replaces the TCP connect (tests): given host:port, a connected stream.
/// It runs on the client's runtime thread.
pub(crate) type TcpDial = Arc<dyn Fn(String) -> LocalBoxFuture<io::Result<Box<dyn Io>>> + Send + Sync>;

type WsConn = ws::Conn<TlsStream<Box<dyn Io>>>;

/// What a connection runs on. QUIC joins as a variant once its carrier is
/// in osewa-carrier.
pub(crate) enum Transport {
    Ws(WsConn),
}

impl Transport {
    pub async fn read(&self) -> Result<Vec<u8>, String> {
        match self {
            Transport::Ws(c) => c.read().await.map_err(|e| e.to_string()),
        }
    }

    pub async fn write(&self, b: &[u8]) -> Result<(), String> {
        match self {
            Transport::Ws(c) => c.write(b).await.map_err(|e| e.to_string()),
        }
    }

    pub async fn ping(&self) -> Result<(), String> {
        match self {
            Transport::Ws(c) => c.ping().await.map_err(|e| e.to_string()),
        }
    }

    pub async fn close(&self, code: u16, reason: &str) {
        match self {
            Transport::Ws(c) => {
                let _ = c.close(code, reason).await;
            }
        }
    }

    pub fn close_now(&self) {
        match self {
            Transport::Ws(c) => {
                let _ = c.close_now();
            }
        }
    }
}

/// How a raced TCP dial went, for the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DialStats {
    /// 1, or 2 if the race started.
    pub attempts: u32,
    /// 0 for the first attempt, 1 for the second.
    pub winner: u32,
}

/// What a dial produced.
pub(crate) struct Dialed {
    pub transport: Transport,
    pub carrier: &'static str,
    pub stats: Option<DialStats>,
}

/// The parts of a wss:// URL a dial needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// host:port as written, for the TCP connect and the Host header.
    pub authority: String,
    /// The host without brackets, for TLS.
    pub host: String,
    pub port: u16,
}

pub(crate) fn parse_url(url: &str) -> Result<Target> {
    let bad = || Error::Other(format!("osewa: bad URL {}: want wss://host:port{}", wire::go_quote(url), wire::PATH));
    let rest = url.strip_prefix("wss://").ok_or_else(bad)?;
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = if let Some(r) = authority.strip_prefix('[') {
        let (h, p) = r.split_once(']').ok_or_else(bad)?;
        (h, p.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(bad());
    }
    let port = match port {
        Some(p) => p.parse().map_err(|_| bad())?,
        None => 443,
    };
    let authority = join_host_port(host, port);
    Ok(Target { authority, host: host.to_string(), port })
}

fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Connects as cfg asks. Runs on the runtime thread.
pub(crate) async fn dial(cfg: &Config) -> Result<Dialed> {
    let quic_addr = || -> Result<String> {
        let t = parse_url(&cfg.url)?;
        Ok(join_host_port(&t.host, if cfg.quic_port > 0 { cfg.quic_port } else { t.port }))
    };
    if cfg.carrier == "quic" {
        let transport = dial_quic(&quic_addr()?, cfg).await?;
        return Ok(Dialed { transport, carrier: "quic", stats: None });
    }
    if cfg.carrier.is_empty() && cfg.quic_port > 0 {
        return dial_race(quic_addr()?, cfg).await;
    }
    let (transport, stats) = dial_ws(cfg).await?;
    Ok(Dialed { transport, carrier: "ws", stats: Some(stats) })
}

/// Connects over QUIC. The carrier is being built (docs/QUIC.md); until it
/// lands this fails at once, which makes a race start the WebSocket
/// without waiting.
async fn dial_quic(addr: &str, _cfg: &Config) -> Result<Transport> {
    Err(Error::Other(format!("osewa: QUIC to {addr}: this build has no QUIC carrier")))
}

/// Dials QUIC and, after QUIC_RACE_AFTER or at once if QUIC fails, the
/// WebSocket; the first connected wins and the other is dropped, which
/// closes it (docs/QUIC.md "Connecting").
async fn dial_race(quic_addr: String, cfg: &Config) -> Result<Dialed> {
    let mut set: JoinSet<(&'static str, Result<Dialed>)> = JoinSet::new();
    let qcfg = cfg.clone();
    set.spawn_local(async move {
        let r = dial_quic(&quic_addr, &qcfg).await.map(|transport| Dialed { transport, carrier: "quic", stats: None });
        ("quic", r)
    });
    let start_ws = |set: &mut JoinSet<_>| {
        let wcfg = cfg.clone();
        set.spawn_local(async move {
            let r = dial_ws(&wcfg).await.map(|(transport, s)| Dialed { transport, carrier: "ws", stats: Some(s) });
            ("ws", r)
        });
    };
    let timer = tokio::time::sleep(QUIC_RACE_AFTER);
    tokio::pin!(timer);
    let mut ws_started = false;
    let mut errs = Vec::new();
    loop {
        tokio::select! {
            _ = &mut timer, if !ws_started => {
                ws_started = true;
                start_ws(&mut set);
            }
            r = set.join_next() => {
                let Some(r) = r else { return Err(Error::Join(errs)) };
                let (name, r) = r.map_err(|e| Error::Other(e.to_string()))?;
                match r {
                    // Dropping the set cancels the loser.
                    Ok(d) => return Ok(d),
                    Err(e) => {
                        errs.push(Error::Other(format!("{name}: {e}")));
                        if !ws_started {
                            ws_started = true;
                            start_ws(&mut set);
                        }
                    }
                }
            }
        }
    }
}

/// Connects the WebSocket, racing TCP connects.
async fn dial_ws(cfg: &Config) -> Result<(Transport, DialStats)> {
    let t = parse_url(&cfg.url)?;
    let tls = cfg.tls.as_ref().ok_or_else(|| Error::Other("osewa: Config.tls is required".into()))?;
    let mut tls = (**tls).clone();
    // The WebSocket upgrade is HTTP/1.1; never let ALPN pick h2.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = ServerName::try_from(t.host.clone()).map_err(|e| Error::Other(format!("osewa: {}: {e}", t.host)))?;
    let after = cfg.race_after.unwrap_or(RACE_AFTER);
    let (stream, stats) = race_tls(Arc::new(tls), name, &t.authority, after, cfg.knobs.dial.clone()).await?;
    let conn = ws::dial(stream, &t.authority).await.map_err(|e| Error::Other(e.to_string()))?;
    Ok((Transport::Ws(conn), stats))
}

/// Connects TCP for real: no Nagle (a keystroke must not wait for an ACK),
/// and little unsent data in the kernel, so what the writer queues next (a
/// keystroke ahead of an upload) goes out soon instead of behind a full
/// send buffer (PROTOCOL.md "In flight").
async fn tcp_connect(addr: String) -> io::Result<Box<dyn Io>> {
    let c = TcpStream::connect(addr).await?;
    c.set_nodelay(true)?;
    set_not_sent_lowat(&c);
    Ok(Box::new(c))
}

pub(crate) fn set_not_sent_lowat(c: &impl AsRawFd) {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
    unsafe {
        libc::setsockopt(
            c.as_raw_fd(),
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            (&NOT_SENT_LOWAT as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

enum Attempt {
    Connected,
    Done(u32, io::Result<Box<TlsStream<Box<dyn Io>>>>),
}

/// Dials TCP and TLS happy-eyeballs style: a second attempt starts if the
/// first one's TCP connect is still pending after `after`, and whichever
/// completes TLS first wins. Zero never races.
async fn race_tls(
    tls: Arc<rustls::ClientConfig>,
    name: ServerName<'static>,
    addr: &str,
    after: Duration,
    dial: Option<TcpDial>,
) -> Result<(TlsStream<Box<dyn Io>>, DialStats)> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    // Dropped on return: a losing attempt is cancelled, and its connection
    // closed, whether or not it would still land.
    let mut set = JoinSet::new();
    let start = |set: &mut JoinSet<()>, i: u32| {
        let (tx, tls, name, addr, dial) = (tx.clone(), tls.clone(), name.clone(), addr.to_string(), dial.clone());
        set.spawn_local(async move {
            let c = match dial {
                Some(d) => d(addr).await,
                None => tcp_connect(addr).await,
            };
            let c = match c {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Attempt::Done(i, Err(e)));
                    return;
                }
            };
            let _ = tx.send(Attempt::Connected);
            let r = tokio_rustls::TlsConnector::from(tls).connect(name, c).await.map(Box::new);
            let _ = tx.send(Attempt::Done(i, r));
        });
    };
    start(&mut set, 0);
    let (mut started, mut running) = (1, 1);
    let timer = tokio::time::sleep(after);
    tokio::pin!(timer);
    let mut racing = !after.is_zero();
    let mut errs = Vec::new();
    loop {
        tokio::select! {
            _ = &mut timer, if racing => {
                racing = false;
                start(&mut set, 1);
                started += 1;
                running += 1;
            }
            a = rx.recv() => match a.expect("osewa: attempts hold a sender") {
                // TCP got through: TLS loss recovers on the connection's
                // own, measured, retransmit timer, so a second attempt buys
                // little.
                Attempt::Connected => racing = false,
                Attempt::Done(i, Ok(s)) => return Ok((*s, DialStats { attempts: started, winner: i })),
                Attempt::Done(_, Err(e)) => {
                    running -= 1;
                    errs.push(Error::from(e));
                    // A first attempt that fails before the race starts
                    // (refused, unreachable) isn't retried: another would
                    // fail the same way.
                    if running == 0 {
                        return Err(if errs.len() == 1 { errs.pop().unwrap() } else { Error::Join(errs) });
                    }
                }
            }
        }
    }
}
