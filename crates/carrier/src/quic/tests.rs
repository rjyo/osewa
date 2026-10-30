//! A Rust client and server over loopback QUIC; the Go peers are in
//! crates/osewa/tests/go_quic.rs.

use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use osewa_auth::{Device, Devices, Identity};
use osewa_wire as wire;

use super::*;

struct Keys {
    server: Identity,
    device: Identity,
    devices: Devices,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn keys() -> Keys {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("osewa-quic-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (server, device) = (Identity::generate(), Identity::generate());
    let devices = Devices::new(dir.join("devices"));
    devices.add(Device { name: "phone".into(), key: osewa_auth::encode_key(&device.public()) }).unwrap();
    Keys { server, device, devices, _dir: TempDir(dir) }
}

/// A server endpoint on loopback, accepting connections (quinn holds a
/// handshake until its Incoming is accepted) and their main streams.
fn listen(k: &Keys, compress: bool, cc: Congestion) -> (Endpoint, mpsc::Receiver<Conn>) {
    let cfg = server_config(osewa_auth::server_config(&k.server, &k.devices), compress, cc).unwrap();
    let ep = endpoint(UdpSocket::bind("127.0.0.1:0").unwrap(), Some(cfg)).unwrap();
    (ep.clone(), serve(ep))
}

fn serve(ep: Endpoint) -> mpsc::Receiver<Conn> {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        while let Some(inc) = ep.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Ok(qc) = inc.await {
                    if let Ok(c) = accept(qc).await {
                        let _ = tx.send(c).await;
                    }
                }
            });
        }
    });
    rx
}

async fn dial_as(ep: &Endpoint, k: &Keys, who: &Identity, cc: Congestion) -> io::Result<Conn> {
    let addr = format!("127.0.0.1:{}", ep.local_addr().unwrap().port());
    dial(&addr, osewa_auth::client_config(who, k.server.public()), cc).await
}

/// A connected pair: (server's end, client's end). The client's first
/// frame opens the main stream on the server.
async fn pair_with(compress: bool, cc: Congestion) -> (Conn, Conn, Endpoint, Keys) {
    let k = keys();
    let (ep, mut conns) = listen(&k, compress, cc);
    let cli = dial_as(&ep, &k, &k.device, cc).await.unwrap();
    cli.write(&frame(wire::TYPE_HELLO, 0, b"{}")).await.unwrap();
    let srv = conns.recv().await.unwrap();
    assert_eq!(read(&srv).await, frame(wire::TYPE_HELLO, 0, b"{}"));
    (srv, cli, ep, k)
}

async fn pair(compress: bool) -> (Conn, Conn, Endpoint, Keys) {
    pair_with(compress, Congestion::Cubic).await
}

fn frame(ty: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![ty];
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(payload);
    b
}

fn text(n: usize) -> Vec<u8> {
    let line = b"drwxr-xr-x  5 osewa staff  160 Oct 30 12:00 \x1b[1;34msrc\x1b[0m\r\n";
    line.iter().copied().cycle().take(n).collect()
}

async fn read(c: &Conn) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), c.read()).await.expect("read timed out").unwrap()
}

fn lanes(c: &Conn) -> Vec<u32> {
    let mut v: Vec<u32> = c.0.out.lanes.lock().unwrap().keys().copied().collect();
    v.sort();
    v
}

#[tokio::test]
async fn alpn_follows_compression() {
    for compress in [true, false] {
        let (srv, cli, _ep, _k) = pair(compress).await;
        let want = if compress { ALPN_DEFLATE } else { ALPN };
        assert_eq!((srv.alpn(), cli.alpn()), (want, want));
    }
}

#[tokio::test]
async fn frames_both_ways() {
    for compress in [true, false] {
        let (srv, cli, _ep, _k) = pair(compress).await;
        // Sizes around the read step and up to the frame limit.
        let sizes = [1usize, 100, 65_531, 65_536, 200_000, wire::MAX_FRAME - wire::HEADER_SIZE];
        for (i, n) in sizes.iter().enumerate() {
            let f = frame(wire::TYPE_OUTPUT, 1, &text(*n));
            srv.write(&f).await.unwrap();
            assert_eq!(read(&cli).await, f, "server frame {i} (compress {compress})");
            let f = frame(wire::TYPE_INPUT, 1, &text(*n));
            cli.write(&f).await.unwrap();
            assert_eq!(read(&srv).await, f, "client frame {i}");
        }
        // Many small ones, in order.
        for i in 0..2000u32 {
            srv.write(&frame(wire::TYPE_OUTPUT, 1, &i.to_le_bytes())).await.unwrap();
        }
        for i in 0..2000u32 {
            assert_eq!(read(&cli).await, frame(wire::TYPE_OUTPUT, 1, &i.to_le_bytes()));
        }
        assert!(srv.write(&[]).await.is_err(), "an empty frame was written");
    }
}

#[tokio::test]
async fn output_is_compressed() {
    let mut got = Vec::new();
    for compress in [true, false] {
        let (srv, cli, _ep, _k) = pair(compress).await;
        let before = cli.connection().stats().udp_rx.bytes;
        for _ in 0..64 {
            srv.write(&frame(wire::TYPE_OUTPUT, 1, &text(16 << 10))).await.unwrap();
        }
        for _ in 0..64 {
            read(&cli).await;
        }
        got.push(cli.connection().stats().udp_rx.bytes - before);
    }
    assert!(got[0] * 4 < got[1], "compressed {} bytes against {}", got[0], got[1]);
}

#[tokio::test]
async fn ping_is_answered() {
    for compress in [true, false] {
        let (srv, cli, _ep, _k) = pair(compress).await;
        // The server answers while nobody takes its frames.
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(5), cli.ping()).await.unwrap().unwrap();
        }
        // A ping is never a frame.
        cli.write(&frame(wire::TYPE_ACK, 1, b"x")).await.unwrap();
        assert_eq!(read(&srv).await, frame(wire::TYPE_ACK, 1, b"x"));
    }
}

#[tokio::test]
async fn lanes_carry_channel_data() {
    let (srv, cli, _ep, _k) = pair(true).await;
    // A session's STREAM_END has no lane; it stays on the main stream.
    srv.write(&frame(wire::TYPE_STREAM_END, 9, b"{}")).await.unwrap();
    assert!(lanes(&srv).is_empty());
    assert_eq!(read(&cli).await, frame(wire::TYPE_STREAM_END, 9, b"{}"));

    cli.write(&frame(wire::TYPE_DATA, 3, b"up")).await.unwrap();
    assert_eq!(read(&srv).await, frame(wire::TYPE_DATA, 3, b"up"));
    assert_eq!(lanes(&cli), [3]);

    let cd = frame(wire::TYPE_CHANNEL_DATA, 3, &text(100_000));
    let (eof, end) = (frame(wire::TYPE_CHANNEL_EOF, 3, &[1]), frame(wire::TYPE_STREAM_END, 3, b"{}"));
    srv.write(&cd).await.unwrap();
    assert_eq!(lanes(&srv), [3]);
    srv.write(&eof).await.unwrap();
    srv.write(&end).await.unwrap();
    // STREAM_END went on the lane and ended it.
    assert!(lanes(&srv).is_empty());
    assert_eq!(read(&cli).await, cd);
    assert_eq!(read(&cli).await, eof);
    assert_eq!(read(&cli).await, end);
    // The client's DATA lane goes at its channel's end.
    assert!(lanes(&cli).is_empty());

    // The client's EOF is its lane's last frame.
    cli.write(&frame(wire::TYPE_DATA, 5, b"a")).await.unwrap();
    cli.write(&frame(wire::TYPE_EOF, 5, b"")).await.unwrap();
    assert!(lanes(&cli).is_empty());
    assert_eq!(read(&srv).await, frame(wire::TYPE_DATA, 5, b"a"));
    assert_eq!(read(&srv).await, frame(wire::TYPE_EOF, 5, b""));
}

// Lanes and the main stream at once: each stream's frames arrive whole and
// in order.
#[tokio::test]
async fn many_lanes_interleaved() {
    let (srv, cli, _ep, _k) = pair(true).await;
    let srv2 = srv.clone();
    let writer = tokio::spawn(async move {
        for round in 0..20u32 {
            for id in 10..30u32 {
                srv2.write(&frame(wire::TYPE_CHANNEL_DATA, id, &round.to_le_bytes())).await.unwrap();
            }
            srv2.write(&frame(wire::TYPE_OUTPUT, 1, &round.to_le_bytes())).await.unwrap();
        }
    });
    let mut next = HashMap::<u32, u32>::new();
    for _ in 0..20 * 21 {
        let f = read(&cli).await;
        let id = u32::from_le_bytes(f[1..5].try_into().unwrap());
        let round = u32::from_le_bytes(f[5..9].try_into().unwrap());
        let want = next.entry(id).or_default();
        assert_eq!(round, *want, "stream {id} out of order");
        *want += 1;
    }
    writer.await.unwrap();
}

#[tokio::test]
async fn close_carries_status() {
    let (srv, cli, _ep, _k) = pair(true).await;
    srv.write(&frame(wire::TYPE_RESPONSE, 0, b"last")).await.unwrap();
    assert_eq!(read(&cli).await, frame(wire::TYPE_RESPONSE, 0, b"last"));
    srv.close(1008, "expected HELLO");
    assert!(srv.read().await.is_err(), "readable after close");
    assert!(srv.write(&frame(wire::TYPE_RESPONSE, 0, b"")).await.is_err(), "writable after close");
    let e = tokio::time::timeout(Duration::from_secs(5), cli.read()).await.unwrap().unwrap_err();
    assert_eq!(close_status(&e), Some((1008, "expected HELLO".into())), "{e}");
    assert!(cli.write(&frame(wire::TYPE_INPUT, 1, b"")).await.is_err());
}

// Dropping every clone closes the connection.
#[tokio::test]
async fn drop_closes() {
    let (srv, cli, _ep, _k) = pair(false).await;
    drop(cli);
    let e = tokio::time::timeout(Duration::from_secs(5), srv.read()).await.unwrap().unwrap_err();
    assert_eq!(close_status(&e), Some((0, String::new())), "{e}");
}

#[tokio::test]
async fn unenrolled_device_refused() {
    let k = keys();
    let (ep, mut conns) = listen(&k, true, Congestion::Cubic);
    let stranger = Identity::generate();
    let r = tokio::time::timeout(Duration::from_secs(5), dial_as(&ep, &k, &stranger, Congestion::Cubic));
    // In TLS 1.3 the client is done before the server checks its
    // certificate, so the refusal may come only after the dial.
    if let Ok(c) = r.await.expect("dial hung") {
        c.write(&frame(wire::TYPE_HELLO, 0, b"{}")).await.ok();
        let e = tokio::time::timeout(Duration::from_secs(5), c.read()).await.expect("read hung");
        assert!(e.is_err(), "an unenrolled device connected");
    }
    assert!(conns.try_recv().is_err(), "the server took an unenrolled device");
    // And a client that pins another key refuses the server.
    let addr = format!("127.0.0.1:{}", ep.local_addr().unwrap().port());
    let tls = osewa_auth::client_config(&k.device, Identity::generate().public());
    let r = tokio::time::timeout(Duration::from_secs(5), dial(&addr, tls, Congestion::Cubic));
    assert!(r.await.expect("dial hung").is_err(), "connected to an unpinned server");
}

// A dial that starts while nothing answers keeps racing new attempts, so
// it connects soon after the server appears, not after a backoff.
#[tokio::test]
async fn dial_races_attempts() {
    let k = keys();
    let addr = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let tls = osewa_auth::client_config(&k.device, k.server.public());
    let dialing = tokio::spawn(async move { dial(&addr.to_string(), tls, Congestion::Cubic).await });
    tokio::time::sleep(Duration::from_millis(1700)).await;
    let cfg = server_config(osewa_auth::server_config(&k.server, &k.devices), true, Congestion::Cubic).unwrap();
    let mut conns = serve(endpoint(UdpSocket::bind(addr).unwrap(), Some(cfg)).unwrap());
    let t0 = std::time::Instant::now();
    let cli = tokio::time::timeout(Duration::from_secs(5), dialing).await.expect("dial did not race").unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_millis(900), "connected after {:?}", t0.elapsed());
    cli.write(&frame(wire::TYPE_HELLO, 0, b"{}")).await.unwrap();
    let srv = conns.recv().await.unwrap();
    assert_eq!(read(&srv).await, frame(wire::TYPE_HELLO, 0, b"{}"));
}

#[test]
fn congestion_names() {
    assert_eq!(Congestion::parse("bbr"), Some(Congestion::Bbr));
    assert_eq!(Congestion::parse("cubic"), Some(Congestion::Cubic));
    assert_eq!(Congestion::parse("newreno"), Some(Congestion::NewReno));
    assert_eq!(Congestion::parse("hystart"), None);
    assert_eq!(Congestion::default(), Congestion::Cubic);
}

#[tokio::test]
async fn every_controller_carries_frames() {
    for cc in [Congestion::Cubic, Congestion::Bbr, Congestion::NewReno] {
        let (srv, cli, _ep, _k) = pair_with(true, cc).await;
        let f = frame(wire::TYPE_CHANNEL_DATA, 3, &text((1 << 20) - 8));
        for _ in 0..8 {
            srv.write(&f).await.unwrap();
        }
        for _ in 0..8 {
            assert_eq!(read(&cli).await, f, "{cc:?}");
        }
    }
}

#[test]
fn socket_buffers_grow() {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    grow_buffers(&sock);
    let (mut v, mut l): (libc::c_int, libc::socklen_t) = (0, size_of::<libc::c_int>() as _);
    unsafe { libc::getsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, (&mut v as *mut libc::c_int).cast(), &mut l) };
    // Past every system's default, whatever its cap.
    assert!(v as usize >= 1 << 20, "receive buffer {v}");
}

/// A raw quinn client on a pair's server, for what Conn hides: when bytes
/// arrive on which stream. transport_tweak adjusts its transport parameters.
async fn raw_client(
    compress: bool,
    transport_tweak: impl FnOnce(&mut TransportConfig),
) -> (Conn, Connection, quinn::SendStream, quinn::RecvStream, Endpoint, Keys) {
    let k = keys();
    let (ep, mut conns) = listen(&k, compress, Congestion::Cubic);
    let mut cfg = client_config(osewa_auth::client_config(&k.device, k.server.public()), Congestion::Cubic).unwrap();
    let mut t = transport(Congestion::Cubic);
    transport_tweak(&mut t);
    cfg.transport_config(Arc::new(t));
    let cep = endpoint(UdpSocket::bind("127.0.0.1:0").unwrap(), None).unwrap();
    let qc = cep.connect_with(cfg, ep.local_addr().unwrap(), "localhost").unwrap().await.unwrap();
    let (mut send, recv) = qc.open_bi().await.unwrap();
    let hello = frame(wire::TYPE_HELLO, 0, b"{}");
    send.write_all(&(hello.len() as u32).to_be_bytes()).await.unwrap();
    send.write_all(&hello).await.unwrap();
    let srv = conns.recv().await.unwrap();
    assert_eq!(read(&srv).await, hello);
    (srv, qc, send, recv, cep, k)
}

/// Splits length-prefixed frames.
fn split(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let n = u32::from_be_bytes(b[..4].try_into().unwrap()) as usize;
        out.push(b[4..4 + n].to_vec());
        b = &b[4 + n..];
    }
    out
}

// A channel's end goes out before what the main stream carries after it:
// once the stream.close RESPONSE has arrived, the lane's data and its
// STREAM_END are all there (no waiting on the network for them).
#[tokio::test]
async fn lane_end_goes_before_later_main_frames() {
    for _ in 0..5 {
        let (srv, qc, _send, mut recv, _cep, _k) = raw_client(false, |_| {}).await;
        let data = frame(wire::TYPE_CHANNEL_DATA, 3, &text(30_000));
        let end = frame(wire::TYPE_STREAM_END, 3, b"{}");
        let resp = frame(wire::TYPE_RESPONSE, 0, br#"{"id":7}"#);
        let (srv2, resp2) = (srv.clone(), resp.clone());
        let writer = tokio::spawn(async move {
            let resp = resp2;
            for _ in 0..20 {
                srv2.write(&data).await.unwrap();
            }
            srv2.write(&end).await.unwrap();
            srv2.write(&resp).await.unwrap();
        });
        let mut got = vec![0u8; 4 + resp.len()];
        tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut got)).await.unwrap().unwrap();
        assert_eq!(&got[4..], &resp[..]);
        // Everything the lane carried was received before the RESPONSE.
        let mut lane = tokio::time::timeout(Duration::ZERO, qc.accept_uni()).await.expect("no lane yet").unwrap();
        let b = tokio::time::timeout(Duration::ZERO, lane.read_to_end(1 << 30)).await.expect("lane data still on its way").unwrap();
        let frames = split(&b);
        assert_eq!(frames.len(), 21);
        assert_eq!(frames.last().unwrap(), &frame(wire::TYPE_STREAM_END, 3, b"{}"));
        writer.await.unwrap();
    }
}

/// A UDP relay to server that stops passing the server's packets on
/// once the flag is set, as a link that has gone dark: nothing the server
/// sends is acknowledged after that.
async fn blackhole_relay(server: SocketAddr) -> (SocketAddr, Arc<AtomicBool>) {
    let sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = sock.local_addr().unwrap();
    let dark = Arc::new(AtomicBool::new(false));
    let d = dark.clone();
    tokio::spawn(async move {
        let mut client = None;
        let mut b = vec![0u8; 65536];
        while let Ok((n, from)) = sock.recv_from(&mut b).await {
            if from == server {
                if let Some(c) = client.filter(|_| !d.load(Ordering::Relaxed)) {
                    let _ = sock.send_to(&b[..n], c).await;
                }
            } else {
                client = Some(from);
                let _ = sock.send_to(&b[..n], server).await;
            }
        }
    });
    (addr, dark)
}

// quinn holds little written but unsent: the backlog stays with the
// caller (the server's send queue, where control frames go first), as
// TCP_NOTSENT_LOWAT keeps it out of a WebSocket's socket.
#[tokio::test]
async fn unsent_is_bounded() {
    let k = keys();
    // Uncompressed, so bytes written are bytes on the wire.
    let (ep, mut conns) = listen(&k, false, Congestion::Cubic);
    let (relay, dark) = blackhole_relay(ep.local_addr().unwrap()).await;
    let tls = osewa_auth::client_config(&k.device, k.server.public());
    let cli = dial(&relay.to_string(), tls, Congestion::Cubic).await.unwrap();
    cli.write(&frame(wire::TYPE_HELLO, 0, b"{}")).await.unwrap();
    let srv = conns.recv().await.unwrap();
    read(&srv).await;
    dark.store(true, Ordering::Relaxed);

    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (srv2, acc) = (srv.clone(), accepted.clone());
    let f = frame(wire::TYPE_OUTPUT, 1, &text(16 << 10));
    let writer = tokio::spawn(async move {
        for _ in 0..256 {
            srv2.write(&f).await.unwrap();
            acc.fetch_add(f.len(), Ordering::Relaxed);
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let n = accepted.load(Ordering::Relaxed);
    let cwnd = srv.connection().stats().path.cwnd as usize;
    // In flight (at most cwnd), LOWAT, and the frame in hand.
    let bound = cwnd + frames::LOWAT as usize + 4 + wire::HEADER_SIZE + (16 << 10);
    assert!(n <= bound, "quinn took {n} bytes for a dark link (bound {bound}, cwnd {cwnd})");
    writer.abort();
}

// A dial dropped by its caller stops sending: no attempt outlives it.
#[tokio::test]
async fn abandoned_dial_stops() {
    let k = keys();
    let sink = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sink.local_addr().unwrap().to_string();
    let tls = osewa_auth::client_config(&k.device, k.server.public());
    // Long enough for three attempts.
    let r = tokio::time::timeout(Duration::from_millis(1300), dial(&addr, tls, Congestion::Cubic)).await;
    assert!(r.is_err(), "dialed a silent socket");
    let mut b = [0u8; 2048];
    // What was sent before the drop, and the closes it sent.
    let drain = tokio::time::Instant::now() + Duration::from_millis(200);
    while tokio::time::timeout_at(drain, sink.recv_from(&mut b)).await.is_ok() {}
    let mut after = 0;
    let until = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::timeout_at(until, sink.recv_from(&mut b)).await.is_ok() {
        after += 1;
    }
    assert_eq!(after, 0, "{after} datagrams from a dropped dial");
}

#[test]
fn address_as_go_picks_it() {
    let (v4, v6): (SocketAddr, SocketAddr) = ("192.0.2.1:7322".parse().unwrap(), "[2001:db8::1]:7322".parse().unwrap());
    assert_eq!(pick("host.example", &[v6, v4]), Some(v4), "a name's IPv4 address first");
    assert_eq!(pick("host.example", &[v6]), Some(v6), "else its first");
    assert_eq!(pick("2001:db8::1", &[v6]), Some(v6), "an IPv6 literal's own");
    assert_eq!(pick("192.0.2.1", &[v4]), Some(v4));
    assert_eq!(pick("host.example", &[]), None);
}

/// A UDP relay to server delaying each packet one_way: a far link with
/// no bandwidth limit of its own.
async fn delay_relay(server: SocketAddr, one_way: Duration) -> SocketAddr {
    let sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut client = None;
        let mut b = vec![0u8; 65536];
        while let Ok((n, from)) = sock.recv_from(&mut b).await {
            let to = if from == server {
                let Some(c) = client else { continue };
                c
            } else {
                client = Some(from);
                server
            };
            let (s, p, at) = (sock.clone(), b[..n].to_vec(), tokio::time::Instant::now() + one_way);
            tokio::spawn(async move {
                tokio::time::sleep_until(at).await;
                let _ = s.send_to(&p, to).await;
            });
        }
    });
    addr
}

// Bounding what quinn holds unsent mustn't cap a download on a far link,
// where in flight is megabytes. Not a benchmark (run it alone, release):
// cargo test --release -p osewa-carrier far_link -- --ignored --nocapture
// 66 MB at 120 ms took 2.5 s either way when it was written (26 MB/s
// bounded, 26-28 unbounded; mostly slow start).
#[tokio::test]
#[ignore]
async fn far_link_throughput() {
    let k = keys();
    let (ep, mut conns) = listen(&k, false, Congestion::from_env());
    let relay = delay_relay(ep.local_addr().unwrap(), Duration::from_millis(60)).await;
    let tls = osewa_auth::client_config(&k.device, k.server.public());
    let cli = dial(&relay.to_string(), tls, Congestion::Cubic).await.unwrap();
    cli.write(&frame(wire::TYPE_HELLO, 0, b"{}")).await.unwrap();
    let srv = conns.recv().await.unwrap();
    read(&srv).await;
    let f = frame(wire::TYPE_CHANNEL_DATA, 3, &[7u8; 32 << 10]);
    let n = 2000;
    let t0 = std::time::Instant::now();
    let w = tokio::spawn(async move {
        for _ in 0..n {
            srv.write(&f).await.unwrap();
        }
        srv
    });
    let mut got = 0;
    let mut marks = Vec::new();
    for i in 0..n {
        got += read(&cli).await.len();
        if (i + 1) % 250 == 0 {
            marks.push(format!("{:.0} MB at {:.2} s", got as f64 / 1e6, t0.elapsed().as_secs_f64()));
        }
    }
    let cwnd = w.await.unwrap().connection().stats().path.cwnd;
    let rate = got as f64 / t0.elapsed().as_secs_f64() / 1e6;
    eprintln!("120 ms link: {rate:.1} MB/s; {marks:?}; cwnd {cwnd}");
}
