//! channel_test.go

use std::io::{Read, Write};
use std::net::TcpListener;

use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::*;
use crate::channel::{Flight, FLIGHT_INITIAL, FLIGHT_MAX, FLIGHT_MIN};

/// A TCP server that echoes until the client half-closes, then closes.
fn echo_server() -> (String, u16) {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in ln.incoming() {
            let Ok(mut c) = c else { return };
            std::thread::spawn(move || {
                let mut w = c.try_clone().unwrap();
                let _ = std::io::copy(&mut c, &mut w);
            });
        }
    });
    ("127.0.0.1".into(), port)
}

fn random(n: usize) -> Vec<u8> {
    (0..n).map(|_| rand::random::<u8>()).collect()
}

fn exec_opts(argv: &[&str]) -> ExecOptions {
    ExecOptions { argv: argv.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

#[test]
fn hello_features() {
    let f = Fake::new();
    let c = f.dial("a");
    assert_eq!(f.lock().hellos[0].features, [wire::FEATURE_FORWARD, wire::FEATURE_EXEC], "HELLO features");
    let has = |feature: &str| run(c.has(feature)).unwrap();
    assert!(has(wire::FEATURE_FORWARD) && has(wire::FEATURE_EXEC) && !has("teleport"), "has doesn't follow WELCOME");
}

/// Dial returns before WELCOME, so has waits for it instead of answering
/// false.
#[test]
fn has_waits_for_welcome() {
    let f = Fake::new();
    f.set(|st| st.late_welcome = true);
    let c = f.dial("a");
    assert!(run_for(Duration::from_millis(50), c.has(wire::FEATURE_FORWARD)).is_none(), "has before WELCOME didn't wait");
    let c2 = c.clone();
    let got = std::thread::spawn(move || run(c2.has(wire::FEATURE_FORWARD)).unwrap_or(false));
    run(c.list()).unwrap(); // the fake's WELCOME follows the first request
    assert!(got.join().unwrap(), "has after WELCOME is false");
    run(c.close());
    assert!(run(c.has(wire::FEATURE_FORWARD)).is_ok(), "has on a closed conn that had WELCOME");
}

/// Without the feature in WELCOME the client never sends its method.
#[test]
fn feature_missing() {
    let f = Fake::new();
    f.set(|st| st.features.clear());
    let c = f.dial("a");
    assert!(!run(c.has(wire::FEATURE_FORWARD)).unwrap(), "has(forward) without it in WELCOME");
    let err = run(c.dial_forward("127.0.0.1", 1)).unwrap_err();
    assert!(err.is_code(wire::ERR_BAD_REQUEST), "dial_forward = {err}");
    let err = run(c.exec(exec_opts(&["true"]))).err().unwrap();
    assert!(err.is_code(wire::ERR_BAD_REQUEST), "exec = {err}");
    assert!(f.lock().chans.is_empty(), "a channel was opened");
}

#[test]
fn forward_echo() {
    let f = Fake::new();
    let (host, port) = echo_server();
    let fc = run(f.dial("a").dial_forward(&host, port)).unwrap();
    assert_eq!(fc.remote_addr(), format!("{host}:{port}"));
    run(fc.write_all(b"hello")).unwrap();
    let mut buf = [0u8; 5];
    let mut r = fc.clone();
    run(r.read_exact(&mut buf)).unwrap();
    assert_eq!(&buf, b"hello");
    fc.close_write().unwrap();
    assert!(run(fc.write(b"x")).is_err(), "write after close_write");
    // The echo server closes after our half-close: EOF, then STREAM_END.
    assert_eq!(run(fc.read(&mut buf)).unwrap(), 0, "read after half-close");
    fc.close();
}

/// HTTP/1.1 runs over a forward, keep-alive and all.
#[test]
fn forward_http() {
    let f = Fake::new();
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in ln.incoming() {
            let Ok(c) = c else { return };
            std::thread::spawn(move || {
                let mut w = c.try_clone().unwrap();
                let mut r = std::io::BufReader::new(c);
                loop {
                    let mut line = String::new();
                    if std::io::BufRead::read_line(&mut r, &mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let path = line.split(' ').nth(1).unwrap_or("").to_string();
                    loop {
                        let mut h = String::new();
                        if std::io::BufRead::read_line(&mut r, &mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let body = format!("hi {path}");
                    let _ = write!(w, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                }
            });
        }
    });
    let c = f.dial("a");
    let fc = run(c.dial_forward("127.0.0.1", port)).unwrap();
    let mut rd = tokio::io::BufReader::new(fc.clone());
    for i in 0..3 {
        run(fc.write_all(format!("GET /x{i} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())).unwrap();
        let mut len = 0;
        loop {
            let mut h = String::new();
            run(rd.read_line(&mut h)).unwrap();
            if let Some(v) = h.strip_prefix("Content-Length: ") {
                len = v.trim().parse().unwrap();
            }
            if h == "\r\n" {
                break;
            }
        }
        let mut body = vec![0; len];
        run(rd.read_exact(&mut body)).unwrap();
        assert_eq!(String::from_utf8(body).unwrap(), format!("hi /x{i}"));
    }
    fc.close();
}

#[test]
fn forward_unreachable() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let f = Fake::new();
    let err = run(f.dial("a").dial_forward("127.0.0.1", port)).unwrap_err();
    assert!(err.is_code(wire::ERR_UNREACHABLE), "err = {err}, want unreachable");
}

/// Megabytes each way arrive intact, and the client never sends DATA
/// beyond its grant.
#[test]
fn forward_large_transfer() {
    let f = Fake::new();
    let (host, port) = echo_server();
    let fc = run(f.dial("a").dial_forward(&host, port)).unwrap();
    let data = random(8 << 20);
    let (w, d2) = (fc.clone(), data.clone());
    std::thread::spawn(move || {
        let _ = run_for(Duration::from_secs(30), w.write_all(&d2));
        let _ = w.close_write();
    });
    let mut got = Vec::new();
    let mut r = fc.clone();
    run_for(Duration::from_secs(30), r.read_to_end(&mut got)).unwrap().unwrap();
    assert!(got == data, "got {} bytes, corrupted", got.len());
    let ch = f.last_chan();
    f.chan(ch, |ch| assert!(!ch.over && ch.max_fly <= wire::CHANNEL_WINDOW as i64, "client exceeded its grant: max outstanding {}", ch.max_fly));
    fc.close();
}

/// A reader that stops reading stops the server: it is sent no more than
/// the window, and the rest waits at the source.
#[test]
fn forward_slow_reader_bounded() {
    let f = Fake::new();
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    let data = random(16 << 20);
    let d2 = data.clone();
    std::thread::spawn(move || {
        if let Ok((mut c, _)) = ln.accept() {
            let _ = c.write_all(&d2);
        }
    });
    let fc = run(f.dial("a").dial_forward("127.0.0.1", port)).unwrap();
    let ch = f.last_chan();
    sleep(Duration::from_millis(200));
    f.chan(ch, |ch| assert!(ch.pumped <= wire::CHANNEL_WINDOW as usize, "sent {} bytes to a reader that read nothing", ch.pumped));
    let mut got = Vec::new();
    let mut r = fc.clone();
    run_for(Duration::from_secs(30), r.read_to_end(&mut got)).unwrap().unwrap();
    assert!(got == data, "got {} bytes", got.len());
    fc.close();
}

/// Go's deadlines become the caller's timeouts: a cancelled read or write
/// leaves the forward usable, and a write without grants takes what the
/// window allows and then waits.
#[test]
fn forward_timeouts() {
    let f = Fake::new();
    let (host, port) = echo_server();
    let fc = run(f.dial("a").dial_forward(&host, port)).unwrap();

    let mut b = [0u8; 1];
    assert!(run_for(Duration::from_millis(30), fc.read(&mut b)).is_none(), "read without data returned");
    run(fc.write_all(b"y")).unwrap();
    assert_eq!(run(fc.read(&mut b)).unwrap(), 1);
    assert_eq!(&b, b"y");

    // Without grants a write takes the window, then waits.
    let ch = f.last_chan();
    f.chan(ch, |ch| ch.no_grant = true);
    let big = vec![0u8; wire::CHANNEL_WINDOW as usize + 1];
    let n = run(fc.write(&big)).unwrap();
    assert!(n > 0 && n <= wire::CHANNEL_WINDOW as usize, "write without grant took {n}");
    let mut rest = &big[n..];
    let mut more = 0;
    while let Some(r) = run_for(Duration::from_millis(200), fc.write(rest)) {
        let k = r.unwrap();
        more += k;
        rest = &rest[k..];
        assert!(n + more <= wire::CHANNEL_WINDOW as usize, "wrote {} without grants", n + more);
    }
    f.grant_all(ch);
    fc.close();
}

#[test]
fn forward_close() {
    let f = Fake::new();
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut c, _)) = ln.accept() {
            let _ = tx.send(c.read(&mut [0u8; 1]).map_err(|e| e.kind()));
        }
    });
    let fc = run(f.dial("a").dial_forward("127.0.0.1", port)).unwrap();
    fc.close();
    fc.close();
    let mut b = [0u8; 1];
    assert_eq!(run(fc.read(&mut b)).unwrap_err().kind(), std::io::ErrorKind::NotConnected, "read after close");
    assert_eq!(run(fc.write(b"x")).unwrap_err().kind(), std::io::ErrorKind::NotConnected, "write after close");
    assert_eq!(fc.close_write().unwrap_err().kind(), std::io::ErrorKind::NotConnected, "close_write after close");
    match rx.recv_timeout(WAIT) {
        Ok(Ok(0)) => {}
        r => panic!("target read {r:?}, want EOF"),
    }
}

/// Channel streams end with their connection.
#[test]
fn channels_end_with_conn() {
    let f = Fake::new();
    let (host, port) = echo_server();
    let c = f.dial("a");
    let fc = run(c.dial_forward(&host, port)).unwrap();
    let e = run(c.exec(exec_opts(&["sleep", "10"]))).unwrap();
    f.drop_all();
    let mut b = [0u8; 1];
    match run(fc.read(&mut b)) {
        Err(_) => {}
        r => panic!("read on a dead conn = {r:?}"),
    }
    assert!(run(fc.write(b"x")).is_err(), "write on a dead conn");
    assert!(fc.close_write().is_err(), "close_write on a dead conn");
    assert!(run(e.wait()).is_err(), "wait on a dead conn");
    run(e.close()).unwrap();
    assert!(run(c.dial_forward(&host, port)).is_err(), "dial_forward on a dead conn");
}

#[test]
fn exec_output() {
    let f = Fake::new();
    let c = f.dial("a");
    let dir = crate::hosts::testutil::TempDir::new();
    let o = run(c.output(ExecOptions {
        argv: vec!["sh".into(), "-c".into(), r#"echo "out $FOO $(pwd)"; echo err >&2; exit 3"#.into()],
        cwd: dir.path().to_string_lossy().into(),
        env: vec!["FOO=bar".into()],
    }))
    .unwrap();
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let base = dir.path().file_name().unwrap().to_string_lossy().to_string();
    assert!(
        o.code == 3 && out.starts_with("out bar ") && out.trim().ends_with(&base) && o.stderr == b"err\n",
        "out {out:?} err {:?} code {}",
        String::from_utf8_lossy(&o.stderr),
        o.code
    );
}

#[test]
fn exec_not_found() {
    let f = Fake::new();
    let err = run(f.dial("a").exec(exec_opts(&["osewa-no-such-program"]))).err().unwrap();
    assert!(err.is_code(wire::ERR_NOT_FOUND), "err = {err}");
    let err = run(f.dial("a").output(exec_opts(&["osewa-no-such-program"]))).unwrap_err();
    assert!(err.is_code(wire::ERR_NOT_FOUND), "output err = {err}");
}

#[test]
fn exec_stdin() {
    let f = Fake::new();
    let e = run(f.dial("a").exec(exec_opts(&["cat"]))).unwrap();
    let data = random(1 << 20);
    let (stdin, d2) = (e.stdin(), data.clone());
    std::thread::spawn(move || {
        let _ = run(stdin.write_all(&d2));
        let _ = stdin.close();
    });
    let got = run(e.stdout().read_to_end()).unwrap();
    assert!(got == data, "cat gave {} bytes", got.len());
    let errb = run(e.stderr().read_to_end()).unwrap();
    assert!(errb.is_empty(), "stderr {errb:?}");
    assert_eq!(run(e.wait()).unwrap(), 0);
    // Already over: nothing to do.
    run(e.close()).unwrap();
}

/// A large output read slowly arrives whole, never more than the window
/// ahead of the reader.
#[test]
fn exec_large_slow_reader() {
    let f = Fake::new();
    let e = run(f.dial("a").exec(exec_opts(&["sh", "-c", "head -c 4194304 /dev/zero | tr '\\0' 'z'"]))).unwrap();
    let ch = f.last_chan();
    let mut n = 0;
    let mut buf = [0u8; 1000];
    let out = e.stdout();
    loop {
        let k = run(out.read(&mut buf)).unwrap();
        if k == 0 {
            break;
        }
        n += k;
        assert!(buf[..k].iter().all(|&b| b == b'z'), "corrupted");
        if n % (512 << 10) < 1000 {
            f.chan(ch, |ch| assert!(ch.pumped - n <= wire::CHANNEL_WINDOW as usize, "{} bytes ahead of the reader", ch.pumped - n));
        }
    }
    assert_eq!(n, 4 << 20);
    assert_eq!(run(e.wait()).unwrap(), 0);
}

#[test]
fn exec_close() {
    let f = Fake::new();
    let e = run(f.dial("a").exec(exec_opts(&["sleep", "30"]))).unwrap();
    let start = Instant::now();
    run(e.close()).unwrap();
    assert_eq!(run(e.wait()).unwrap(), 137, "wait after close");
    assert!(start.elapsed() <= Duration::from_secs(5), "close took too long");
    let mut b = [0u8; 1];
    assert_eq!(run(e.stdout().read(&mut b)).unwrap_err().kind(), std::io::ErrorKind::NotConnected, "read after close");
}

/// Dropping output's future ends the command.
#[test]
fn output_cancel() {
    let f = Fake::new();
    let c = f.dial("a");
    let start = Instant::now();
    assert!(run_for(Duration::from_millis(200), c.output(exec_opts(&["sleep", "30"]))).is_none());
    // The command is gone: its stream ended.
    eventually("command ended", || f.lock().chans.values().all(|ch| ch.ended));
    assert!(start.elapsed() < Duration::from_secs(5));
}

/// A STREAM_END without exit code, or with another reason, is an error.
#[test]
fn exec_odd_end() {
    let f = Fake::new();
    let c = f.dial("a");
    let ch = crate::channel::Channel::new(&c, 99);
    ch.lock().end = Some(wire::StreamEnd { reason: wire::END_EXITED.into(), exit_code: None });
    assert!(Exec::from_channel(ch).wait_now().is_err(), "exited without a code");
}

/// Keystrokes and replies go out before queued uploads.
#[test]
fn writer_puts_data_last() {
    let mut q = crate::conn::WriteQ::default();
    for f in [
        wire::Frame::new(wire::TYPE_DATA, 1, b"bulk1".to_vec()),
        wire::Frame::new(wire::TYPE_EOF, 1, Vec::new()),
        wire::json(wire::TYPE_INPUT, 2, &wire::Input { seq: 1, ty: "text".into(), text: "k".into(), ..Default::default() }),
        wire::credit(2, 10),
        wire::Frame::new(wire::TYPE_DATA, 1, b"bulk2".to_vec()),
    ] {
        q.push(&f);
    }
    let order: Vec<u8> = std::iter::from_fn(|| q.pop()).map(|b| b[0]).collect();
    assert_eq!(order, [wire::TYPE_INPUT, wire::TYPE_CREDIT, wire::TYPE_DATA, wire::TYPE_EOF, wire::TYPE_DATA]);
}

/// The server keeps uncredited data within an in-flight window as small as
/// 16 KiB and grants as late as PROTOCOL.md lets it. Bulk transfers both
/// ways must still finish: credit can't wait for a batch the server will
/// never fill.
#[test]
fn channel_small_server_window() {
    let f = Fake::new();
    f.set(|st| {
        st.chan_fly = 16 << 10;
        st.lazy_grant = true;
    });
    let c = f.dial("a");
    let (host, port) = echo_server();
    let fc = run(c.dial_forward(&host, port)).unwrap();
    let data = random(1 << 20);
    let (w, d2) = (fc.clone(), data.clone());
    std::thread::spawn(move || {
        let _ = run_for(Duration::from_secs(30), w.write_all(&d2));
        let _ = w.close_write();
    });
    let mut got = Vec::new();
    let mut r = fc.clone();
    run_for(Duration::from_secs(10), r.read_to_end(&mut got)).expect("forward timed out").unwrap();
    assert!(got == data, "forward: {} of {} bytes", got.len(), data.len());
    // Exec: the window is shared by stdout and stderr.
    let o = run(c.output(exec_opts(&["sh", "-c", "head -c 1000000 /dev/zero; head -c 300000 /dev/zero >&2"]))).unwrap();
    assert!(o.code == 0 && o.stdout.len() == 1000000 && o.stderr.len() == 300000, "exec: {}+{} bytes, code {}", o.stdout.len(), o.stderr.len(), o.code);
    fc.close();
}

/// A few bytes read are credited soon after, not when 8 KiB build up.
#[test]
fn channel_credit_timer() {
    let f = Fake::new();
    let x = run(f.dial("a").exec(exec_opts(&["sh", "-c", "printf abc; sleep 5"]))).unwrap();
    let mut buf = [0u8; 3];
    let mut out = x.stdout();
    run(out.read_exact(&mut buf)).unwrap();
    let ch = f.last_chan();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // 3 bytes and the channel byte.
        let credited = f.chan(ch, |ch| ch.credited);
        if credited == 4 {
            break;
        }
        assert!(Instant::now() < deadline, "credited {credited}, want 4");
        sleep(Duration::from_millis(10));
    }
    let _ = run(x.close());
}

#[test]
fn flight_window() {
    let mut f = Flight::default();
    assert_eq!(f.room(), FLIGHT_INITIAL, "initial room");
    let t0 = Instant::now();
    let ms = |n: u64| t0 + Duration::from_millis(n);
    f.sent(32 << 10, t0);
    assert_eq!(f.room(), FLIGHT_INITIAL - (32 << 10));
    // 32 KiB granted back after a 100 ms round trip: the link carries
    // 32 KiB per round trip, so the window is 2 x that.
    f.granted(32 << 10, ms(100));
    assert!(f.window == 64 << 10 && f.in_flight == 0, "window {} in flight {}", f.window, f.in_flight);
    // A slow link shrinks it, to no less than the minimum.
    f.sent(1 << 10, ms(1000));
    f.granted(1 << 10, ms(1100));
    assert_eq!(f.window, FLIGHT_MIN, "window, want the minimum");
    // A fast one grows it, to no more than the maximum.
    let now = ms(2000);
    for _ in 0..400 {
        f.sent(32 << 10, now);
        f.granted(32 << 10, now + Duration::from_millis(100));
    }
    assert_eq!(f.window, FLIGHT_MAX, "window, want the maximum");
    // Old round trips age out after the RTT window.
    f.sent(1, now + Duration::from_secs(60));
    f.granted(1, now + Duration::from_millis(60_300));
    assert_eq!(f.rtts.len(), 1, "round trip samples kept");
}

#[test]
fn create_options() {
    let f = Fake::new();
    run(f.dial("a").create(CreateOptions {
        cols: 80,
        rows: 24,
        argv: vec!["sh".into()],
        cwd: "/tmp".into(),
        env: vec!["A=1".into()],
    }))
    .unwrap();
    let r = f.lock().creates[0].clone();
    assert!(r.cwd == "/tmp" && r.env == ["A=1"] && r.argv == ["sh"], "create request {r:?}");
}
