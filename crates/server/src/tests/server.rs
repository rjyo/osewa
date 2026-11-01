//! Sessions through the server: welcome, lifecycle, detach, resume,
//! errors.

use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;

use super::helpers::*;
use crate::client::{new_session_id, Client};
use crate::conn::{NullConn, CLOSE_GOING_AWAY};
use crate::{await_ready, holder_args, Config, Server};

#[test]
fn welcome() {
    run(async {
        let e = Env::new();
        let mut c = e.dial();
        c.send(wire::json(wire::TYPE_HELLO, 0, &wire::Hello { client_id: "a".into(), ..Default::default() })).await;
        let f = c.next().await;
        let w: wire::Welcome = f.decode().unwrap();
        assert!(
            f.ty == wire::TYPE_WELCOME
                && w.server == "testsrv"
                && w.ghostty == osewa_ghostty::COMMIT
                && w.snapshot_version == osewa_ghostty::SNAPSHOT_VERSION
                && w.carriers.is_empty(),
            "welcome {}",
            String::from_utf8_lossy(&f.payload)
        );
        let p = String::from_utf8_lossy(&f.payload);
        assert!(p.contains(r#""carriers":[]"#), "carriers not an empty array: {p}");
    });
}

#[test]
fn session_lifecycle() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let id = c.create(&["/bin/cat"]).await;

        let resp = c.attach(&id, 1, true).await;
        assert!(resp.error.is_empty() && !resp.epoch.is_empty() && resp.owner == "phone" && !resp.resumed, "attach {resp:?}");
        c.until(|f| f.ty == wire::TYPE_SNAPSHOT_END && f.stream == 1).await;
        c.input(1, 1, "hello\r").await;
        let (_, acked) = c.output_ack(1, "hello").await;
        assert_eq!(acked, 1, "input ack");
        c.send(wire::ack(1, 1, 10)).await;
        c.send(wire::credit(1, 1000)).await;

        let list = c.list().await;
        assert!(list.sessions.len() == 1 && list.sessions[0].id == id && list.sessions[0].owner == "phone", "list {list:?}");

        let r = c.call(wire::Request { cols: 100, rows: 30, ..stream_req(wire::METHOD_RESIZE, 1) }).await;
        assert!(r.error.is_empty(), "resize {r:?}");
        let r = c.call(stream_req(wire::METHOD_OWN, 1)).await;
        assert!(r.error.is_empty() && r.owner == "phone", "own {r:?}");

        let r = c.call(wire::Request { session: id.clone(), ..req(wire::METHOD_KILL) }).await;
        assert!(r.error.is_empty(), "kill {r:?}");
        let end = c.until(|f| f.ty == wire::TYPE_STREAM_END && f.stream == 1).await;
        assert_eq!(decode_end(&end).reason, wire::END_KILLED, "stream end");
        // The stream id is free again; the exited session still attaches.
        let r = c.attach(&id, 1, false).await;
        assert!(r.error.is_empty(), "attach after exit {r:?}");
        c.until(|f| f.ty == wire::TYPE_STREAM_END && f.stream == 1).await;

        // After the exit retention the holder goes away.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let l = c.list().await;
            if l.sessions.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "session still listed: {l:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let r = c.call(wire::Request { session: id.clone(), ..req(wire::METHOD_KILL) }).await;
        assert_eq!(r.error, wire::ERR_NOT_FOUND, "kill gone session {r:?}");
    });
}

#[test]
fn detach_and_reattach() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let id = c.create(&["/bin/cat"]).await;
        c.attach(&id, 3, true).await;
        let r = c.call(stream_req(wire::METHOD_DETACH, 3)).await;
        assert!(r.error.is_empty(), "detach {r:?}");
        // STREAM_END precedes the detach response; once both arrived, the
        // id can be reused.
        let r = c.call(stream_req(wire::METHOD_DETACH, 3)).await;
        assert_eq!(r.error, wire::ERR_NOT_FOUND, "second detach {r:?}");
        let r = c.attach(&id, 3, false).await;
        assert!(r.error.is_empty(), "reattach {r:?}");
        c.input(3, 1, "again\r").await;
        c.output(3, "again").await;
    });
}

#[test]
fn resume() {
    run(async {
        let e = Env::new();
        let mut a = e.connect("phone").await;
        let id = a.create(&["/bin/cat"]).await;
        let r = a.attach(&id, 1, true).await;
        a.input(1, 1, "one\r").await;
        let (pos, _) = a.output_ack(1, "one\r\none").await;
        a.hang_up();

        let mut b = e.connect("phone").await;
        let r2 = b
            .call(wire::Request {
                session: id.clone(),
                stream: 7,
                cols: 80,
                rows: 24,
                own: true,
                epoch: r.epoch.clone(),
                position: Some(pos),
                ..req(wire::METHOD_ATTACH)
            })
            .await;
        assert!(r2.error.is_empty() && r2.resumed && r2.epoch == r.epoch, "resume {r2:?}");
        // Seq 1 is a resend and must not be applied twice.
        b.input(7, 1, "one\r").await;
        b.input(7, 2, "two\r").await;
        let mut out = Vec::new();
        b.until(|f| {
            assert_ne!(f.ty, wire::TYPE_SNAPSHOT_BEGIN, "resume sent a snapshot");
            if f.ty == wire::TYPE_OUTPUT && f.stream == 7 {
                let (end, data) = wire::split_end(&f.payload).unwrap();
                assert!(end > pos, "duplicate output at {end} <= {pos}");
                out.extend_from_slice(data);
            }
            String::from_utf8_lossy(&out).contains("two\r\ntwo")
        })
        .await;
        assert!(!String::from_utf8_lossy(&out).contains("one"), "resent input applied again: {:?}", String::from_utf8_lossy(&out));
    });
}

#[test]
fn request_errors() {
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let missing = "ab".repeat(16);
        let cases = [
            (req("session.nope"), wire::ERR_BAD_REQUEST),
            (req(wire::METHOD_CREATE), wire::ERR_BAD_REQUEST),
            (wire::Request { session: missing.clone(), ..req(wire::METHOD_ATTACH) }, wire::ERR_BAD_REQUEST),
            (wire::Request { session: missing.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }, wire::ERR_NOT_FOUND),
            (wire::Request { session: "../../etc".into(), stream: 1, ..req(wire::METHOD_ATTACH) }, wire::ERR_NOT_FOUND),
            (stream_req(wire::METHOD_DETACH, 9), wire::ERR_NOT_FOUND),
            (stream_req(wire::METHOD_OWN, 9), wire::ERR_NOT_FOUND),
            (stream_req(wire::METHOD_RESIZE, 9), wire::ERR_NOT_FOUND),
            (wire::Request { session: missing.clone(), ..req(wire::METHOD_KILL) }, wire::ERR_NOT_FOUND),
            (wire::Request { session: "x".into(), ..req(wire::METHOD_KILL) }, wire::ERR_NOT_FOUND),
        ];
        for (r, code) in cases {
            let got = c.call(r.clone()).await;
            assert_eq!(got.error, code, "{} {r:?}", r.method);
        }
        // Data frames for an unknown stream are dropped, not fatal.
        c.input(9, 1, "x").await;
        c.send(wire::ack(9, 0, 0)).await;

        let id = c.create(&["/bin/cat"]).await;
        assert!(c.attach(&id, 2, false).await.error.is_empty());
        let r = c.attach(&id, 2, false).await;
        assert_eq!(r.error, wire::ERR_BAD_REQUEST, "duplicate stream {r:?}");
        // An error from the holder frees the stream id.
        let r = c
            .call(wire::Request { session: id.clone(), stream: 4, cols: 80, rows: 24, epoch: "old".into(), ..req(wire::METHOD_ATTACH) })
            .await;
        assert_eq!(r.error, wire::ERR_ENDED, "stale epoch {r:?}");
        let r = c.attach(&id, 4, false).await;
        assert!(r.error.is_empty(), "stream after holder error {r:?}");

        c.send(wire::Frame::new(wire::TYPE_REQUEST, 0, b"{".to_vec())).await;
        let f = c.until(|f| f.ty == wire::TYPE_RESPONSE).await;
        let resp: wire::Response = f.decode().unwrap();
        assert_eq!(resp.error, wire::ERR_BAD_REQUEST, "malformed request");
    });
}

#[test]
fn spawn_failure() {
    run(async {
        let spawn: crate::Spawn = Rc::new(|_| Box::pin(async { Err("no pty".to_string()) }));
        let e = Env::with_spawn(Some(spawn), Default::default(), |_| {});
        let mut c = e.connect("phone").await;
        let r = c.call(wire::Request { cols: 80, rows: 24, ..req(wire::METHOD_CREATE) }).await;
        assert_eq!(r.error, wire::ERR_INTERNAL, "create {r:?}");
    });
}

#[test]
fn create_defaults() {
    run(async {
        let got = Rc::new(std::cell::RefCell::new(None));
        let g = got.clone();
        let spawn: crate::Spawn = Rc::new(move |cfg| {
            *g.borrow_mut() = Some(cfg);
            Box::pin(async { Ok(()) })
        });
        let e = Env::with_spawn(Some(spawn), Default::default(), |cfg| cfg.shell = vec!["/bin/zsh".into(), "-l".into()]);
        let mut c = e.connect("phone").await;
        let r = c
            .call(wire::Request { cols: 90, rows: 20, cwd: "/tmp".into(), env: vec!["A=b".into()], ..req(wire::METHOD_CREATE) })
            .await;
        let cfg = got.borrow_mut().take().unwrap();
        assert!(
            cfg.id == r.session_id
                && cfg.run_dir == e.dir()
                && cfg.cols == 90
                && cfg.rows == 20
                && cfg.cwd == "/tmp"
                && cfg.env[0] == "A=b"
                && cfg.argv[0] == "/bin/zsh",
            "spawn config {cfg:?}"
        );
    });
}

#[test]
fn malformed_frames() {
    run(async {
        let e = Env::new();
        let hello = |id: &str, version| {
            wire::json(wire::TYPE_HELLO, 0, &wire::Hello { version, client_id: id.into(), ..Default::default() }).marshal()
        };
        let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
            ("request before hello", vec![wire::json(wire::TYPE_REQUEST, 0, &req(wire::METHOD_LIST)).marshal()]),
            ("hello without id", vec![hello("", 0)]),
            ("hello wrong version", vec![hello("x", 99)]),
            ("short", vec![vec![1, 0]]),
            ("unknown type", vec![hello("x", 0), vec![0x7f, 0, 0, 0, 0]]),
            ("server frame from client", vec![hello("x", 0), wire::output(1, 0, b"x").marshal()]),
            ("too large", vec![hello("x", 0), vec![0; wire::MAX_FRAME + 1]]),
        ];
        for (name, msgs) in cases {
            let mut c = e.dial();
            for m in msgs {
                let _ = c.send_raw(m).await;
            }
            assert!(c.closed().await, "{name}: connection stayed open");
        }
    });
}

// A client that stops reading must not hold up another client.
#[test]
fn slow_client() {
    run(async {
        let e = Env::new();
        let mut slow = e.connect("slow").await;
        let flood = slow.create(&["/bin/sh", "-c", "while :; do echo 0123456789abcdefghijklmnopqrstuvwxyz0123456789; done"]).await;
        slow.attach(&flood, 1, true).await;
        // slow never reads again.

        let mut fast = e.connect("fast").await;
        let id = fast.create(&["/bin/cat"]).await;
        fast.attach(&id, 1, true).await;
        for i in 0..20 {
            fast.input(1, i + 1, "ping\r").await;
        }
        fast.output(1, &"ping\r\n".repeat(20)).await;
        // The flooding holder is still responsive to others too.
        let mut other = e.connect("other").await;
        let r = other.attach(&flood, 1, false).await;
        assert!(r.error.is_empty(), "attach flood {r:?}");
        other.output(1, "xyz").await;
        let r = fast.list().await;
        assert_eq!(r.sessions.len(), 2, "list {r:?}");
        drop(slow);
    });
}

#[test]
fn list_removes_stale_sockets() {
    run(async {
        let e = Env::new();
        let stale = e.dir().join(format!("{}.sock", "cd".repeat(16)));
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        age(&stale);
        let junk = e.dir().join("junk.sock");
        std::fs::write(&junk, b"").unwrap();
        let mut c = e.connect("phone").await;
        let r = c.list().await;
        assert!(r.error.is_empty() && r.sessions.is_empty(), "list {r:?}");
        assert!(!stale.exists(), "stale socket kept");
        assert!(junk.exists(), "non-session file removed");
    });
}

#[test]
fn holder_dies() {
    run(async {
        let e = Env::new();
        let attach_then_die = "01".repeat(16);
        let die_at_once = "02".repeat(16);
        let misbehave = "03".repeat(16);
        let _h1 = fake_holder(
            e.dir(),
            &attach_then_die,
            handler(|mut conn| async move {
                let Ok(r) = read_request(&mut conn).await else { return };
                assert_eq!(r.client_id, "phone", "forwarded client_id");
                let _ = write_frame(&mut conn, wire::json(wire::TYPE_WELCOME, 0, &wire::Welcome::default())).await;
                let resp = wire::Response { id: r.id, epoch: "e".into(), ..Default::default() };
                let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &resp)).await;
                let _ = write_frame(&mut conn, wire::output(r.stream, 5, b"bye")).await;
            }),
        );
        let _h2 = fake_holder(
            e.dir(),
            &die_at_once,
            handler(|mut conn| async move {
                let _ = read_request(&mut conn).await;
            }),
        );
        let _h3 = fake_holder(
            e.dir(),
            &misbehave,
            handler(|mut conn| async move {
                let _ = read_request(&mut conn).await;
                let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &response(999))).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }),
        );

        let mut c = e.connect("phone").await;
        let r = c.call(wire::Request { session: attach_then_die.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }).await;
        assert!(r.error.is_empty(), "attach {r:?}");
        c.output(1, "bye").await;
        let end = c.until(|f| f.ty == wire::TYPE_STREAM_END).await;
        assert!(decode_end(&end).reason == wire::END_ERROR && end.stream == 1, "stream end {end:?}");

        let r = c.call(wire::Request { session: die_at_once.clone(), stream: 2, ..req(wire::METHOD_ATTACH) }).await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "attach to dying holder {r:?}");
        let r = c.call(wire::Request { session: die_at_once.clone(), ..req(wire::METHOD_KILL) }).await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "kill dying holder {r:?}");
        let r = c.call(wire::Request { session: misbehave.clone(), stream: 3, ..req(wire::METHOD_ATTACH) }).await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "attach to misbehaving holder {r:?}");
        // None of them show up in a list, and the sockets stay: they accept.
        let r = c.list().await;
        assert!(r.sessions.is_empty(), "list {r:?}");
    });
}

#[test]
fn disconnect_closes_holder_conns() {
    run(async {
        let e = Env::new();
        let id = "0a".repeat(16);
        let closed = Rc::new(tokio::sync::Notify::new());
        let cl = closed.clone();
        let _h = fake_holder(
            e.dir(),
            &id,
            handler(move |mut conn| {
                let cl = cl.clone();
                async move {
                    let Ok(r) = read_request(&mut conn).await else { return };
                    let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &response(r.id))).await;
                    while crate::read_frame(&mut conn).await.is_ok() {}
                    cl.notify_one();
                }
            }),
        );
        let mut c = e.connect("phone").await;
        c.call(wire::Request { session: id.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }).await;
        c.hang_up();
        tokio::time::timeout(Duration::from_secs(5), closed.notified()).await.expect("holder connection left open");
    });
}

#[test]
fn holder_args_line() {
    let cfg = |cwd: &str, argv: &[&str]| osewa_holder::Config {
        run_dir: "/r".into(),
        id: "i".into(),
        cols: 1,
        rows: 2,
        cwd: cwd.into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    assert_eq!(holder_args(&cfg("", &["sh", "-c", "x"])).join(" "), "holder --run /r --id i --cols 1 --rows 2 --env-stdin -- sh -c x");
    assert_eq!(holder_args(&cfg("/c", &["sh"])).join(" "), "holder --run /r --id i --cols 1 --rows 2 --cwd /c --env-stdin -- sh");
}

#[test]
fn await_ready_lines() {
    run(async {
        let ready = |script: &str, timeout: Duration| {
            let child = tokio::process::Command::new("/bin/sh")
                .args(["-c", script])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            await_ready(child, async { Ok(()) }, timeout)
        };
        assert!(ready("echo ready; echo more", Duration::from_secs(1)).await.is_ok());
        assert_eq!(ready("echo nope", Duration::from_secs(1)).await, Err(r#"server: holder said "nope\n": exit status: 0"#.into()));
        assert_eq!(ready("printf nope", Duration::from_secs(1)).await, Err("server: holder exited before ready: exit status: 0".into()));
        // An early exit says how, and the end of what the holder said.
        assert_eq!(
            ready("echo starting >&2; echo 'holder: no pty' >&2; exit 3", Duration::from_secs(1)).await,
            Err(r#"server: holder exited before ready: exit status: 3: "starting\nholder: no pty""#.into())
        );
        assert!(ready("printf ready", Duration::from_secs(1)).await.is_ok(), "ready without a newline");
        assert!(ready("exit 1", Duration::from_secs(1)).await.is_err(), "early exit accepted");
        assert!(ready("sleep 10", Duration::from_millis(50)).await.is_err(), "timeout not reported");
    });
}

/// The env written to a holder's stdin shares the readiness deadline: a
/// holder that never reads it, or fills stderr first, fails the create in
/// time rather than hanging it, and a failed write is said.
#[test]
fn await_ready_delivers_within_the_deadline() {
    run(async {
        let start = |script: &str| {
            tokio::process::Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let deliver = |child: &mut tokio::process::Child| {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().unwrap();
            // Far more than a pipe holds.
            let env = vec![b' '; 4 << 20];
            async move {
                stdin.write_all(&env).await?;
                stdin.shutdown().await
            }
        };
        let t0 = Instant::now();
        let mut c = start("exec sleep 10");
        let d = deliver(&mut c);
        assert!(await_ready(c, d, Duration::from_millis(300)).await.is_err(), "never ready, never reading: accepted");
        // Lots of stderr first, then never reading: drained, not deadlocked.
        let mut c = start("head -c 1000000 /dev/zero >&2; exec sleep 10");
        let d = deliver(&mut c);
        assert!(await_ready(c, d, Duration::from_millis(500)).await.is_err());
        assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
        // Exits without reading: the write's failure is in the error.
        let mut c = start("exit 4");
        let d = deliver(&mut c);
        let e = await_ready(c, d, Duration::from_secs(2)).await.unwrap_err();
        assert!(e.contains("exit status: 4") && e.contains("writing its env"), "{e}");
        // Reads it all, then ready.
        let mut c = start("cat >/dev/null; echo ready; exec sleep 10");
        let d = deliver(&mut c);
        assert_eq!(await_ready(c, d, Duration::from_secs(5)).await, Ok(()));
    });
}

#[test]
fn new_and_defaults() {
    let _g = super::ENV.lock().unwrap();
    let d = TempDir::new();
    let run_dir = d.path().join("run");
    // The default shell is the account's login shell (passwd), whatever
    // $SHELL the server was started with.
    let old_shell = std::env::var("SHELL").ok();
    unsafe { std::env::set_var("SHELL", "/bin/false") };
    assert!(Server::new(Config::default()).is_err(), "new without identity");
    let s = Server::new(Config { run_dir: run_dir.clone(), ..auth_config(d.path()) }).unwrap();
    match old_shell {
        Some(v) => unsafe { std::env::set_var("SHELL", v) },
        None => unsafe { std::env::remove_var("SHELL") },
    }
    assert!(s.cfg.shell == osewa_paths::account(None).login_shell() && !s.cfg.name.is_empty(), "defaults {:?} {:?}", s.cfg.shell, s.cfg.name);

    assert!(run_dir.is_dir());
    run(async {
        s.close().await;
        assert!(s.is_closed());
        // A connection after close is turned away.
        let (end, mut peer) = crate::conn::mem::pair();
        s.serve_conn(end).await;
        assert!(matches!(peer.recv().await, crate::conn::mem::Msg::Close(CLOSE_GOING_AWAY)));
    });
}

#[test]
fn list_keeps_young_refused_socket() {
    run(async {
        let e = Env::with(|c| c.tune.stale_retry = Duration::from_millis(10));
        let id = "55".repeat(16);
        let path = e.dir().join(format!("{id}.sock"));
        let _fd = bind_only(&path);
        let mut c = e.connect("phone").await;
        let r = c.list().await;
        assert!(r.error.is_empty() && r.sessions.is_empty(), "list {r:?}");
        assert!(path.exists(), "list removed a socket that may belong to a starting holder");
        // Refused is not gone: the client should retry, not conclude it ended.
        let r = c.call(wire::Request { session: id.clone(), stream: 1, ..req(wire::METHOD_ATTACH) }).await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "attach {r:?}");
        assert_eq!(r.message, format!("dial unix {}: connect: connection refused", path.display()));
        let r = c.call(wire::Request { session: id.clone(), ..req(wire::METHOD_KILL) }).await;
        assert_eq!(r.error, wire::ERR_UNAVAILABLE, "kill {r:?}");

        // Once old enough and still refusing, it is stale.
        age(&path);
        c.list().await;
        assert!(!path.exists(), "old refused socket kept");
    });
}

// close can reach a client that is tracked but not yet running.
#[test]
fn close_before_run() {
    run(async {
        let e = Env::new();
        let c = Client::new(e.srv.clone(), Rc::new(NullConn));
        c.close(CLOSE_GOING_AWAY, "");
        tokio::time::timeout(Duration::from_secs(1), c.cancelled()).await.expect("close before run left the client live");
        // Nothing can be reserved on it.
        assert!(!c.reserve(&wire::Request { id: 1, stream: 1, ..Default::default() }, |_| panic!("reserved")));
    });
}

// session.list relays each holder's session.info whole, including the
// fields the holder fills for `osewa ls`.
#[test]
fn list_relays_attached_and_idle() {
    run(async {
        let e = Env::new();
        let id = "56".repeat(16);
        let want = wire::SessionInfo {
            id: id.clone(),
            title: "vim".into(),
            cols: 80,
            rows: 24,
            created: 1,
            owner: "phone".into(),
            attached: 2,
            idle_seconds: 3600,
            ..Default::default()
        };
        let w = want.clone();
        let _h = fake_holder(
            e.dir(),
            &id,
            handler(move |mut conn| {
                let w = w.clone();
                async move {
                    let Ok(r) = read_request(&mut conn).await else { return };
                    if r.method != wire::METHOD_INFO {
                        return;
                    }
                    let resp = wire::Response { id: r.id, sessions: vec![w], ..Default::default() };
                    let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &resp)).await;
                }
            }),
        );
        let mut c = e.connect("phone").await;
        let r = c.list().await;
        assert!(r.error.is_empty() && r.sessions == vec![want], "list {r:?}");
    });
}

#[test]
fn session_ids() {
    let id = new_session_id();
    assert!(crate::client::valid_id(&id), "{id}");
    assert_ne!(id, new_session_id());
    for bad in ["x", "../../etc", &"AB".repeat(16), &"g".repeat(32)] {
        assert!(!crate::client::valid_id(bad), "{bad}");
    }
}

/// A session runs as the account (here a test one), whatever environment
/// the server was started with: none of it reaches the session, which
/// starts in the account's home with the client's env on top.
#[test]
fn session_env_is_the_accounts() {
    let _junk = super::Junk::set();
    run(async {
        let e = Env::new();
        let mut c = e.connect("phone").await;
        let resp = c
            .call(wire::Request {
                method: wire::METHOD_CREATE.into(),
                cols: 120,
                rows: 24,
                env: vec!["EDITOR=vi".into()],
                argv: ["sh", "-c", "echo [$HERDR_ENV][$OSEWA_TEST_TOKEN][$SHELL][$EDITOR][$HOME] cwd=$(pwd -P); sleep 30"].iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            })
            .await;
        assert!(resp.error.is_empty(), "create {resp:?}");
        c.attach(&resp.session_id, 1, true).await;
        let (home, shell) = (e.home(), osewa_paths::account(None).shell);
        c.output(1, &format!("[][][{shell}][vi][{home}] cwd={home}")).await;
    });
}

/// A sessions' home must be absolute: a relative one would resolve
/// against wherever each holder happens to run.
#[test]
fn home_must_be_absolute() {
    let d = TempDir::new();
    let cfg = Config { run_dir: d.path().join("run"), home: Some("relative/home".into()), ..auth_config(d.path()) };
    assert!(Server::new(cfg).err().is_some_and(|e| e.contains("absolute")), "relative home accepted");
}

/// Under OSEWA_REQUIRE_SANDBOX (cargo sets it for tests) an in-process
/// server without a sandbox home refuses too, as do holders and exec.
#[test]
fn unsandboxed_refuses() {
    if std::env::var_os(osewa_paths::REQUIRE_SANDBOX).is_none_or(|v| v != "1") {
        panic!("{} is not set: .cargo/config.toml sets it for every test", osewa_paths::REQUIRE_SANDBOX);
    }
    let d = TempDir::new();
    for home in [None, Some(osewa_paths::passwd_account().home)] {
        let cfg = Config { run_dir: d.path().join("run"), home: home.clone(), ..auth_config(d.path()) };
        assert!(Server::new(cfg).err().is_some_and(|e| e.contains(osewa_paths::REQUIRE_SANDBOX)), "home {home:?} accepted");
    }
    let opts = osewa_execproc::Options { argv: vec!["true".into()], ..Default::default() };
    assert!(osewa_execproc::Proc::start(opts).is_err(), "exec without a home");
    let cfg = osewa_holder::Config { argv: vec!["/bin/true".into()], ..Default::default() };
    assert!(osewa_holder::run(cfg, || {}, false).err().is_some_and(|e| e.contains(osewa_paths::REQUIRE_SANDBOX)), "holder without a home");
}
