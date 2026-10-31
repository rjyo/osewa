//! exec and forward: spec parsing, and live against a real server.

use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, TcpStream};

use super::live::LiveHost;
use super::*;
use crate::channels::{parse_forward, ForwardSpec};

#[test]
fn parse_forward_specs() {
    let spec = |listen: &str, host: &str, port: u16| ForwardSpec { listen: listen.into(), host: host.into(), port };
    for (input, want) in [
        ("8080", spec("127.0.0.1:8080", "localhost", 8080)),
        ("0.0.0.0:8080", spec("0.0.0.0:8080", "localhost", 8080)),
        ("8080:db:5432", spec("127.0.0.1:8080", "db", 5432)),
        ("[::1]:8080:[fe80::1]:22", spec("[::1]:8080", "fe80::1", 22)),
        ("localhost:0:10.0.0.1:443", spec("localhost:0", "10.0.0.1", 443)),
    ] {
        match parse_forward(input) {
            Ok(got) => assert_eq!(got, want, "parse_forward({input:?})"),
            Err(e) => panic!("parse_forward({input:?}): {e}"),
        }
    }
    for bad in ["", "x", "1:2:3:4:5", "8080:db:x", "8080::22", "99999", "0"] {
        assert!(parse_forward(bad).is_err(), "parse_forward({bad:?}) accepted");
    }
}

#[test]
fn live_exec() {
    let h = LiveHost::new();
    let (e, out, err) = h.env("line\n");
    let code = block(e.exec(
        &ctx(),
        &args(&["--cwd", "/", "--env", "A=1", "--env", "B=2", "h", "--", "sh", "-c", r#"echo "hi $A$B $(pwd)"; echo err >&2; read x; echo "got $x"; exit 7"#]),
    ))
    .unwrap();
    assert!(code == 7 && out.string() == "hi 12 /\ngot line\n" && err.string() == "err\n", "code {code} out {:?} err {:?}", out.string(), err.string());
    assert_eq!(h.run(&["exec", "h", "--", "true"]).0, 0, "exec true");
    assert_eq!(h.run(&["exec", "h", "--", "osewa-no-such-program"]).0, 1, "missing program");
    for a in [&["exec", "h"][..], &["exec", "--", "true"], &["exec", "h", "x", "--", "true"]] {
        assert_eq!(h.run(a).0, 2, "{a:?}, want usage");
    }
}

/// Interrupting exec ends the command and reports 130.
#[test]
fn live_exec_interrupted() {
    let h = LiveHost::new();
    let (e, _, _) = h.env("");
    let start = Instant::now();
    let code = block(e.exec(&Ctx::timeout(Duration::from_millis(300)), &args(&["h", "--", "sleep", "30"]))).unwrap();
    assert!(code == 130 && start.elapsed() < Duration::from_secs(5), "code {code} after {:?}", start.elapsed());
}

/// A minimal HTTP server: GET path answers "served path".
fn http_server() -> u16 {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in ln.incoming().flatten() {
            std::thread::spawn(move || {
                let mut w = c.try_clone().unwrap();
                let mut r = BufReader::new(c);
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let body = format!("served {path}");
                let _ = write!(w, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            });
        }
    });
    port
}

/// GETs path from addr, returning the body.
fn get(addr: &str, path: &str) -> std::io::Result<String> {
    let mut c = TcpStream::connect(addr)?;
    c.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(c, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")?;
    let mut resp = String::new();
    c.read_to_string(&mut resp)?;
    Ok(resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

#[test]
fn live_forward() {
    let h = LiveHost::new();
    let port = http_server();
    let (e, out, _) = h.env("");
    let c = Ctx::new();
    let (c2, e2) = (c.clone(), e.clone());
    let spec = format!("0:127.0.0.1:{port}");
    let done = std::thread::spawn(move || block(e2.forward(&c2, &args(&["h", &spec]))).map_err(|e| e.to_string()));
    wait_for(&out, "forwarding ");
    let local = out.string().split_whitespace().nth(1).unwrap().to_string();
    assert_eq!(get(&local, "/one").unwrap(), "served /one");
    // The server restarts: the link reconnects and new forwards work.
    h.restart();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match get(&local, "/two") {
            Ok(b) if b == "served /two" => break,
            r => assert!(Instant::now() < deadline, "no forward after a restart: {r:?}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    c.cancel();
    let start = Instant::now();
    done.join().unwrap().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5), "forward didn't stop");

    assert_eq!(h.run(&["forward", "h"]).0, 2, "forward without specs");
    assert_eq!(h.run(&["forward", "h", "x:y:z"]).0, 1, "bad spec");
    assert_eq!(h.run(&["forward", "nohost", "8080"]).0, 1, "unknown host");
}

/// A refused target is reported per connection; forwarding goes on.
#[test]
fn live_forward_unreachable() {
    let h = LiveHost::new();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (e, out, err) = h.env("");
    let c = Ctx::new();
    let (c2, e2) = (c.clone(), e.clone());
    let spec = format!("0:127.0.0.1:{port}");
    let done = std::thread::spawn(move || block(e2.forward(&c2, &args(&["h", &spec]))).map_err(|e| e.to_string()));
    wait_for(&out, "forwarding ");
    let mut conn = TcpStream::connect(out.string().split_whitespace().nth(1).unwrap()).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut b = [0u8; 1];
    assert_eq!(conn.read(&mut b).unwrap(), 0, "read, want EOF");
    wait_for(&err, "unreachable");
    c.cancel();
    done.join().unwrap().unwrap();
}
