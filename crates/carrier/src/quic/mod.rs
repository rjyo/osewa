//! The QUIC carrier (docs/QUIC.md), wire-compatible with the Go peers'
//! quic-go one (internal/carrier): the same ALPNs, transport parameters,
//! main stream, lanes and compression.
//!
//! Frames travel on QUIC streams, each after its length (u32):
//!   - the main stream (bidirectional, opened by the client) carries all
//!     but channel data: control, requests, sessions, credit and grants. A
//!     zero length, which no frame has, is a ping, answered with one;
//!   - a lane (a unidirectional stream, opened by its sender) carries one
//!     channel stream's data one way: DATA and EOF from the client;
//!     CHANNEL_DATA, CHANNEL_EOF and then STREAM_END from the server.
//!
//! So a lost packet of a download only holds up that download: a
//! keystroke's echo, on the main stream, doesn't wait for its
//! retransmission. Order holds within a stream; the protocol needs no more
//! across them (a channel's data and STREAM_END share its lane, a client
//! registers a channel before opening it and never reuses its id).

mod frames;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use osewa_wire as wire;
use quinn::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use quinn::{congestion, EndpointConfig, IdleTimeout, TransportConfig, VarInt};
pub use quinn::{Connection, Endpoint};
use tokio::sync::{mpsc, watch, Mutex};

use frames::{conn_err, Item, Reader, Writer};

/// ALPN names the protocol in the QUIC handshake, as the WebSocket
/// subprotocol does over TCP. ALPN_DEFLATE is the same with the server's
/// half of the main stream compressed (deflate, flushed after each frame),
/// as permessage-deflate compresses the WebSocket's. A client offers both;
/// the server takes ALPN_DEFLATE unless it doesn't compress.
pub const ALPN: &str = wire::SUBPROTOCOL;
pub const ALPN_DEFLATE: &str = "osewa.v0+deflate";

/// The congestion controller a connection's sender runs (docs/QUIC.md,
/// "Rust"). Each end picks its own: the server's governs downloads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Congestion {
    /// quinn's Cubic (RFC 9438): slow start ends only on loss or ECN, never
    /// on a delay rise, which is what stalled quic-go's on jittery paths.
    #[default]
    Cubic,
    /// quinn's BBR (v1, marked experimental by quinn).
    Bbr,
    NewReno,
}

impl Congestion {
    /// The environment variable that picks one, for A/B runs on a real
    /// path: cubic (the default), bbr or newreno.
    pub const ENV: &'static str = "OSEWA_QUIC_CC";

    pub fn parse(s: &str) -> Option<Congestion> {
        match s {
            "cubic" => Some(Congestion::Cubic),
            "bbr" => Some(Congestion::Bbr),
            "newreno" => Some(Congestion::NewReno),
            _ => None,
        }
    }

    /// From $OSEWA_QUIC_CC; the default when unset or unknown (with a
    /// line on stderr for the latter, so an A/B run can't silently
    /// measure the wrong one).
    pub fn from_env() -> Congestion {
        match std::env::var(Self::ENV) {
            Ok(v) if !v.is_empty() => Congestion::parse(&v).unwrap_or_else(|| {
                eprintln!("osewa: unknown {}={v:?}, using cubic", Self::ENV);
                Congestion::default()
            }),
            _ => Congestion::default(),
        }
    }
}

/// quic-go's first congestion window: 32 packets. quinn's default (ten,
/// 14720 bytes) would cost a fresh connection's first download a round
/// trip or two against the Go peers.
const INITIAL_WINDOW: u64 = 32 * 1200;

/// What quic-go asks of a UDP socket's buffers: a burst at 10 MB/s and
/// 120 ms doesn't fit Linux's default 208 KiB.
const SOCKET_BUFFER: usize = 7 << 20;

/// Both ends' transport parameters, Go's QUICConfig: first packets that fit
/// a tailnet's 1280-byte MTU (path MTU discovery then grows them), liveness
/// as the WebSocket's pings keep it, and receive windows above credit's
/// (wire::CHANNEL_WINDOW, 4 MiB) from the start, so that osewa's own credit
/// always binds first and the queue stays where it can be dropped. Lanes
/// come and go with channels.
pub fn transport(cc: Congestion) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.initial_mtu(1200)
        .min_mtu(1200)
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(30_000))))
        .keep_alive_interval(Some(Duration::from_secs(15)))
        // quinn's windows don't grow, so they start where quic-go's end:
        // 8 MiB per stream, 64 MiB per connection.
        .stream_receive_window(VarInt::from_u32(8 << 20))
        .receive_window(VarInt::from_u32(64 << 20))
        // Written but unacknowledged, across streams; each write moves it
        // with the congestion window (frames::bound_unsent).
        .send_window(INITIAL_WINDOW + frames::LOWAT)
        .max_concurrent_uni_streams(VarInt::from_u32(4096))
        .max_concurrent_bidi_streams(VarInt::from_u32(100))
        // quic-go's first RTT guess, so a lost first packet is resent as
        // soon as it would be there.
        .initial_rtt(Duration::from_millis(100))
        // Go's peers enable none.
        .datagram_receive_buffer_size(None);
    match cc {
        Congestion::Cubic => {
            let mut c = congestion::CubicConfig::default();
            c.initial_window(INITIAL_WINDOW);
            t.congestion_controller_factory(Arc::new(c));
        }
        Congestion::Bbr => {
            let mut c = congestion::BbrConfig::default();
            c.initial_window(INITIAL_WINDOW);
            t.congestion_controller_factory(Arc::new(c));
        }
        Congestion::NewReno => {
            let mut c = congestion::NewRenoConfig::default();
            c.initial_window(INITIAL_WINDOW);
            t.congestion_controller_factory(Arc::new(c));
        }
    }
    t
}

/// The server's QUIC config over tls (osewa_auth::server_config: the
/// device check, no resumption and so no 0-RTT), taking ALPN_DEFLATE when
/// it compresses: rustls, like Go, picks by the server's order.
pub fn server_config(mut tls: rustls::ServerConfig, compress: bool, cc: Congestion) -> io::Result<quinn::ServerConfig> {
    tls.alpn_protocols =
        if compress { vec![ALPN_DEFLATE.into(), ALPN.into()] } else { vec![ALPN.into()] };
    let crypto = QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut c = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    c.transport_config(Arc::new(transport(cc)));
    Ok(c)
}

/// A client's QUIC config over tls (osewa_auth::client_config: the
/// server's pinned key), offering both ALPNs.
pub fn client_config(mut tls: rustls::ClientConfig, cc: Congestion) -> io::Result<quinn::ClientConfig> {
    tls.alpn_protocols = vec![ALPN_DEFLATE.into(), ALPN.into()];
    let crypto = QuicClientConfig::try_from(tls).map_err(io::Error::other)?;
    let mut c = quinn::ClientConfig::new(Arc::new(crypto));
    c.transport_config(Arc::new(transport(cc)));
    Ok(c)
}

/// An endpoint on sock, a server's when server is given. It must be
/// called within a tokio runtime, which drives it.
pub fn endpoint(sock: UdpSocket, server: Option<quinn::ServerConfig>) -> io::Result<Endpoint> {
    grow_buffers(&sock);
    let rt = quinn::default_runtime().ok_or_else(|| io::Error::other("quic: no async runtime"))?;
    Endpoint::new(EndpointConfig::default(), server, sock, rt)
}

/// Raises the socket's buffers toward SOCKET_BUFFER, as quic-go does; a
/// system that refuses keeps what it allows.
fn grow_buffers(sock: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let len = size_of::<libc::c_int>() as libc::socklen_t;
    let set = |opt: libc::c_int, n: usize| {
        let v = n as libc::c_int;
        unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, opt, (&v as *const libc::c_int).cast(), len) == 0 }
    };
    let get = |opt: libc::c_int| {
        let (mut v, mut l): (libc::c_int, libc::socklen_t) = (0, len);
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, opt, (&mut v as *mut libc::c_int).cast(), &mut l) };
        v.max(0) as usize
    };
    for (opt, _force) in [(libc::SO_RCVBUF, FORCE[0]), (libc::SO_SNDBUF, FORCE[1])] {
        if get(opt) >= SOCKET_BUFFER {
            continue;
        }
        // darwin refuses more than kern.ipc.maxsockbuf outright; Linux
        // caps at rmem_max silently, which a privileged process can pass.
        let mut n = SOCKET_BUFFER;
        while n >= 256 << 10 && !set(opt, n) {
            n /= 2;
        }
        #[cfg(target_os = "linux")]
        if get(opt) < SOCKET_BUFFER {
            set(_force, SOCKET_BUFFER);
        }
    }
}

#[cfg(target_os = "linux")]
const FORCE: [libc::c_int; 2] = [libc::SO_RCVBUFFORCE, libc::SO_SNDBUFFORCE];
#[cfg(not(target_os = "linux"))]
const FORCE: [libc::c_int; 2] = [0, 0];

/// How long a dial waits before starting another attempt beside it. One
/// that began while the network was down is in its retransmission backoff,
/// seconds apart, when the network comes back; a fresh one sends its first
/// packet at once.
const RACE_AFTER: Duration = Duration::from_millis(500);

/// Bounds the attempts in flight.
const RACE_MAX: usize = 4;

/// Connects to the server's QUIC listener at addr (host:port, the UDP port
/// of the same number as its TCP one) and opens the main stream, starting
/// a new attempt every RACE_AFTER until one succeeds. It never gives up on
/// a silent path; bound it with a timeout. The main stream reaches the
/// server with its first frame (HELLO).
pub async fn dial(addr: &str, tls: rustls::ClientConfig, cc: Congestion) -> io::Result<Conn> {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h).trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr).await?.collect();
    let target = pick(host, &addrs)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("quic: no address for {addr}")))?;
    // The pin, not the name, authenticates the server; any valid name does.
    let name = if rustls::pki_types::ServerName::try_from(host).is_ok() { host } else { "osewa" };
    let bind: SocketAddr = if target.is_ipv6() { "[::]:0".parse().unwrap() } else { "0.0.0.0:0".parse().unwrap() };
    let mut ep = endpoint(UdpSocket::bind(bind)?, None)?;
    ep.set_default_client_config(client_config(tls, cc)?);

    // Dropping the set aborts every attempt still in it, so a dial its
    // caller gave up on (a timeout, a race the WebSocket won) stops
    // sending at once, as Go's deferred cancel does.
    let mut attempts = tokio::task::JoinSet::new();
    let start = |attempts: &mut tokio::task::JoinSet<_>| -> io::Result<()> {
        let connecting = ep.connect(target, name).map_err(io::Error::other)?;
        attempts.spawn(connecting);
        Ok(())
    };
    start(&mut attempts)?;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + RACE_AFTER, RACE_AFTER);
    let qc = loop {
        tokio::select! {
            Some(r) = attempts.join_next() => match r.map_err(io::Error::other)? {
                Ok(qc) => break qc,
                // The peer answered: another attempt meets the same answer.
                Err(e) if refused(&e) => return Err(conn_err(e)),
                Err(_) => {}
            },
            _ = tick.tick() => {
                if attempts.len() < RACE_MAX {
                    start(&mut attempts)?;
                }
            }
        }
    };
    // The losers go with the set, closing any that connected meanwhile.
    drop(attempts);
    let (send, recv) = qc.open_bi().await.map_err(conn_err)?;
    Ok(Conn::new(qc, send, recv, false))
}

/// The address to dial among host's, as Go's net.ResolveUDPAddr("udp")
/// picks it for quic-go: an IPv6 literal's own, else the first IPv4
/// address, else the first.
fn pick(host: &str, addrs: &[SocketAddr]) -> Option<SocketAddr> {
    if host.contains(':') {
        return addrs.iter().find(|a| a.is_ipv6()).copied();
    }
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied()
}

/// Whether a dial failed because the server answered no.
fn refused(e: &quinn::ConnectionError) -> bool {
    matches!(
        e,
        quinn::ConnectionError::TransportError(_)
            | quinn::ConnectionError::ConnectionClosed(_)
            | quinn::ConnectionError::ApplicationClosed(_)
            | quinn::ConnectionError::VersionMismatch
    )
}

/// Takes a connection's main stream, the first the client opens.
pub async fn accept(qc: Connection) -> io::Result<Conn> {
    let (send, recv) = qc.accept_bi().await.map_err(conn_err)?;
    Ok(Conn::new(qc, send, recv, true))
}

/// The close code and reason the peer ended the connection with, if that
/// is what err is: a WebSocket status code, carried as QUIC's application
/// error code.
pub fn close_status(err: &io::Error) -> Option<(u16, String)> {
    match err.get_ref()?.downcast_ref::<quinn::ConnectionError>()? {
        quinn::ConnectionError::ApplicationClosed(c) => {
            Some((u64::from(c.error_code) as u16, String::from_utf8_lossy(&c.reason).into_owned()))
        }
        _ => None,
    }
}

/// One QUIC carrier connection. Clones share it: one task may read while
/// others write, ping or close. Dropping the last clone closes it.
#[derive(Clone)]
pub struct Conn(Arc<Inner>);

struct Inner {
    qc: Connection,
    server: bool,
    deflate: bool,
    out: Arc<Out>,
    rx: Mutex<mpsc::Receiver<io::Result<Vec<u8>>>>,
    pongs: Arc<watch::Sender<u64>>,
    closing: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The readers hold the connection too; this ends them.
        self.qc.close(VarInt::from_u32(0), b"");
    }
}

/// Our half of the main stream and our lanes.
struct Out {
    qc: Connection,
    main: Mutex<Writer>,
    /// By osewa stream id. A lane has its own lock, so a write blocked on
    /// one lane doesn't hold up the main stream or another lane.
    lanes: StdMutex<HashMap<u32, Arc<Mutex<Option<Writer>>>>>,
}

/// quinn sends a higher priority stream's data first. Lanes are 0.
const PRIORITY_MAIN: i32 = 1;

/// Frames read but not yet taken by read; past it the readers wait, and
/// QUIC's flow control pushes back on the peer.
const INBOUND: usize = 64;

impl Conn {
    fn new(qc: Connection, send: quinn::SendStream, recv: quinn::RecvStream, server: bool) -> Conn {
        let deflate = alpn(&qc).as_deref() == Some(ALPN_DEFLATE);
        // The main stream goes ahead of lanes: an echo waits behind none of
        // the download data quinn holds unsent (frames::LOWAT of it, 130 ms
        // at 1 Mbit/s).
        let _ = send.set_priority(PRIORITY_MAIN);
        let out = Arc::new(Out {
            qc: qc.clone(),
            main: Mutex::new(Writer::new(&qc, send, deflate && server)),
            lanes: StdMutex::new(HashMap::new()),
        });
        let (tx, rx) = mpsc::channel(INBOUND);
        let pongs = Arc::new(watch::channel(0u64).0);
        tokio::spawn(read_main(Reader::new(recv, deflate && !server), tx.clone(), out.clone(), pongs.clone(), server));
        tokio::spawn(accept_lanes(qc.clone(), tx));
        Conn(Arc::new(Inner {
            qc,
            server,
            deflate,
            out,
            rx: Mutex::new(rx),
            pongs,
            closing: AtomicBool::new(false),
        }))
    }

    /// The negotiated ALPN.
    pub fn alpn(&self) -> &'static str {
        if self.0.deflate {
            ALPN_DEFLATE
        } else {
            ALPN
        }
    }

    /// The quinn connection, for its stats and addresses.
    pub fn connection(&self) -> &Connection {
        &self.0.qc
    }

    fn closed_err() -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionAborted, "quic: connection closed")
    }

    /// Reads one frame, from the main stream or any lane.
    pub async fn read(&self) -> io::Result<Vec<u8>> {
        let c = &*self.0;
        if c.closing.load(Ordering::Relaxed) {
            return Err(Self::closed_err());
        }
        let m = c.rx.lock().await.recv().await;
        let f = m.unwrap_or_else(|| Err(Self::closed_err()))?;
        if !c.server && f.len() >= wire::HEADER_SIZE && f[0] == wire::TYPE_STREAM_END {
            // The channel is over: our lane to it (DATA) is no longer read.
            self.end_lane(u32::from_le_bytes(f[1..5].try_into().unwrap()));
        }
        Ok(f)
    }

    /// Writes one frame on the main stream or its lane.
    pub async fn write(&self, frame: &[u8]) -> io::Result<()> {
        if frame.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "osewa: empty frame"));
        }
        if self.0.closing.load(Ordering::Relaxed) {
            return Err(Self::closed_err());
        }
        self.0.out.write(frame).await
    }

    /// Pings the peer and waits for its answer. Only a client's pings are
    /// answered, as with Go's peers.
    pub async fn ping(&self) -> io::Result<()> {
        let c = &*self.0;
        let mut pong = c.pongs.subscribe();
        pong.borrow_and_update();
        c.out.main.lock().await.write(&[]).await?;
        tokio::select! {
            r = pong.changed() => r.map_err(|_| Self::closed_err()),
            e = c.qc.closed() => Err(conn_err(e)),
        }
    }

    /// Ends the connection with a WebSocket status code, sent as QUIC's
    /// application error code, and reason. Unlike a WebSocket's close it
    /// doesn't wait: what is still unsent is dropped, as with quic-go.
    pub fn close(&self, code: u16, reason: &str) {
        self.0.closing.store(true, Ordering::Relaxed);
        self.0.qc.close(VarInt::from_u32(code.into()), reason.as_bytes());
    }

    /// Closes without a status (code 0).
    pub fn close_now(&self) {
        self.close(0, "");
    }

    /// Resets our lane for id, if open, without waiting on a write in it.
    fn end_lane(&self, id: u32) {
        let Some(lane) = self.0.out.lanes.lock().unwrap().remove(&id) else { return };
        tokio::spawn(async move {
            if let Some(w) = lane.lock().await.as_mut() {
                let _ = w.s.reset(VarInt::from_u32(0));
            }
        });
    }
}

impl Out {
    async fn write(&self, frame: &[u8]) -> io::Result<()> {
        let Some((id, last)) = self.lane_of(frame) else {
            return self.main.lock().await.write(frame).await;
        };
        let lane = self.lanes.lock().unwrap().entry(id).or_default().clone();
        let mut w = lane.lock().await;
        if w.is_none() {
            let s = self.qc.open_uni().await.map_err(conn_err)?;
            *w = Some(Writer::new(&self.qc, s, false));
        }
        let lw = w.as_mut().unwrap();
        lw.write(frame).await?;
        if last {
            let _ = lw.s.finish();
            *w = None;
            let mut lanes = self.lanes.lock().unwrap();
            if lanes.get(&id).is_some_and(|l| Arc::ptr_eq(l, &lane)) {
                lanes.remove(&id);
            }
        }
        Ok(())
    }

    /// Which lane a frame goes on (None: the main stream) and whether it is
    /// the lane's last frame.
    fn lane_of(&self, b: &[u8]) -> Option<(u32, bool)> {
        if b.len() < wire::HEADER_SIZE {
            return None;
        }
        let id = u32::from_le_bytes(b[1..5].try_into().unwrap());
        match b[0] {
            wire::TYPE_DATA | wire::TYPE_CHANNEL_DATA | wire::TYPE_CHANNEL_EOF => Some((id, false)),
            wire::TYPE_EOF => Some((id, true)),
            // After the channel's data, on its lane, if it has one; a
            // session's (or a channel's that sent nothing) stays on the
            // main stream.
            wire::TYPE_STREAM_END => self.lanes.lock().unwrap().contains_key(&id).then_some((id, true)),
            _ => None,
        }
    }
}

/// The negotiated ALPN, if any.
fn alpn(qc: &Connection) -> Option<String> {
    let hd = qc.handshake_data()?.downcast::<HandshakeData>().ok()?;
    hd.protocol.map(|p| String::from_utf8_lossy(&p).into_owned())
}

async fn read_main(
    mut r: Reader,
    tx: mpsc::Sender<io::Result<Vec<u8>>>,
    out: Arc<Out>,
    pongs: Arc<watch::Sender<u64>>,
    server: bool,
) {
    loop {
        match r.next().await {
            Ok(Item::Frame(f)) => {
                if tx.send(Ok(f)).await.is_err() {
                    return;
                }
            }
            Ok(Item::Ping) if server => {
                if let Err(e) = out.main.lock().await.write(&[]).await {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
            Ok(Item::Ping) => {
                pongs.send_modify(|n| *n += 1);
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

async fn accept_lanes(qc: Connection, tx: mpsc::Sender<io::Result<Vec<u8>>>) {
    // The connection's end is reported by read_main.
    while let Ok(s) = qc.accept_uni().await {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut r = Reader::new(s, false);
            // A lane ends at its last frame (or a stray ping: none come
            // here) or the connection's end.
            while let Ok(Item::Frame(f)) = r.next().await {
                if tx.send(Ok(f)).await.is_err() {
                    return;
                }
            }
        });
    }
}
