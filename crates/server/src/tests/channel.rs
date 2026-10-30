//! Ported from server/channel_test.go.

use std::rc::Rc;
use std::time::Duration;

use osewa_wire as wire;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::helpers::*;
use crate::channel::Channel;
use crate::client::Client;
use crate::conn::NullConn;
use crate::flight::FLIGHT_INITIAL;
use crate::queue::Class;

/// What a channel stream delivered.
#[derive(Default)]
struct ChanResult {
    /// By channel.
    data: [Vec<u8>; 3],
    /// CHANNEL_EOF channels, in order.
    eofs: Vec<u8>,
    end: wire::StreamEnd,
}

impl ChanResult {
    fn out(&self, ch: usize) -> String {
        String::from_utf8_lossy(&self.data[ch]).into_owned()
    }
}

impl Tc {
    /// Reads stream's frames until STREAM_END, crediting CHANNEL_DATA as
    /// read does, and checks that each channel's EOF comes after its data.
    async fn drain(&mut self, stream: u32) -> ChanResult {
        let mut r = ChanResult::default();
        self.until(|f| {
            if f.stream != stream {
                return false;
            }
            match f.ty {
                wire::TYPE_CHANNEL_DATA => {
                    let (ch, data) = wire::split_channel(&f.payload).unwrap();
                    assert!(f.payload.len() <= wire::MAX_CHANNEL_DATA, "CHANNEL_DATA of {} bytes", f.payload.len());
                    assert!(!r.eofs.contains(&ch), "CHANNEL_DATA {ch} after its EOF");
                    r.data[ch as usize].extend_from_slice(data);
                }
                wire::TYPE_CHANNEL_EOF => r.eofs.push(f.payload[0]),
                wire::TYPE_STREAM_END => {
                    r.end = f.decode().unwrap();
                    return true;
                }
                _ => {}
            }
            false
        })
        .await;
        r
    }

    async fn forward(&mut self, stream: u32, port: u16) -> wire::Response {
        self.call(wire::Request { host: "127.0.0.1".into(), port: port.into(), ..stream_req(wire::METHOD_FORWARD_OPEN, stream) })
            .await
    }

    async fn exec(&mut self, stream: u32, argv: &[&str]) -> wire::Response {
        self.call(wire::Request {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            ..stream_req(wire::METHOD_EXEC_START, stream)
        })
        .await
    }

    async fn data(&self, stream: u32, b: &[u8]) {
        self.send(wire::Frame::new(wire::TYPE_DATA, stream, b.to_vec())).await;
    }

    async fn eof(&self, stream: u32) {
        self.send(wire::Frame::new(wire::TYPE_EOF, stream, Vec::new())).await;
    }
}

fn random(n: usize) -> Vec<u8> {
    (0..n).map(|_| rand::random::<u8>()).collect()
}

#[test]
fn features() {
    run(async {
        let e = Env::new();
        assert_eq!(e.connect("a").await.welcome.features, ["forward", "exec"]);
        // A client that didn't list a feature can't use it.
        let mut c = e.connect_with("b", &[wire::FEATURE_EXEC]).await;
        assert_eq!(c.forward(1, 1).await.error, wire::ERR_BAD_REQUEST, "forward without the feature");

        let off = Env::with(|cfg| {
            cfg.no_forward = true;
            cfg.no_exec = true;
        });
        let mut c = off.connect("c").await;
        assert!(c.welcome.features.is_empty(), "features {:?} with --no-forward --no-exec", c.welcome.features);
        assert_eq!(c.forward(1, 1).await.error, wire::ERR_BAD_REQUEST, "forward.open with --no-forward");
        assert_eq!(c.exec(2, &["true"]).await.error, wire::ERR_BAD_REQUEST, "exec.start with --no-exec");
    });
}

#[test]
fn forward() {
    run(async {
        let (got_tx, got_rx) = tokio::sync::oneshot::channel();
        let got_tx = std::cell::RefCell::new(Some(got_tx));
        let (port, _l) = listen(move |mut conn| {
            let tx = got_tx.borrow_mut().take();
            async move {
                conn.write_all(b"hello from the host").await.unwrap();
                let mut b = Vec::new();
                let _ = conn.read_to_end(&mut b).await; // until the client's EOF
                if let Some(tx) = tx {
                    let _ = tx.send(b);
                }
            }
        })
        .await;
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.forward(3, port).await;
        assert!(r.error.is_empty(), "forward.open {r:?}");
        c.data(3, b"ping ").await;
        c.data(3, b"pong").await;
        c.eof(3).await;
        let r = c.drain(3).await;
        assert!(
            r.out(1) == "hello from the host" && r.eofs == [1] && r.end.reason == wire::END_CLOSED,
            "data {:?} eofs {:?} end {:?}",
            r.out(1),
            r.eofs,
            r.end
        );
        assert_eq!(got_rx.await.unwrap(), b"ping pong", "host got");
        // The id is free again, and the stream gone.
        let r = c.call(stream_req(wire::METHOD_STREAM_CLOSE, 3)).await;
        assert_eq!(r.error, wire::ERR_NOT_FOUND, "close after end {r:?}");
        let r = c.forward(3, port).await;
        assert!(r.error.is_empty(), "reopen {r:?}");
    });
}

#[test]
fn forward_errors() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let port = {
            let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            ln.local_addr().unwrap().port()
        };
        assert_eq!(c.forward(1, port).await.error, wire::ERR_UNREACHABLE, "closed port");
        for r in [
            wire::Request { port: 80, ..stream_req(wire::METHOD_FORWARD_OPEN, 1) },
            wire::Request { host: "localhost".into(), port: 70000, ..stream_req(wire::METHOD_FORWARD_OPEN, 1) },
            wire::Request { host: "localhost".into(), port: 80, ..req(wire::METHOD_FORWARD_OPEN) },
            stream_req(wire::METHOD_EXEC_START, 1),
        ] {
            assert_eq!(c.call(r.clone()).await.error, wire::ERR_BAD_REQUEST, "{r:?}");
        }
    });
}

// A dial that doesn't finish within the timeout is unreachable.
#[test]
fn forward_timeout() {
    run(async {
        let e = Env::with(|c| c.tune.forward_timeout = Duration::from_nanos(1));
        let mut c = e.connect("phone").await;
        let (port, _l) = listen(|conn| async move { drop(conn) }).await;
        let r = c.forward(1, port).await;
        assert!(r.error == wire::ERR_UNREACHABLE && r.message.contains("timeout"), "{r:?}");
    });
}

// The server reads the forwarded connection only as far as the client's
// credit, and resumes when credit comes back.
#[test]
fn forward_credit() {
    run(async {
        let payload = Rc::new(random(1 << 20));
        let p = payload.clone();
        let (port, _l) = listen(move |mut conn| {
            let p = p.clone();
            async move {
                let _ = conn.write_all(&p).await;
            }
        })
        .await;
        let e = Env::new();
        let mut c = e.connect("phone").await;
        c.hold_channel = true;
        let r = c.forward(1, port).await;
        assert!(r.error.is_empty(), "forward.open {r:?}");
        // Without CREDIT, the in-flight window (below the window of credit)
        // is all that may be outstanding.
        let mut got = Vec::new();
        let mut counted = 0;
        // One byte of window left can't carry data past the channel byte.
        while counted < FLIGHT_INITIAL - 1 {
            let f = c.next().await;
            if f.ty == wire::TYPE_CHANNEL_DATA {
                got.extend_from_slice(&f.payload[1..]);
                counted += f.payload.len();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 77, ..req(wire::METHOD_LIST) })).await;
        c.until(|f| {
            assert_ne!(f.ty, wire::TYPE_CHANNEL_DATA, "CHANNEL_DATA past {counted} bytes without credit");
            f.ty == wire::TYPE_RESPONSE
        })
        .await;
        assert!(counted <= FLIGHT_INITIAL, "{counted} bytes outstanding, window {FLIGHT_INITIAL}");
        c.hold_channel = false;
        c.send(wire::credit(1, counted as u32)).await;
        // STREAM_END needs both directions done.
        c.eof(1).await;
        let r = c.drain(1).await;
        got.extend_from_slice(&r.data[1]);
        assert!(got == *payload, "got {} bytes, want {} intact", got.len(), payload.len());
    });
}

// Upload: the client sends within GRANT, and the host gets every byte.
#[test]
fn forward_upload() {
    run(async {
        let payload = random(2 << 20);
        let (got_tx, got_rx) = tokio::sync::oneshot::channel();
        let got_tx = std::cell::RefCell::new(Some(got_tx));
        let (port, _l) = listen(move |mut conn| {
            let tx = got_tx.borrow_mut().take();
            async move {
                let mut b = Vec::new();
                let _ = conn.read_to_end(&mut b).await;
                if let Some(tx) = tx {
                    let _ = tx.send(b);
                }
            }
        })
        .await;
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.forward(1, port).await;
        assert!(r.error.is_empty(), "forward.open {r:?}");
        let mut window = wire::CHANNEL_WINDOW as usize;
        let mut rest = &payload[..];
        while !rest.is_empty() {
            while window < wire::MAX_CHANNEL_DATA {
                let f = c.next().await;
                if f.ty == wire::TYPE_GRANT && f.stream == 1 {
                    window += wire::parse_credit(&f.payload).unwrap() as usize;
                }
            }
            let n = rest.len().min(wire::MAX_CHANNEL_DATA);
            c.data(1, &rest[..n]).await;
            rest = &rest[n..];
            window -= n;
        }
        c.eof(1).await;
        let b = got_rx.await.unwrap();
        assert!(b == payload, "host got {} bytes, want {} intact", b.len(), payload.len());
        assert_eq!(c.drain(1).await.end.reason, wire::END_CLOSED);
    });
}

// DATA past the window, or after EOF, is a protocol violation.
#[test]
fn forward_window_violation() {
    run(async {
        let e = Env::new();
        let data = |b: Vec<u8>| wire::Frame::new(wire::TYPE_DATA, 1, b);
        let cases: Vec<(&str, Vec<wire::Frame>)> = vec![
            // sleep never reads stdin, so only a pipe's worth comes back.
            (
                "beyond grant",
                (0..2 * wire::CHANNEL_WINDOW as usize / wire::MAX_CHANNEL_DATA + 1)
                    .map(|_| data(vec![0; wire::MAX_CHANNEL_DATA]))
                    .collect(),
            ),
            ("too large", vec![data(vec![0; wire::MAX_CHANNEL_DATA + 1])]),
            ("after eof", vec![wire::Frame::new(wire::TYPE_EOF, 1, Vec::new()), data(b"x".to_vec())]),
        ];
        for (name, frames) in cases {
            let mut c = e.connect("phone").await;
            let r = c.exec(1, &["sleep", "60"]).await;
            assert!(r.error.is_empty(), "exec.start {r:?}");
            for f in frames {
                // The server may close the connection, as it should, while
                // frames after the violation are still being written.
                if c.try_send(f).await.is_err() {
                    break;
                }
            }
            assert!(c.closed().await, "{name}: connection open");
        }
    });
}

#[test]
fn forward_stream_close() {
    run(async {
        let closed = Rc::new(tokio::sync::Notify::new());
        let cl = closed.clone();
        let (port, _l) = listen(move |mut conn| {
            let cl = cl.clone();
            async move {
                let _ = tokio::io::copy(&mut conn, &mut tokio::io::sink()).await;
                cl.notify_one();
            }
        })
        .await;
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.forward(1, port).await;
        assert!(r.error.is_empty(), "forward.open {r:?}");
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 900, ..stream_req(wire::METHOD_STREAM_CLOSE, 1) })).await;
        // STREAM_END first, then the response.
        let end = c
            .until(|f| {
                assert_ne!(f.ty, wire::TYPE_RESPONSE, "response before STREAM_END");
                f.ty == wire::TYPE_STREAM_END && f.stream == 1
            })
            .await;
        assert_eq!(decode_end(&end).reason, wire::END_CLOSED);
        let f = c.until(|f| f.ty == wire::TYPE_RESPONSE).await;
        let resp: wire::Response = f.decode().unwrap();
        assert!(resp.id == 900 && resp.error.is_empty(), "response {resp:?}");
        tokio::time::timeout(Duration::from_secs(5), closed.notified()).await.expect("forwarded connection still open");
    });
}

#[test]
fn exec() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.exec(1, &["sh", "-c", "echo out; echo err >&2; exit 3"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        let r = c.drain(1).await;
        assert!(r.out(1) == "out\n" && r.out(2) == "err\n" && r.eofs.len() == 2, "stdout {:?} stderr {:?} eofs {:?}", r.out(1), r.out(2), r.eofs);
        assert!(r.end.reason == wire::END_EXITED && r.end.exit_code == Some(3), "end {:?}", r.end);
    });
}

#[test]
fn exec_stdin_cwd_env() {
    run(async {
        let dir = TempDir::new();
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c
            .call(wire::Request {
                cwd: dir.real(),
                env: vec!["OSEWA_T=v".into()],
                argv: ["sh", "-c", "pwd; echo $OSEWA_T; cat"].iter().map(|s| s.to_string()).collect(),
                ..stream_req(wire::METHOD_EXEC_START, 2)
            })
            .await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        c.data(2, b"from stdin\n").await;
        c.eof(2).await;
        let res = c.drain(2).await;
        assert_eq!(res.out(1), format!("{}\nv\nfrom stdin\n", dir.real()));
        assert_eq!(res.end.exit_code, Some(0), "end {:?}", res.end);
    });
}

#[test]
fn exec_errors() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.exec(1, &["osewa-no-such-program"]).await;
        assert_eq!(r.error, wire::ERR_NOT_FOUND, "missing program {r:?}");
        let r = c
            .call(wire::Request { argv: vec!["true".into()], cwd: "/no/such/dir".into(), ..stream_req(wire::METHOD_EXEC_START, 1) })
            .await;
        assert_eq!(r.error, wire::ERR_BAD_REQUEST, "missing cwd {r:?}");
        // The failed starts left the id free.
        let r = c.exec(1, &["true"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
    });
}

#[test]
fn exec_stream_close() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.exec(1, &["sh", "-c", "echo started; exec sleep 60"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        c.until(|f| f.ty == wire::TYPE_CHANNEL_DATA).await;
        let resp = c.call(stream_req(wire::METHOD_STREAM_CLOSE, 1)).await;
        assert!(resp.error.is_empty(), "stream.close {resp:?}");
        // call skipped the STREAM_END; it came first, which drain would
        // otherwise wait for forever.
        let r = c.call(stream_req(wire::METHOD_STREAM_CLOSE, 1)).await;
        assert_eq!(r.error, wire::ERR_NOT_FOUND, "second close {r:?}");
    });
}

// Killed processes report killed and 128+SIGHUP.
#[test]
fn exec_killed_status() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let r = c.exec(1, &["sleep", "60"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 901, ..stream_req(wire::METHOD_STREAM_CLOSE, 1) })).await;
        let r = c.drain(1).await;
        assert!(r.end.reason == wire::END_KILLED && r.end.exit_code == Some(128 + libc::SIGHUP as i64), "end {:?}", r.end);
    });
}

// A dropped connection closes forwards and ends processes.
#[test]
fn channels_end_with_connection() {
    run(async {
        let closed = Rc::new(tokio::sync::Notify::new());
        let cl = closed.clone();
        let (port, _l) = listen(move |mut conn| {
            let cl = cl.clone();
            async move {
                let _ = tokio::io::copy(&mut conn, &mut tokio::io::sink()).await;
                cl.notify_one();
            }
        })
        .await;
        let dir = TempDir::new();
        let pid_file = dir.path().join("pid");
        let e = Env::new();
        let mut c = e.connect("phone").await;
        assert!(c.forward(1, port).await.error.is_empty());
        let script = format!("echo $$ > {}; trap '' HUP; while :; do sleep 0.1; done", pid_file.display());
        assert!(c.exec(2, &["sh", "-c", &script]).await.error.is_empty());
        let pid = read_pid(&pid_file).await;
        c.peer.close();
        tokio::time::timeout(Duration::from_secs(5), closed.notified()).await.expect("forwarded connection still open");
        // It ignores SIGHUP, so it takes the SIGKILL 2 s later.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(tokio::time::Instant::now() < deadline, "process {pid} still running");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
}

// Session and channel streams share ids and the limit.
#[test]
fn channel_stream_ids() {
    run(async {
        let (port, _l) = listen(|mut conn| async move {
            let _ = tokio::io::copy(&mut conn, &mut tokio::io::sink()).await;
        })
        .await;
        let e = Env::with(|c| c.tune.max_streams = 2);
        let mut c = e.connect("phone").await;
        let id = c.create(&["/bin/cat"]).await;
        assert!(c.attach(&id, 1, false).await.error.is_empty());
        assert_eq!(c.forward(1, port).await.error, wire::ERR_BAD_REQUEST, "forward on a session's id");
        assert!(c.exec(2, &["sleep", "60"]).await.error.is_empty());
        assert_eq!(c.attach(&id, 2, false).await.error, wire::ERR_BAD_REQUEST, "attach on an exec's id");
        assert_eq!(c.forward(3, port).await.error, wire::ERR_UNAVAILABLE, "third stream");
        assert_eq!(c.call(stream_req(wire::METHOD_STREAM_CLOSE, 1)).await.error, wire::ERR_BAD_REQUEST, "stream.close on a session");
        assert_eq!(c.call(stream_req(wire::METHOD_STREAM_CLOSE, 9)).await.error, wire::ERR_NOT_FOUND, "stream.close on nothing");
        // Session frames on a channel stream, and channel frames on a
        // session stream, are dropped.
        c.input(2, 1, "x").await;
        c.data(1, b"x").await;
        assert_eq!(c.list().await.sessions.len(), 1);
    });
}

// Channels share the connection's in-flight window: two forwards with no
// credit returned stop at one window between them, not one each.
#[test]
fn channels_share_connection_flight() {
    run(async {
        let start = Rc::new(tokio::sync::Notify::new());
        let s = start.clone();
        let (port, _l) = listen(move |mut conn| {
            let s = s.clone();
            async move {
                s.notified().await;
                let _ = conn.write_all(&vec![0u8; 1 << 20]).await;
            }
        })
        .await;
        let e = Env::new();
        let mut c = e.connect("phone").await;
        c.hold_channel = true;
        for id in [1, 3] {
            assert!(c.forward(id, port).await.error.is_empty());
        }
        start.notify_waiters();
        let mut counted = 0;
        while let Ok(f) = c.read(Duration::from_millis(500)).await {
            if f.ty == wire::TYPE_CHANNEL_DATA {
                counted += f.payload.len();
            }
        }
        assert!(
            (FLIGHT_INITIAL / 2..=FLIGHT_INITIAL).contains(&counted),
            "{counted} bytes in flight across two forwards, want about one window ({FLIGHT_INITIAL})"
        );
    });
}

// stdout and stderr share the stream's credit: with none returned, both
// pumps together stay within the in-flight window (below the window of
// credit).
#[test]
fn exec_pumps_share_credit() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        c.hold_channel = true;
        let r = c.exec(1, &["sh", "-c", "head -c 1048576 /dev/zero & head -c 1048576 /dev/zero >&2; wait"]).await;
        assert!(r.error.is_empty(), "exec.start {r:?}");
        let mut counted = 0;
        while counted < FLIGHT_INITIAL - 2 {
            let f = c.next().await;
            if f.ty == wire::TYPE_CHANNEL_DATA {
                counted += f.payload.len();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 78, ..req(wire::METHOD_LIST) })).await;
        c.until(|f| {
            if f.ty == wire::TYPE_CHANNEL_DATA {
                counted += f.payload.len();
            }
            f.ty == wire::TYPE_RESPONSE
        })
        .await;
        assert!(counted <= FLIGHT_INITIAL, "{counted} bytes of CHANNEL_DATA on a window of {FLIGHT_INITIAL}");
    });
}

// A connection closing while a stream is opened can't leave a channel that
// nobody stops.
#[test]
fn reserve_while_closing() {
    run(async {
        let e = Env::new();
        let c = Client::new(e.srv.clone(), Rc::new(NullConn));
        c.close(crate::conn::CLOSE_GOING_AWAY, "");
        let mut added = false;
        assert!(!c.reserve(&wire::Request { id: 1, stream: 1, ..Default::default() }, |_| added = true) && !added);
    });
}

fn pop_types(c: &Client, n: usize) -> Vec<u8> {
    (0..n).map(|_| c.send.try_pop().expect("queue empty")[0]).collect()
}

// stream.close racing the stream's own end is answered after STREAM_END,
// and no GRANT follows the end.
#[test]
fn channel_end_order() {
    run(async {
        let e = Env::new();
        let c = Client::new(e.srv.clone(), Rc::new(NullConn));
        let ch = Channel::new(c.clone(), 5, Class::Exec);
        c.st.borrow_mut().chans.insert(5, ch.clone());
        ch.finish(); // the process exited; end hasn't pushed yet
        c.close_stream(wire::Request { id: 9, stream: 5, ..req(wire::METHOD_STREAM_CLOSE) });
        ch.grant(100);
        assert_eq!(c.send.bytes(), 0, "frames queued before STREAM_END");
        ch.end(wire::StreamEnd { reason: wire::END_EXITED.into(), exit_code: Some(0) });
        // A close after the end is answered behind it too.
        c.st.borrow_mut().chans.insert(5, ch.clone());
        c.close_stream(wire::Request { id: 10, stream: 5, ..req(wire::METHOD_STREAM_CLOSE) });
        assert_eq!(pop_types(&c, 3), [wire::TYPE_STREAM_END, wire::TYPE_RESPONSE, wire::TYPE_RESPONSE]);
        // The ended channel's id is free.
        assert!(c.reserve(&wire::Request { id: 11, stream: 5, ..Default::default() }, |_| {}), "id of an ended stream still in use");
    });
}

// close lets execproc's SIGKILL reach a process that ignores SIGHUP before
// it returns, since `osewa server` exits right after.
#[test]
fn close_kills_exec() {
    run(async {
        let dir = TempDir::new();
        let pid_file = dir.path().join("pid");
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let script = format!("trap '' HUP; echo $$ > {}; while :; do sleep 0.1; done", pid_file.display());
        assert!(c.exec(1, &["sh", "-c", &script]).await.error.is_empty());
        let pid = read_pid(&pid_file).await;
        e.srv.close().await;
        assert!(!alive(pid), "process {pid} alive after close");
    });
}

// A stream.close for a process that already exited reports exited, with
// its own code.
#[test]
fn exec_close_after_exit() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        // The background sleep holds stdout, so the stream outlives the
        // shell.
        assert!(c.exec(1, &["sh", "-c", "sleep 30 & exit 5"]).await.error.is_empty());
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 902, ..stream_req(wire::METHOD_STREAM_CLOSE, 1) })).await;
        let r = c.drain(1).await;
        assert!(r.end.reason == wire::END_EXITED && r.end.exit_code == Some(5), "end {:?}", r.end);
    });
}

// A client that credits as late as PROTOCOL.md allows (every 8 KiB) gets
// every byte, while the window shrinks to its minimum on loopback and an
// idle stderr pump waits in read.
#[test]
fn channel_lazy_credit() {
    const N: usize = 4 << 20;
    run(async {
        let e = Env::new();
        // exec
        let mut c = e.connect("phone").await;
        c.lazy_channel = true;
        assert!(c.exec(1, &["head", "-c", &N.to_string(), "/dev/zero"]).await.error.is_empty());
        let r = c.drain(1).await;
        assert!(r.data[1].len() == N && r.end.reason == wire::END_EXITED, "got {} bytes, end {:?}", r.data[1].len(), r.end);
        // forward
        let (port, _l) = listen(|mut conn| async move {
            let _ = conn.write_all(&vec![0u8; N]).await;
        })
        .await;
        let mut c = e.connect("phone").await;
        c.lazy_channel = true;
        assert!(c.forward(1, port).await.error.is_empty());
        c.eof(1).await;
        let r = c.drain(1).await;
        assert_eq!(r.data[1].len(), N, "end {:?}", r.end);
    });
}

// close outwaits a grandchild that ignores SIGHUP after the shell that
// started it dies of SIGHUP and is reaped: the stream ends at once, but only
// execproc's SIGKILL 2 s later reaches the grandchild.
#[test]
fn close_kills_exec_grandchild() {
    run(async {
        let dir = TempDir::new();
        let pid_file = dir.path().join("pid");
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let script = format!(r#"(trap "" HUP; exec sleep 100) & echo $! > {}; wait"#, pid_file.display());
        assert!(c.exec(1, &["sh", "-c", &script]).await.error.is_empty());
        let pid = read_pid(&pid_file).await;
        struct Kill(i32);
        impl Drop for Kill {
            fn drop(&mut self) {
                unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
        let _k = Kill(pid);
        e.srv.close().await;
        assert!(!alive(pid), "grandchild {pid} alive after close");
    });
}

// When writing stdin fails, the failed chunk and the ones queued behind it
// are granted back, so a client that spent its window on them can go on.
#[test]
fn exec_stdin_failure_grants() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        assert!(c.exec(1, &["sh", "-c", "exec 0<&-; sleep 30"]).await.error.is_empty());
        tokio::time::sleep(Duration::from_millis(200)).await; // let the shell close stdin
        for _ in 0..wire::CHANNEL_WINDOW as usize / wire::MAX_CHANNEL_DATA {
            c.data(1, &vec![0; wire::MAX_CHANNEL_DATA]).await;
        }
        let mut granted = 0usize;
        c.until(|f| {
            if f.ty == wire::TYPE_GRANT && f.stream == 1 {
                granted += wire::parse_credit(&f.payload).unwrap() as usize;
            }
            granted >= wire::CHANNEL_WINDOW as usize
        })
        .await;
        assert_eq!(granted, wire::CHANNEL_WINDOW as usize);
    });
}

// A GRANT whose check passed is queued before STREAM_END, never after it.
// Go needed a lock for this; here the check and the push run without an
// await between them, so the order holds by construction: a grant before
// the end goes first, and one after it is not sent.
#[test]
fn grant_before_stream_end() {
    run(async {
        let e = Env::new();
        let c = Client::new(e.srv.clone(), Rc::new(NullConn));
        let ch = Channel::new(c.clone(), 5, Class::Exec);
        c.st.borrow_mut().chans.insert(5, ch.clone());
        ch.grant(100);
        ch.end(wire::StreamEnd { reason: wire::END_EXITED.into(), exit_code: Some(0) });
        ch.grant(100);
        assert_eq!(pop_types(&c, 2), [wire::TYPE_GRANT, wire::TYPE_STREAM_END]);
        assert!(c.send.try_pop().is_none(), "GRANT after STREAM_END");
    });
}
