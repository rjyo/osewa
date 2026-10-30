//! The Rust carrier against coder/websocket v1.8.15, configured as osewa's
//! Go server and client (tests/gohelper). Skipped without `go`.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::{frames, go_helper, terminal_data, terminal_data_file, text, Counting};
use osewa_carrier::ws::{self, CloseFrame, Conn, Error};
use osewa_wire::MAX_FRAME;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::{spawn_local, LocalSet};

/// How a Rust server's connection ended.
#[derive(Debug)]
enum Outcome {
    Accept(Error),
    Read { compressed: bool, err: Error },
    Closed(Result<(), Error>),
}

/// Serves one connection as the interop peer: echoes, and answers
/// "ping-me" (ping, then "pinged") and "close-me" (close 4000).
async fn serve(conn: Conn<TcpStream>) -> Outcome {
    let compressed = conn.compressed();
    loop {
        match conn.read().await {
            Ok(m) if m == b"ping-me" => {
                let c = conn.clone();
                spawn_local(async move {
                    c.ping().await.expect("ping");
                    c.write(b"pinged").await.expect("write");
                });
            }
            Ok(m) if m == b"close-me" => {
                return Outcome::Closed(conn.close(4000, "rust bye").await);
            }
            Ok(m) => conn.write(&m).await.expect("echo"),
            Err(err) => return Outcome::Read { compressed, err },
        }
    }
}

/// Runs the Go client's test against a Rust server.
async fn go_client(helper: &std::path::Path, mode: &str, test: &str, data: bool) -> (String, Outcome) {
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ln.local_addr().unwrap().to_string();
    let server = spawn_local(async move {
        let (tcp, _) = ln.accept().await.unwrap();
        match ws::accept(tcp, true).await {
            Ok(conn) => serve(conn).await,
            Err(e) => Outcome::Accept(e),
        }
    });
    let mut cmd = Command::new(helper);
    cmd.args(["client", &addr, mode, test]);
    if data {
        cmd.arg(terminal_data_file());
    }
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output()).await.expect("go client timed out").unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success() && stdout.lines().any(|l| l == "ok"),
        "go client {mode} {test}: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (stdout, server.await.unwrap())
}

fn ratio_line(stdout: &str) -> (f64, u64, u64) {
    let l = stdout.lines().find_map(|l| l.strip_prefix("ratio ")).expect("ratio line");
    let v: Vec<u64> = l.split(' ').map(|x| x.parse().unwrap()).collect();
    (v[0] as f64 / v[1] as f64, v[0], v[1])
}

#[tokio::test]
async fn go_client_rust_server() {
    let Some(helper) = go_helper() else { return };
    LocalSet::new()
        .run_until(async {
            // Echo: every size, the terminal stream through the shared
            // deflate context, pings both ways, a clean close from Go.
            for (mode, compressed) in [("osewa", true), ("ctx", true), ("noctx", true), ("none", false)] {
                let (stdout, outcome) = go_client(&helper, mode, "echo", true).await;
                match outcome {
                    Outcome::Read { compressed: c, err: Error::Close(CloseFrame { code: 1000, reason }) } => {
                        assert_eq!(c, compressed, "{mode}");
                        assert_eq!(reason, "done");
                    }
                    o => panic!("{mode}: {o:?}"),
                }
                let (r, payload, wire) = ratio_line(&stdout);
                eprintln!("go client ({mode}) <- rust server: {payload} bytes as {wire} on the wire, {r:.2}x");
                if compressed {
                    assert!(r > 3.0, "{mode}: {r}");
                }
            }

            let (_, o) = go_client(&helper, "osewa", "text", false).await;
            assert!(matches!(o, Outcome::Read { err: Error::NotBinary, .. }), "{o:?}");

            let (_, o) = go_client(&helper, "osewa", "big", false).await;
            assert!(matches!(o, Outcome::Read { err: Error::TooBig(1048577), .. }), "{o:?}");

            let (_, o) = go_client(&helper, "osewa", "nosub", false).await;
            assert!(matches!(o, Outcome::Accept(Error::Handshake(_))), "{o:?}");

            let (_, o) = go_client(&helper, "osewa", "close-me", false).await;
            assert!(matches!(o, Outcome::Closed(Ok(()))), "{o:?}");
        })
        .await;
}

/// A Go server (tests/gohelper "server") and the lines it prints.
struct GoServer {
    addr: String,
    lines: mpsc::UnboundedReceiver<String>,
    _child: tokio::process::Child,
}

impl GoServer {
    async fn start(helper: &std::path::Path, mode: &str) -> GoServer {
        let mut child = Command::new(helper)
            .args(["server", mode])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = out.next_line().await.unwrap().unwrap();
        let addr = first.strip_prefix("addr ").unwrap().to_string();
        let (tx, lines) = mpsc::unbounded_channel();
        spawn_local(async move {
            while let Ok(Some(l)) = out.next_line().await {
                let _ = tx.send(l);
            }
        });
        GoServer { addr, lines, _child: child }
    }

    async fn line(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(10), self.lines.recv()).await.expect("go server line").unwrap()
    }

    async fn dial(&self) -> (Conn<Counting<TcpStream>>, std::rc::Rc<std::cell::Cell<u64>>) {
        let tcp = TcpStream::connect(&self.addr).await.unwrap();
        let (s, read) = Counting::new(tcp);
        (ws::dial(s, &self.addr).await.unwrap(), read)
    }
}

async fn round_trip<S: tokio::io::AsyncRead + tokio::io::AsyncWrite>(conn: &Conn<S>, m: &[u8]) {
    conn.write(m).await.unwrap();
    assert_eq!(conn.read().await.unwrap(), m, "echo of {} bytes", m.len());
}

#[tokio::test]
async fn rust_client_go_server() {
    let Some(helper) = go_helper() else { return };
    LocalSet::new()
        .run_until(async {
            let mut go = GoServer::start(&helper, "ctx").await;

            // 1: echo, the terminal stream, pings both ways, a clean close.
            let (conn, read) = go.dial().await;
            assert!(conn.compressed());
            assert_eq!(conn.subprotocol(), "osewa.v0");
            for n in [0, 1, 127, 128, 129, 511, 512, 4096, 65535, 65536, 300_000, MAX_FRAME] {
                round_trip(&conn, &text(n, n)).await;
            }
            for i in 0..500 {
                round_trip(&conn, &text(200 + i % 50, i)).await;
            }
            let before = read.get();
            let mut payload = 0;
            for f in frames(&terminal_data()) {
                round_trip(&conn, &f).await;
                payload += f.len();
            }
            let wire = read.get() - before;
            let r = payload as f64 / wire as f64;
            eprintln!("rust client <- go server: {payload} bytes as {wire} on the wire, {r:.2}x");
            assert!(r > 3.0);
            // Our ping needs Go's reader; Go's needs ours.
            let c = conn.clone();
            let reader = spawn_local(async move { c.read().await });
            conn.ping().await.unwrap();
            conn.write(b"ping-me").await.unwrap();
            assert_eq!(reader.await.unwrap().unwrap(), b"pinged");
            conn.close(1000, "done").await.unwrap();
            let l = go.line().await;
            assert!(l.starts_with("end 1 1000 "), "{l}");

            // 2: Go closes.
            let (conn, _) = go.dial().await;
            conn.write(b"close-me").await.unwrap();
            let e = conn.read().await.unwrap_err();
            assert!(matches!(e, Error::Close(CloseFrame { code: 4001, ref reason }) if reason == "go bye"), "{e}");
            assert_eq!(go.line().await, "closed 2 <nil>");

            // 3: Go sends text; we close 1003.
            let (conn, _) = go.dial().await;
            conn.write(b"text-me").await.unwrap();
            assert!(matches!(conn.read().await, Err(Error::NotBinary)));
            let l = go.line().await;
            assert!(l.starts_with("end 3 1003 "), "{l}");

            // 4: over Go's read limit.
            let (conn, _) = go.dial().await;
            let c = conn.clone();
            let w = spawn_local(async move { c.write(&vec![0; MAX_FRAME + 1]).await });
            let e = conn.read().await.unwrap_err();
            assert_eq!(e.close_status(), Some(1009), "{e}");
            let _ = w.await.unwrap();
            let l = go.line().await;
            assert!(l.starts_with("end 4 -1 ") && l.contains("read limited at 1048577 bytes"), "{l}");
        })
        .await;
}

#[tokio::test]
async fn rust_client_go_server_no_compress() {
    let Some(helper) = go_helper() else { return };
    LocalSet::new()
        .run_until(async {
            let mut go = GoServer::start(&helper, "none").await;
            let (conn, _) = go.dial().await;
            assert!(!conn.compressed());
            for i in 0..100 {
                round_trip(&conn, &text(100 + i * 37, i)).await;
            }
            conn.close(1000, "").await.unwrap();
            let l = go.line().await;
            assert!(l.starts_with("end 1 1000 "), "{l}");
        })
        .await;
}

#[tokio::test]
async fn rust_client_go_server_without_subprotocol() {
    let Some(helper) = go_helper() else { return };
    LocalSet::new()
        .run_until(async {
            let mut go = GoServer::start(&helper, "nosub").await;
            let tcp = TcpStream::connect(&go.addr).await.unwrap();
            let e = ws::dial(tcp, &go.addr).await.unwrap_err();
            assert!(matches!(e, Error::Handshake(_)), "{e}");
            let l = go.line().await;
            assert!(l.starts_with("end 1 1002 "), "{l}");
        })
        .await;
}
