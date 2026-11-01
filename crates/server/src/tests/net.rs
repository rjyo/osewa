//! The server on the network: pinned mutual TLS, the WebSocket upgrade and
//! compression: welcome, refused auth, a text message, running the
//! server, and compression.

use std::rc::Rc;
use std::time::Duration;

use osewa_wire as wire;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::helpers::*;
use crate::cli;

/// Upgrades by hand over TLS, offering subprotocol if given, and returns
/// the stream after the response head along with that head.
async fn raw_upgrade(e: &Env, subprotocol: Option<&str>) -> (TlsStream, String) {
    let (mut s, _) = e.tls(&e.device).await.unwrap();
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        wire::PATH
    );
    if let Some(p) = subprotocol {
        req += &format!("Sec-WebSocket-Protocol: {p}\r\n");
    }
    req += "\r\n";
    s.write_all(req.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).await.unwrap(), 1, "EOF in the response head");
        head.push(b[0]);
    }
    (s, String::from_utf8(head).unwrap())
}

/// Reads until EOF and returns the status of the first close frame.
async fn close_code(s: &mut TlsStream) -> Option<u16> {
    let mut b = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut b)).await;
    let mut i = 0;
    while i + 2 <= b.len() {
        let (op, len) = (b[i] & 0x0f, (b[i + 1] & 0x7f) as usize);
        assert!(len < 126, "a long frame in a test that expects none");
        if op == 0x8 && len >= 2 {
            return Some(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        }
        i += 2 + len;
    }
    None
}

#[test]
fn welcome_over_the_network() {
    run(async {
        let mut e = Env::new();
        e.listen().await;
        let mut c = e.connect_ws("phone").await;
        assert!(c.welcome.server == "testsrv" && c.welcome.carriers.is_empty(), "welcome {:?}", c.welcome);
        let id = c.create(&["/bin/cat"]).await;
        assert!(c.attach(&id, 1, true).await.error.is_empty());
        c.input(1, 1, "over tls\r").await;
        c.output(1, "over tls").await;
    });
}

#[test]
fn auth_rejected() {
    run(async {
        let mut e = Env::new();
        e.listen().await;
        let stranger = osewa_auth::Identity::generate();
        assert!(e.dial_ws(&stranger).await.is_err(), "unenrolled device connected");
        // Without the subprotocol the upgrade goes through and the server
        // closes at once.
        let (mut s, head) = raw_upgrade(&e, None).await;
        assert!(head.starts_with("HTTP/1.1 101"), "head {head:?}");
        assert_eq!(close_code(&mut s).await, Some(crate::conn::CLOSE_POLICY_VIOLATION));
    });
}

// A text message closes the connection: no osewa peer sends one.
#[test]
fn text_message_closes() {
    run(async {
        let mut e = Env::new();
        e.listen().await;
        let (mut s, head) = raw_upgrade(&e, Some(wire::SUBPROTOCOL)).await;
        assert!(head.starts_with("HTTP/1.1 101"), "head {head:?}");
        let mask = [1u8, 2, 3, 4];
        let mut f = vec![0x81, 0x80 | 2];
        f.extend_from_slice(&mask);
        f.extend(b"hi".iter().enumerate().map(|(i, c)| c ^ mask[i % 4]));
        s.write_all(&f).await.unwrap();
        assert_eq!(close_code(&mut s).await, Some(osewa_carrier::ws::STATUS_UNSUPPORTED_DATA));
    });
}

/// 1 MiB of log-like text: repetitive, as terminal output is, but not one
/// repeated line.
fn log_text() -> String {
    let mut b = String::new();
    let mut i = 0usize;
    while b.len() < 1 << 20 {
        b += &format!(
            "2026-10-28T12:{:02}:{:02}.{:03} INFO api request id={} path=/v1/items/{} status=200 bytes={}\r\n",
            i / 60 % 60,
            i % 60,
            i % 1000,
            100000 + i,
            i * 7 % 9973,
            512 + i % 4096
        );
        i += 1;
    }
    b.truncate(1 << 20);
    b
}

/// Answers the attach and then sends text as OUTPUT frames.
fn flood_holder(dir: &std::path::Path, id: &str, text: Rc<String>) -> FakeHolder {
    fake_holder(
        dir,
        id,
        handler(move |mut conn| {
            let text = text.clone();
            async move {
                let Ok(r) = read_request(&mut conn).await else { return };
                let resp = wire::Response { id: r.id, epoch: "e".into(), ..Default::default() };
                let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &resp)).await;
                let mut end = 0u64;
                for chunk in text.as_bytes().chunks(64 << 10) {
                    end += wire::record_size(chunk.len());
                    let _ = write_frame(&mut conn, wire::output(r.stream, end, chunk)).await;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }),
    )
}

/// Relays 1 MiB of text OUTPUT and returns how many bytes the server wrote
/// to the link meanwhile, and whether compression was negotiated.
async fn output_bytes(no_compress: bool) -> (u64, bool) {
    let mut e = Env::with(|c| c.no_compress = no_compress);
    e.listen().await;
    let text = Rc::new(log_text());
    let id = "57".repeat(16);
    let _h = flood_holder(e.dir(), &id, text.clone());
    let mut c = e.connect_ws("phone").await;
    let negotiated = matches!(&c.peer, Transport::Ws(ws) if ws.compressed());
    let link = c.link_read.clone().unwrap();
    // Counted from before the attach: the holder floods as soon as it has
    // answered.
    let before = link.get();
    assert!(c.call(wire::Request { session: id.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }).await.error.is_empty());
    let mut got = Vec::new();
    c.until(|f| {
        if f.ty == wire::TYPE_OUTPUT {
            got.extend_from_slice(wire::split_end(&f.payload).unwrap().1);
        }
        got.len() >= text.len()
    })
    .await;
    assert!(got == text.as_bytes(), "output changed in transit");
    (link.get() - before, negotiated)
}

#[test]
fn compressed_output() {
    run(async {
        let (plain, neg) = output_bytes(true).await;
        assert!(!neg, "--no-compress still negotiated permessage-deflate");
        let (packed, neg) = output_bytes(false).await;
        assert!(neg, "permessage-deflate not negotiated");
        eprintln!("1 MiB of output: {plain} bytes plain, {packed} compressed ({:.1}x)", plain as f64 / packed as f64);
        assert!(plain >= 1 << 20, "uncompressed output took only {plain} bytes");
        assert!(packed * 4 <= plain, "compressed output took {packed} bytes, plain {plain}");
    });
}

// A large, compressible response crosses compressed.
#[test]
fn compressed_response() {
    run(async {
        let mut e = Env::new();
        e.listen().await;
        let id = "58".repeat(16);
        let title = Rc::new("osewa ".repeat(20000));
        let t = title.clone();
        let _h = fake_holder(
            e.dir(),
            &id,
            handler(move |mut conn| {
                let (t, id) = (t.clone(), "58".repeat(16));
                async move {
                    let Ok(r) = read_request(&mut conn).await else { return };
                    let info = wire::SessionInfo { id, title: (*t).clone(), ..Default::default() };
                    let resp = wire::Response { id: r.id, sessions: vec![info], ..Default::default() };
                    let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &resp)).await;
                }
            }),
        );
        let mut c = e.connect_ws("phone-1").await;
        let link = c.link_read.clone().unwrap();
        let before = link.get();
        let list = c.list().await;
        assert!(list.sessions.len() == 1 && list.sessions[0].title == *title, "list {list:?}");
        let n = link.get() - before;
        assert!(n * 4 <= title.len() as u64, "a {}-byte list response took {n} bytes: not compressed", title.len());
    });
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Sets OSEWA_CONFIG_DIR and OSEWA_RUN_DIR for `osewa server` while held.
struct ServerDirs {
    _g: std::sync::MutexGuard<'static, ()>,
    _dir: TempDir,
}

/// cfg with sessions in a sandbox home, never the real one.
fn sandboxed(cfg: crate::Config) -> crate::Config {
    crate::Config { home: Some(osewa_paths::sandbox_home()), ..cfg }
}

impl ServerDirs {
    fn new() -> ServerDirs {
        let g = super::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new();
        unsafe {
            std::env::set_var("OSEWA_CONFIG_DIR", dir.path().join("config"));
            std::env::set_var("OSEWA_RUN_DIR", dir.path().join("run"));
            // cli::main reads it, as a server process would.
            std::env::set_var(osewa_paths::HOME_OVERRIDE, osewa_paths::sandbox_home());
        }
        ServerDirs { _g: g, _dir: dir }
    }
}

impl Drop for ServerDirs {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("OSEWA_CONFIG_DIR");
            std::env::remove_var("OSEWA_RUN_DIR");
            std::env::remove_var(osewa_paths::HOME_OVERRIDE);
        }
    }
}

#[test]
fn run_server() {
    let _d = ServerDirs::new();
    assert_eq!(cli::main(&args(&["--bogus"])), 2, "bad flag");
    assert_eq!(cli::main(&args(&["--listen", "256.0.0.1:x"])), 1, "bad listen address");
    run(async {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::task::spawn_local(cli::serve("127.0.0.1:0".into(), sandboxed(Default::default()), async {
            let _ = stop_rx.await;
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop_tx.send(()).unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), serving).await.expect("server did not stop");
        assert_eq!(r.unwrap(), Ok(()));
    });
}

/// Runs `osewa server` with args and reports whether a client offering
/// permessage-deflate gets it.
async fn negotiated_by(extra: &[&str]) -> bool {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let mut a = args(&["--listen", &addr]);
    a.extend(args(extra));
    let (listen, cfg) = cli::parse(&a).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::task::spawn_local(cli::serve(listen, sandboxed(cfg), async {
        let _ = stop_rx.await;
    }));
    let dev = osewa_auth::Identity::generate();
    let devs = osewa_auth::Devices::new(osewa_auth::devices_path());
    devs.add(osewa_auth::Device { name: "test".into(), key: osewa_auth::encode_key(&dev.public()) }).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(&addr).await.is_err() {
        assert!(tokio::time::Instant::now() < deadline, "server did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let srv = osewa_auth::Identity::load_or_create(&osewa_auth::identity_path()).unwrap();
    let tcp = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(osewa_auth::client_config(&dev, srv.public())));
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap().to_owned();
    let s = tls.connect(name, tcp).await.unwrap();
    let ws = osewa_carrier::ws::dial(s, "127.0.0.1").await.unwrap();
    let negotiated = ws.compressed();
    let _ = ws.close_now();
    stop_tx.send(()).unwrap();
    serving.await.unwrap().unwrap();
    negotiated
}

#[test]
fn no_compress_flag() {
    let _d = ServerDirs::new();
    run(async {
        assert!(negotiated_by(&[]).await, "osewa server doesn't compress by default");
        assert!(!negotiated_by(&["--no-compress"]).await, "--no-compress still compresses");
    });
}

// On stop the listener closes at once, while close still waits for exec
// processes.
#[test]
fn stop_refuses_new_connections() {
    run(async {
        let e = Env::with(|c| c.tune.exec_grace = Duration::from_secs(2));
        e.srv.exec_started(); // an exec that won't end
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let srv = e.srv.clone();
        let serving = tokio::task::spawn_local(async move {
            cli::serve_until(&srv, ln, async {
                let _ = stop_rx.await;
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!serving.is_finished(), "close did not wait for the exec");
        assert!(tokio::net::TcpStream::connect(addr).await.is_err(), "listener still open while closing");
        serving.await.unwrap().unwrap();
        e.srv.exec_done();
    });
}

// The TLS handshake and the upgrade share one deadline: a client that
// finishes TLS and never sends its request is dropped when it passes.
#[test]
fn handshake_and_upgrade_share_a_deadline() {
    run(async {
        let mut e = Env::with(|c| c.tune.handshake_timeout = Duration::from_millis(300));
        e.listen().await;
        let (mut s, _) = e.tls(&e.device).await.unwrap();
        let mut b = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut b)).await;
        assert!(r.is_ok(), "connection still open past the handshake deadline");
    });
}

// HELLO is decoded leniently, as v0 peers decode JSON: invalid UTF-8
// becomes U+FFFD.
#[test]
fn hello_with_invalid_utf8() {
    run(async {
        let e = Env::new();
        let mut c = e.dial();
        let mut hello = br#"{"version":0,"client_id":"phone","name":"bad "#.to_vec();
        hello.extend_from_slice(&[0xff, 0xfe]);
        hello.extend_from_slice(br#""}"#);
        c.send(wire::Frame::new(wire::TYPE_HELLO, 0, hello)).await;
        assert_eq!(c.next().await.ty, wire::TYPE_WELCOME);
    });
}

// `osewa server` serves QUIC on the TCP port's number; on stop its QUIC
// clients hear "going away" and new handshakes are refused, not left to
// time out.
#[test]
fn quic_on_stop() {
    use osewa_carrier::quic;
    let _d = ServerDirs::new();
    run(async {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let (listen, cfg) = cli::parse(&args(&["--listen", &addr])).unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::task::spawn_local(cli::serve(listen, sandboxed(cfg), async {
            let _ = stop_rx.await;
        }));
        let dev = osewa_auth::Identity::generate();
        osewa_auth::Devices::new(osewa_auth::devices_path())
            .add(osewa_auth::Device { name: "test".into(), key: osewa_auth::encode_key(&dev.public()) })
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::TcpStream::connect(&addr).await.is_err() {
            assert!(tokio::time::Instant::now() < deadline, "server did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let key = osewa_auth::Identity::load_or_create(&osewa_auth::identity_path()).unwrap().public();
        let dial = || quic::dial(&addr, osewa_auth::client_config(&dev, key), quic::Congestion::Cubic);
        let c = tokio::time::timeout(WAIT, dial()).await.unwrap().unwrap();
        let hello = wire::Hello { client_id: "q".into(), ..Default::default() };
        c.write(&wire::json(wire::TYPE_HELLO, 0, &hello).marshal()).await.unwrap();
        let f = wire::Frame::unmarshal(&tokio::time::timeout(WAIT, c.read()).await.unwrap().unwrap()).unwrap();
        let w: wire::Welcome = f.decode().unwrap();
        assert_eq!(w.carriers.first().map(|c| c.port), Some(port.into()), "carriers {:?}", w.carriers);

        stop_tx.send(()).unwrap();
        let err = tokio::time::timeout(WAIT, c.read()).await.unwrap().unwrap_err();
        assert_eq!(quic::close_status(&err), Some((crate::conn::CLOSE_GOING_AWAY, "server closing".into())), "{err}");
        // While the server drains, or once it is gone, a dial fails.
        let r = tokio::time::timeout(Duration::from_secs(3), dial()).await;
        assert!(!matches!(r, Ok(Ok(_))), "a stopped server took a QUIC connection");
        let r = tokio::time::timeout(Duration::from_secs(5), serving).await.expect("server did not stop");
        assert_eq!(r.unwrap(), Ok(()));
    });
}
