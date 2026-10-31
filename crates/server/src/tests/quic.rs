//! The server on the QUIC carrier (docs/QUIC.md): WELCOME's carriers,
//! sessions, channel streams on lanes, compression by ALPN, auth, close;
//! and a smoke comparison with the WebSocket on loopback.

use std::time::{Duration, Instant};

use osewa_carrier::quic;
use osewa_wire as wire;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::helpers::*;

fn conn(c: &Tc) -> &quic::Conn {
    match &c.peer {
        Transport::Quic(q) => q,
        _ => unreachable!("not a QUIC client"),
    }
}

// QUIC on the TCP port's number, ALPN osewa.v0 listed, as docs/QUIC.md
// has v0 servers advertise it.
#[test]
fn welcome_advertises_quic() {
    run(async {
        let mut e = Env::new();
        e.listen().await;
        e.listen_quic().await;
        let port = e.addr.unwrap().port();
        assert_eq!(e.quic_addr.unwrap().port(), port);
        let want = vec![wire::Carrier { carrier: "quic".into(), port: port.into(), alpn: vec!["osewa.v0".into()] }];
        let c = e.connect_ws("phone").await;
        assert_eq!(c.welcome.carriers, want);
        let json = serde_json::to_string(&c.welcome.carriers).unwrap();
        assert_eq!(json, format!(r#"[{{"carrier":"quic","port":{port},"alpn":["osewa.v0"]}}]"#));
        let c = e.connect_quic("phone").await;
        assert_eq!(c.welcome.carriers, want);
    });
}

#[test]
fn session_over_quic() {
    run(async {
        let mut e = Env::new();
        e.listen_quic().await;
        let mut c = e.connect_quic("phone").await;
        assert_eq!(conn(&c).alpn(), quic::ALPN_DEFLATE);
        let id = c.create(&["/bin/cat"]).await;
        assert!(c.attach(&id, 1, true).await.error.is_empty());
        c.input(1, 1, "over quic\r").await;
        c.output(1, "over quic\r\nover quic").await;
        // A resume on a new connection, as after a network change.
        c.hang_up();
        let mut c = e.connect_quic("phone").await;
        assert!(c.attach(&id, 1, true).await.error.is_empty());
        c.input(1, 2, "again\r").await;
        c.output(1, "again\r\nagain").await;
        assert_eq!(c.list().await.sessions.len(), 1);
    });
}

#[test]
fn no_compress_takes_plain_alpn() {
    run(async {
        let mut e = Env::with(|c| c.no_compress = true);
        e.listen_quic().await;
        let mut c = e.connect_quic("phone").await;
        assert_eq!(conn(&c).alpn(), quic::ALPN);
        let id = c.create(&["/bin/cat"]).await;
        assert!(c.attach(&id, 1, true).await.error.is_empty());
        c.input(1, 1, "plain\r").await;
        c.output(1, "plain\r\nplain").await;
    });
}

// exec and forward data travel on lanes; more than a window of credit
// moves under the protocol's CREDIT rules.
#[test]
fn channels_on_lanes() {
    run(async {
        let mut e = Env::new();
        e.listen_quic().await;
        let mut c = e.connect_quic("phone").await;
        c.lazy_channel = true;
        let r = c.exec(1, &["head", "-c", "10000000", "/dev/zero"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        let r = c.drain(1).await;
        assert_eq!(r.data[1].len(), 10_000_000);
        assert!(r.end.reason == wire::END_EXITED && r.end.exit_code == Some(0), "end {:?}", r.end);

        let r = c.exec(3, &["wc", "-c"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        for _ in 0..100 {
            c.data(3, &[b'x'; 10_000]).await;
        }
        c.eof(3).await;
        let r = c.drain(3).await;
        assert_eq!(r.out(1).trim(), "1000000");

        let (port, _l) = listen(|mut conn| async move {
            let mut b = [0u8; 4096];
            while let Ok(n) = conn.read(&mut b).await {
                if n == 0 || conn.write_all(&b[..n]).await.is_err() {
                    break;
                }
            }
        })
        .await;
        let r = c.forward(5, port).await;
        assert!(r.error.is_empty(), "forward.open {r:?}");
        c.data(5, b"through a lane").await;
        c.eof(5).await;
        let r = c.drain(5).await;
        assert_eq!(r.out(1), "through a lane");
    });
}

#[test]
fn unenrolled_device_refused() {
    run(async {
        let mut e = Env::new();
        e.listen_quic().await;
        let stranger = osewa_auth::Identity::generate();
        // TLS 1.3 lets the client finish first; the refusal ends the
        // connection before any WELCOME.
        if let Ok(mut c) = e.dial_quic(&stranger).await {
            let hello = wire::Hello { client_id: "x".into(), ..Default::default() };
            let _ = c.try_send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
            assert!(matches!(c.read(Duration::from_secs(5)).await, Err(ReadErr::Closed)), "an unenrolled device got in");
        }
    });
}

#[test]
fn close_codes_reach_the_client() {
    run(async {
        let mut e = Env::new();
        e.listen_quic().await;
        // A first frame that isn't HELLO: policy violation.
        let c = e.dial_quic(&e.device).await.unwrap();
        c.send(wire::Frame::new(wire::TYPE_ACK, 1, vec![0; 8])).await;
        let err = tokio::time::timeout(WAIT, conn(&c).read()).await.unwrap().unwrap_err();
        assert_eq!(quic::close_status(&err).map(|s| s.0), Some(crate::conn::CLOSE_POLICY_VIOLATION), "{err}");
        // The server closing: going away.
        let c = e.connect_quic("phone").await;
        e.srv.close().await;
        let err = tokio::time::timeout(WAIT, conn(&c).read()).await.unwrap().unwrap_err();
        assert_eq!(quic::close_status(&err), Some((crate::conn::CLOSE_GOING_AWAY, "server closing".into())), "{err}");
    });
}

/// Download rate (MB/s) of n random bytes from exec, and the median
/// keystroke echo of a /bin/cat session, on c.
async fn smoke(c: &mut Tc, n: usize) -> (f64, Duration) {
    c.lazy_channel = true;
    let t0 = Instant::now();
    let r = c.exec(9, &["head", "-c", &n.to_string(), "/dev/urandom"]).await;
    assert!(r.error.is_empty(), "exec.start {r:?}");
    let r = c.drain(9).await;
    assert_eq!(r.data[1].len(), n);
    let rate = n as f64 / t0.elapsed().as_secs_f64() / 1e6;

    let id = c.create(&["/bin/cat"]).await;
    assert!(c.attach(&id, 11, true).await.error.is_empty());
    let mut echo = Vec::new();
    for i in 0..40 {
        let t = Instant::now();
        let key = format!("k{i}.");
        c.input(11, i + 1, &key).await;
        c.output(11, &key).await;
        echo.push(t.elapsed());
    }
    echo.sort();
    (rate, echo[echo.len() / 2])
}

// Not a benchmark: loopback, a debug build, one run. It catches a carrier
// that is grossly slower than the WebSocket beside it. Random bytes go over
// the WebSocket uncompressed, at loopback speed, while a debug build's
// QUIC pays per packet: 8x is still far from gross.
#[test]
fn smoke_quic_against_websocket() {
    run(async {
        let mut e = Env::new();
        e.listen().await;
        e.listen_quic().await;
        const N: usize = 32 << 20;
        let mut ws = e.dial_ws(&e.device).await.unwrap();
        let hello = wire::Hello { client_id: "ws".into(), features: vec![wire::FEATURE_EXEC.into()], ..Default::default() };
        ws.send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
        assert_eq!(ws.next().await.ty, wire::TYPE_WELCOME);
        let (ws_rate, ws_echo) = smoke(&mut ws, N).await;
        let (q_rate, q_echo) = smoke(&mut e.connect_quic("quic").await, N).await;
        eprintln!("loopback smoke: download WS {ws_rate:.1} MB/s, QUIC {q_rate:.1} MB/s; echo p50 WS {ws_echo:?}, QUIC {q_echo:?}");
        assert!(q_rate * 8.0 >= ws_rate, "QUIC download {q_rate:.1} MB/s against WebSocket {ws_rate:.1}");
        assert!(q_echo < Duration::from_millis(50), "QUIC echo p50 {q_echo:?}");
    });
}
