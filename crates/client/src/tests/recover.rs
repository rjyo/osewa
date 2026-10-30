//! recover_test.go and proxy_test.go

use std::sync::atomic::{AtomicBool, Ordering};

use super::robust::dial_ping;
use super::*;

/// Forwards TCP to target and can black-hole the connections it has: they
/// stay open but carry nothing, like a half-open connection after a network
/// change. New connections work.
pub(super) struct Proxy {
    addr: std::net::SocketAddr,
    conns: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
}

impl Proxy {
    pub fn new(target: std::net::SocketAddr) -> Proxy {
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap();
        let conns: Arc<Mutex<Vec<Arc<AtomicBool>>>> = Arc::default();
        let cs = conns.clone();
        std::thread::spawn(move || {
            for c in ln.incoming() {
                let Ok(c) = c else { return };
                let Ok(up) = std::net::TcpStream::connect(target) else { continue };
                let frozen = Arc::new(AtomicBool::new(false));
                cs.lock().unwrap().push(frozen.clone());
                let (c2, up2, f2) = (c.try_clone().unwrap(), up.try_clone().unwrap(), frozen.clone());
                std::thread::spawn(move || pipe(c, up, frozen));
                std::thread::spawn(move || pipe(up2, c2, f2));
            }
        });
        Proxy { addr, conns }
    }

    /// Freezes every connection made so far.
    pub fn blackhole(&self) {
        for c in self.conns.lock().unwrap().iter() {
            c.store(true, Ordering::SeqCst);
        }
    }

    /// cfg pointed at the proxy instead of the fake server.
    pub fn through(&self, mut cfg: Config) -> Config {
        cfg.url = format!("wss://{}{}", self.addr, wire::PATH);
        cfg
    }
}

fn pipe(mut src: std::net::TcpStream, mut dst: std::net::TcpStream, frozen: Arc<AtomicBool>) {
    use std::io::{Read, Write};
    let mut buf = vec![0; 32 << 10];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = dst.shutdown(std::net::Shutdown::Write);
                return;
            }
            Ok(n) => {
                if !frozen.load(Ordering::SeqCst) && dst.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

fn dial_via(p: &Proxy, cfg: Config) -> Conn {
    let c = run(Conn::dial(p.through(cfg))).unwrap();
    run(c.welcome()).unwrap();
    c
}

/// Typing on a connection that went silent makes it suspect within about a
/// second, long before keepalive declares it dead, and resuming on a fresh
/// connection delivers the input once.
#[test]
fn suspect_after_unanswered_input() {
    let f = Fake::new();
    let p = Proxy::new(f.addr);
    let id = f.create(40, 10);
    let c = dial_via(&p, f.config("a"));
    let s = attach(&c, &id, own());
    s.send_text("first").unwrap();
    run(s.flush()).unwrap();

    p.blackhole();
    let start = Instant::now();
    s.send_text("second").unwrap();
    wait_for("lost", s.lost());
    let d = start.elapsed();
    assert!(d <= Duration::from_millis(1500), "suspect after {d:?}");
    assert!(c.is_suspect(), "conn not suspect");
    assert!(!c.is_done(), "suspect conn already closed: it may still recover");

    run(s.resume(&dial_via(&p, f.config("a")))).unwrap();
    run(s.flush()).unwrap();
    f.output(&id, "fresh");
    s.wait_text("fresh");
    let d = start.elapsed();
    assert!(d <= Duration::from_millis(1500), "screen fresh after {d:?}");
    f.with(&id, |fs| assert!(fs.applied.len() == 2 && fs.applied[1].text == "second", "applied {:?}", fs.applied));
    c.close_now();
}

/// Idle, the regular ping finds a silent connection.
#[test]
fn suspect_when_idle() {
    let f = Fake::new();
    let p = Proxy::new(f.addr);
    let id = f.create(40, 10);
    let mut cfg = f.config("a");
    cfg.knobs.ping = Some(Duration::from_millis(200));
    let c = dial_via(&p, cfg);
    let s = attach(&c, &id, Default::default());
    p.blackhole();
    let start = Instant::now();
    wait_for("lost", s.lost());
    let d = start.elapsed();
    assert!(d <= Duration::from_secs(2), "suspect after {d:?}");
    c.close_now();
}

/// A healthy connection never turns suspect from typing.
#[test]
fn no_suspect_on_healthy_conn() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = dial_ping(&f, "a", Duration::from_secs(5));
    let s = attach(&c, &id, own());
    for _ in 0..20 {
        s.send_text("x").unwrap();
        sleep(Duration::from_millis(10));
    }
    assert!(run_for(Duration::from_millis(1500), s.lost()).is_none(), "healthy connection reported lost");
}
