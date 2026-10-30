//! Ported from server/relay_test.go and server/security_test.go.

use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::helpers::*;
use crate::{Config, Server};

// A holder applying input backpressure must neither end its stream nor
// stall the client's other streams and requests.
#[test]
fn holder_backpressure() {
    run(async {
        let e = Env::with(|c| c.tune.holder_timeout = Duration::from_millis(100));
        let (stalled, echo) = ("51".repeat(16), "52".repeat(16));
        let _s = stalled_holder(e.dir(), &stalled);
        let _e = echo_holder(e.dir(), &echo);

        let mut c = e.connect("phone").await;
        assert!(c.call(wire::Request { session: stalled.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }).await.error.is_empty());
        assert!(c.call(wire::Request { session: echo.clone(), stream: 2, ..req(wire::METHOD_ATTACH) }).await.error.is_empty());
        let chunk = "x".repeat(4096);
        // 2 MiB, far past socket buffers.
        for i in 0..512 {
            c.input(1, i + 1, &chunk).await;
        }
        // Every frame from here on is checked: no stream may end.
        let no_end = |f: &wire::Frame| {
            assert_ne!(f.ty, wire::TYPE_STREAM_END, "stream {} ended under backpressure: {f:?}", f.stream);
        };
        // The shared read loop still serves requests and the other stream.
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 1 << 40, ..req(wire::METHOD_LIST) })).await;
        c.until(|f| {
            no_end(f);
            f.ty == wire::TYPE_RESPONSE
        })
        .await;
        c.input(2, 1, "still here").await;
        c.until(|f| {
            no_end(f);
            f.ty == wire::TYPE_OUTPUT && f.stream == 2
        })
        .await;
        // Well past the holder timeout, stream 1 is still alive.
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: (1 << 40) + 1, ..req(wire::METHOD_LIST) })).await;
        c.until(|f| {
            no_end(f);
            f.ty == wire::TYPE_RESPONSE
        })
        .await;
    });
}

// Past its queue bound only the stalled stream ends.
#[test]
fn stream_queue_overflow() {
    run(async {
        let e = Env::with(|c| {
            c.tune.stream_queue_bytes = 64 << 10;
            c.tune.holder_timeout = Duration::from_secs(3600); // only the queue bound may end it
        });
        let (stalled, echo) = ("53".repeat(16), "54".repeat(16));
        let _s = stalled_holder(e.dir(), &stalled);
        let _e = echo_holder(e.dir(), &echo);

        let mut c = e.connect("phone").await;
        c.call(wire::Request { session: stalled.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }).await;
        c.call(wire::Request { session: echo.clone(), stream: 2, ..req(wire::METHOD_ATTACH) }).await;
        let chunk = "x".repeat(4096);
        for i in 0..512 {
            c.input(1, i + 1, &chunk).await;
        }
        let end = c.until(|f| f.ty == wire::TYPE_STREAM_END).await;
        assert!(end.stream == 1 && decode_end(&end).reason == wire::END_ERROR, "stream end {end:?}");
        let r = c.call(stream_req(wire::METHOD_DETACH, 1)).await;
        assert_eq!(r.error, wire::ERR_NOT_FOUND, "overflowed stream still registered: {r:?}");
        c.input(2, 1, "unaffected").await;
        c.output(2, "unaffected").await;
    });
}

// Client ids reach other clients' screens (owner in `osewa ls`, takeover
// notices), so ones that could carry escape sequences are refused.
#[test]
fn hello_client_id_must_be_printable() {
    run(async {
        let e = Env::new();
        for id in ["evil\x1b]52;c;aGk=\x07", "a\u{9b}31mb", "tab\there", &"x".repeat(129)] {
            let mut c = e.dial();
            let hello = wire::Hello { version: wire::VERSION, client_id: id.into(), ..Default::default() };
            c.send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
            assert!(c.closed().await, "client_id {id:?} accepted");
        }
        // A normal id with non-ASCII letters is fine.
        let mut c = e.connect("ノートPC-3f2a").await;
        assert!(c.list().await.error.is_empty());
    });
}

#[test]
fn size_bounds() {
    run(async {
        let e = Env::new();
        let id = "61".repeat(16);
        let _h = echo_holder(e.dir(), &id);
        let mut c = e.connect("phone").await;
        for r in [
            wire::Request { cols: 65535, rows: 65535, ..req(wire::METHOD_CREATE) },
            wire::Request { cols: 80, rows: wire::MAX_ROWS + 1, ..req(wire::METHOD_CREATE) },
            wire::Request { session: id.clone(), stream: 1, cols: wire::MAX_COLS + 1, rows: 24, ..req(wire::METHOD_ATTACH) },
        ] {
            let got = c.call(r.clone()).await;
            assert_eq!(got.error, wire::ERR_BAD_REQUEST, "{} {}x{}: {got:?}", r.method, r.cols, r.rows);
        }
        let r = c
            .call(wire::Request { session: id.clone(), stream: 1, cols: wire::MAX_COLS, rows: wire::MAX_ROWS, ..req(wire::METHOD_ATTACH) })
            .await;
        assert!(r.error.is_empty(), "largest size refused: {r:?}");
        let r = c.call(wire::Request { cols: 65535, rows: 65535, ..stream_req(wire::METHOD_RESIZE, 1) }).await;
        assert_eq!(r.error, wire::ERR_BAD_REQUEST, "resize {r:?}");
    });
}

#[test]
fn stream_limit() {
    run(async {
        let e = Env::with(|c| c.tune.max_streams = 2);
        let id = "62".repeat(16);
        let _h = echo_holder(e.dir(), &id);
        let mut c = e.connect("phone").await;
        let attach = |s| wire::Request { session: id.clone(), stream: s, ..req(wire::METHOD_ATTACH) };
        for s in 1..=2 {
            assert!(c.call(attach(s)).await.error.is_empty());
        }
        let r = c.call(attach(3)).await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "third stream: {r:?}");
        c.call(stream_req(wire::METHOD_DETACH, 1)).await;
        // The slot frees once the relay finishes, just after the response.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = c.call(attach(3)).await;
            if r.error.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "after a detach: {r:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
}

// Each create, list and kill runs as its own task; a client can't pile up
// more than max_busy of them.
#[test]
fn busy_limit() {
    run(async {
        let e = Env::with(|c| {
            c.tune.max_busy = 1;
            c.tune.holder_timeout = Duration::from_millis(500);
        });
        let id = "63".repeat(16);
        // Never answers, so a kill holds its slot until holder_timeout.
        let _h = fake_holder(
            e.dir(),
            &id,
            handler(|conn| async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(conn);
            }),
        );
        let mut c = e.connect("phone").await;
        c.send(wire::json(wire::TYPE_REQUEST, 0, &wire::Request { id: 1 << 41, session: id.clone(), ..req(wire::METHOD_KILL) })).await;
        let r = c.list().await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "second request in flight: {r:?}");
        c.until(|f| f.ty == wire::TYPE_RESPONSE && f.decode::<wire::Response>().is_ok_and(|r| r.id == 1 << 41)).await;
        let r = c.list().await;
        assert!(r.error.is_empty(), "slot not released: {r:?}");
    });
}

#[test]
fn run_dir_must_be_private() {
    let d = TempDir::new();
    let new = |dir: std::path::PathBuf| Server::new(Config { run_dir: dir, ..auth_config(d.path()) }).map(|_| ());
    let open = d.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let err = new(open).unwrap_err();
    assert!(err.contains("must be private"), "0755 run dir: {err}");
    let file = d.path().join("f");
    std::fs::write(&file, b"").unwrap();
    assert!(new(file).is_err(), "a file as run dir accepted");
    let target = d.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = d.path().join("l");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(new(link).is_err(), "a symlink as run dir accepted");
    assert!(new(d.path().join("private")).is_ok(), "0700 run dir");
}

/// The server's one client in e.
fn only_client(e: &Env) -> std::rc::Rc<crate::client::Client> {
    let st = e.srv.st.borrow();
    assert_eq!(st.clients.len(), 1);
    st.clients.values().next().unwrap().upgrade().unwrap()
}

// A failed write to the holder ends the stream as a dropped holder does:
// STREAM_END error, and the id free again. Also when it fails while the
// relay waits for a client that is behind.
#[test]
fn holder_write_failure_ends_stream() {
    run(async {
        let e = Env::with(|c| c.tune.send_soft = 1);
        let id = "64".repeat(16);
        let _h = echo_holder(e.dir(), &id);
        let mut c = e.connect("phone").await;
        let attach = || wire::Request { session: id.clone(), stream: 1, ..req(wire::METHOD_ATTACH) };
        assert!(c.call(attach()).await.error.is_empty());
        // More echoes than the connection buffers: the relay waits to push.
        for i in 0..200 {
            c.input(1, i + 1, "x").await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        // What the writer does when its write fails (EPIPE).
        only_client(&e).stream(1).unwrap().conn_broken();
        let end = c.until(|f| f.ty == wire::TYPE_STREAM_END).await;
        assert!(end.stream == 1 && decode_end(&end).reason == wire::END_ERROR, "stream end {end:?}");
        assert_eq!(c.call(stream_req(wire::METHOD_DETACH, 1)).await.error, wire::ERR_NOT_FOUND);
        assert!(c.call(attach()).await.error.is_empty(), "stream id still in use");
    });
}
