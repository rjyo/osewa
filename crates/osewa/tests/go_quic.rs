//! QUIC between the Go and Rust pieces (docs/QUIC.md): the Go osewa CLI and
//! client library (quic-go) against the Rust server, and the Rust carrier
//! (quinn) against the Go server; both ALPNs each way. Skips without a Go
//! toolchain or the Go checkout (set OSEWA_GO_DIR to point at one).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use osewa_carrier::quic;
use osewa_wire as wire;

mod common;
use common::*;

/// Forces QUIC in the Go client: no WebSocket to fall back on, so every
/// command that works went over QUIC.
fn over_quic(rig: &mut Rig) {
    rig.env.push(("OSEWA_CARRIER".into(), "quic".into()));
}

#[test]
fn go_cli_over_quic_to_rust_server() {
    let Some(go) = go_osewa() else { return };
    for extra in [&[][..], &["--no-compress"][..]] {
        let mut rig = Rig::start(None, extra);
        over_quic(&mut rig);
        eprintln!("== rust server {extra:?}");

        let id = rig.go_ok(&["new", "rs", "--", "/bin/sh", "-c", "sleep 300"]).trim().to_string();
        let ls = rig.go_ok(&["ls", "rs"]);
        assert!(ls.lines().any(|l| l.starts_with(&id) && l.contains("running")), "ls:\n{ls}");
        rig.go_ok(&["kill", "rs", &id]);

        let out = rig.go(&["exec", "rs", "--", "sh", "-c", "echo out; echo err >&2; exit 3"], b"");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert!(String::from_utf8_lossy(&out.stderr).contains("err\n"));
        // Downloads and uploads past a window of credit, on lanes.
        let out = rig.go(&["exec", "rs", "--", "head", "-c", "10000000", "/dev/urandom"], b"");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 10_000_000);
        let upload = vec![b'x'; 6_000_000];
        let out = rig.go(&["exec", "rs", "--", "wc", "-c"], &upload);
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "6000000");

        forward(&rig, go);
    }
}

/// `osewa forward` through the server to a TCP echo server.
fn forward(rig: &Rig, go: &std::path::Path) {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let tport = target.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in target.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = c;
                let mut b = [0u8; 4096];
                while let Ok(n) = c.read(&mut b) {
                    if n == 0 || c.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    let lport = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let spec = format!("127.0.0.1:{lport}:127.0.0.1:{tport}");
    let mut fwd = Command::new(go)
        .args(["forward", "rs", &spec])
        .env("OSEWA_CONFIG_DIR", &rig.client_dir)
        .envs(rig.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut conn = loop {
        match TcpStream::connect(("127.0.0.1", lport)) {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("forward did not listen: {e}"),
        }
    };
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let msg = b"through QUIC".repeat(100_000);
    let echo = conn.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        let mut echo = echo;
        let mut back = vec![0u8; 1_200_000];
        echo.read_exact(&mut back).map(|_| back)
    });
    conn.write_all(&msg).unwrap();
    assert_eq!(reader.join().unwrap().unwrap(), msg);
    let _ = fwd.kill();
    let _ = fwd.wait();
}

// The Go client library over QUIC: attach with a snapshot, echo, a resume
// on a new connection, list and kill.
#[test]
fn go_client_library_over_quic() {
    let Some(helper) = go_client_lib() else { return };
    for extra in [&[][..], &["--no-compress"][..]] {
        let rig = Rig::start(None, extra);
        let out = Command::new(helper)
            .arg(&rig.addr)
            .arg(&rig.server_key)
            .arg(rig.client_dir.join("client.key"))
            .arg("quic")
            .stderr(Stdio::inherit())
            .output()
            .unwrap();
        assert!(out.status.success() && out.stdout == b"ok\n", "goclient {extra:?}: {} {:?}", out.status, String::from_utf8_lossy(&out.stdout));
    }
}

/// A Rust QUIC client on the Go server.
struct Client {
    c: quic::Conn,
    next_id: u64,
}

impl Client {
    async fn read(&self) -> wire::Frame {
        let b = tokio::time::timeout(Duration::from_secs(10), self.c.read()).await.expect("read timed out").unwrap();
        wire::Frame::unmarshal(&b).unwrap()
    }

    async fn send(&self, f: wire::Frame) {
        self.c.write(&f.marshal()).await.unwrap();
    }

    async fn call(&mut self, mut r: wire::Request) -> wire::Response {
        self.next_id += 1;
        r.id = self.next_id;
        self.send(wire::json(wire::TYPE_REQUEST, 0, &r)).await;
        loop {
            let f = self.read().await;
            if f.ty == wire::TYPE_RESPONSE {
                let resp: wire::Response = f.decode().unwrap();
                if resp.id == r.id {
                    return resp;
                }
            }
        }
    }

    /// Reads stream's channel data until its STREAM_END, crediting it.
    async fn drain(&self, stream: u32) -> (Vec<u8>, wire::StreamEnd) {
        let mut out = Vec::new();
        loop {
            let f = self.read().await;
            if f.stream != stream {
                continue;
            }
            match f.ty {
                wire::TYPE_CHANNEL_DATA => {
                    let (ch, data) = wire::split_channel(&f.payload).unwrap();
                    if ch == 1 {
                        out.extend_from_slice(data);
                    }
                    self.send(wire::credit(stream, f.payload.len() as u32)).await;
                }
                wire::TYPE_STREAM_END => return (out, f.decode().unwrap()),
                _ => {}
            }
        }
    }
}

async fn rust_client_session(rig: &Rig, compress: bool) {
    let id = osewa_auth::Identity::load_or_create(&rig.client_dir.join("client.key")).unwrap();
    let key = osewa_auth::decode_key(&rig.server_key).unwrap();
    let dial = quic::dial(&rig.addr, osewa_auth::client_config(&id, key), quic::Congestion::Cubic);
    let c = tokio::time::timeout(Duration::from_secs(10), dial).await.expect("dial timed out").unwrap();
    assert_eq!(c.alpn(), if compress { quic::ALPN_DEFLATE } else { quic::ALPN });
    let mut c = Client { c, next_id: 0 };
    let hello = wire::Hello {
        client_id: "rust-quic".into(),
        features: vec![wire::FEATURE_EXEC.into(), wire::FEATURE_FORWARD.into()],
        ..Default::default()
    };
    c.send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
    let f = c.read().await;
    assert_eq!(f.ty, wire::TYPE_WELCOME);
    let w: wire::Welcome = f.decode().unwrap();
    let port: u16 = rig.addr.rsplit_once(':').unwrap().1.parse().unwrap();
    assert_eq!(w.carriers, vec![wire::Carrier { carrier: "quic".into(), port: port.into(), alpn: vec![quic::ALPN.into()] }]);
    tokio::time::timeout(Duration::from_secs(5), c.c.ping()).await.expect("no pong").unwrap();

    // A terminal: create, attach, echo (output on the deflated main stream).
    let r = c
        .call(wire::Request { method: wire::METHOD_CREATE.into(), cols: 80, rows: 24, argv: vec!["/bin/cat".into()], ..Default::default() })
        .await;
    assert!(r.error.is_empty(), "create {r:?}");
    let sid = r.session_id;
    let r = c
        .call(wire::Request { method: wire::METHOD_ATTACH.into(), session: sid.clone(), stream: 1, cols: 80, rows: 24, own: true, ..Default::default() })
        .await;
    assert!(r.error.is_empty(), "attach {r:?}");
    let input = wire::Input { seq: 1, ty: "text".into(), text: "hello from rust\r".into(), ..Default::default() };
    c.send(wire::json(wire::TYPE_INPUT, 1, &input)).await;
    let mut out = Vec::new();
    while !String::from_utf8_lossy(&out).contains("hello from rust\r\nhello from rust") {
        let f = c.read().await;
        if f.stream == 1 && f.ty == wire::TYPE_OUTPUT {
            out.extend_from_slice(wire::split_end(&f.payload).unwrap().1);
            c.send(wire::credit(1, f.payload.len() as u32)).await;
        }
    }

    // exec: a download on the Go server's lanes, past a window of credit.
    let r = c
        .call(wire::Request { method: wire::METHOD_EXEC_START.into(), stream: 3, argv: vec!["head".into(), "-c".into(), "10000000".into(), "/dev/urandom".into()], ..Default::default() })
        .await;
    assert!(r.error.is_empty(), "exec {r:?}");
    let (data, end) = c.drain(3).await;
    assert_eq!(data.len(), 10_000_000);
    assert!(end.reason == wire::END_EXITED && end.exit_code == Some(0), "end {end:?}");

    // And an upload on our lanes: DATA, then EOF ends the lane.
    let r = c
        .call(wire::Request { method: wire::METHOD_EXEC_START.into(), stream: 5, argv: vec!["wc".into(), "-c".into()], ..Default::default() })
        .await;
    assert!(r.error.is_empty(), "exec {r:?}");
    for _ in 0..30 {
        c.send(wire::Frame::new(wire::TYPE_DATA, 5, vec![b'y'; 10_000])).await;
    }
    c.send(wire::Frame::new(wire::TYPE_EOF, 5, Vec::new())).await;
    let (data, _) = c.drain(5).await;
    assert_eq!(String::from_utf8_lossy(&data).trim(), "300000");

    let r = c.call(wire::Request { method: wire::METHOD_KILL.into(), session: sid, ..Default::default() }).await;
    assert!(r.error.is_empty(), "kill {r:?}");
    c.c.close(1000, "");
}

#[test]
fn rust_quic_client_to_go_server() {
    let Some(go) = go_osewa() else { return };
    for extra in [&[][..], &["--no-compress"][..]] {
        let rig = Rig::start_bin(go, None, extra);
        eprintln!("== go server {extra:?}");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(rust_client_session(&rig, extra.is_empty()));
    }
}
