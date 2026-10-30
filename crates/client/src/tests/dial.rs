//! dial_test.go

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::trace::{find, read_trace};
use super::*;
use crate::dial::{Io, LocalBoxFuture};
use crate::hosts::testutil::TempDir;

type Step = Box<dyn Fn(&Arc<FakeNet>, String) -> LocalBoxFuture<io::Result<Box<dyn Io>>> + Send + Sync>;

/// Scripts each TCP connect attempt in order; attempts past the script dial
/// for real.
#[derive(Default)]
struct FakeNet {
    script: Mutex<Vec<Step>>,
    calls: Mutex<usize>,
    /// A stalled attempt saw itself cancelled.
    stalled: Arc<AtomicBool>,
    /// (read delay, closed) of every real connection, in connect order.
    conns: Mutex<Vec<(Duration, Arc<AtomicBool>)>>,
}

impl FakeNet {
    fn new(script: Vec<Step>) -> Arc<FakeNet> {
        Arc::new(FakeNet { script: Mutex::new(script), ..Default::default() })
    }

    fn dialer(self: &Arc<Self>) -> crate::dial::TcpDial {
        let n = self.clone();
        Arc::new(move |addr| {
            let i = {
                let mut c = n.calls.lock().unwrap();
                *c += 1;
                *c - 1
            };
            let script = n.script.lock().unwrap();
            match script.get(i) {
                Some(step) => step(&n, addr),
                None => Box::pin(real(n.clone(), addr, Duration::ZERO)),
            }
        })
    }

    fn count(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

async fn real(n: Arc<FakeNet>, addr: String, delay: Duration) -> io::Result<Box<dyn Io>> {
    let c = tokio::net::TcpStream::connect(addr).await?;
    let closed = Arc::new(AtomicBool::new(false));
    n.conns.lock().unwrap().push((delay, closed.clone()));
    let delay = (!delay.is_zero()).then(|| Box::pin(tokio::time::sleep(delay)));
    Ok(Box::new(Tracked { inner: c, delay, closed }))
}

/// A connect whose SYN is lost: it never completes on its own.
fn stall() -> Step {
    Box::new(|n, _| {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = Flag(n.stalled.clone());
        Box::pin(async move {
            let _flag = flag;
            std::future::pending().await
        })
    })
}

/// Connects at once, but the peer's first bytes arrive late.
fn slow_tls(d: Duration) -> Step {
    Box::new(move |n, addr| Box::pin(real(n.clone(), addr, d)))
}

/// Completes the TCP connect after d.
fn slow_connect(d: Duration) -> Step {
    Box::new(move |n, addr| {
        let n = n.clone();
        Box::pin(async move {
            tokio::time::sleep(d).await;
            real(n, addr, Duration::ZERO).await
        })
    })
}

struct Tracked {
    inner: tokio::net::TcpStream,
    delay: Option<Pin<Box<tokio::time::Sleep>>>,
    closed: Arc<AtomicBool>,
}

impl AsyncRead for Tracked {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Some(d) = self.delay.as_mut() {
            std::task::ready!(d.as_mut().poll(cx));
            self.delay = None;
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Tracked {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

use std::future::Future;

fn dial_with(f: &Fake, n: &Arc<FakeNet>, after: Duration, trace: Option<&std::path::Path>) -> (Result<Conn>, Duration) {
    let mut cfg = f.config("a");
    cfg.race_after = Some(after);
    cfg.knobs.dial = Some(n.dialer());
    cfg.knobs.trace_dir = trace.map(Into::into);
    let start = Instant::now();
    let r = run(async {
        let c = Conn::dial(cfg).await?;
        if let Err(e) = c.welcome().await {
            c.close_now();
            return Err(e);
        }
        Ok(c)
    });
    (r, start.elapsed())
}

/// A first connect that never completes (its SYN lost) is raced by a second
/// one after race_after, instead of waiting out TCP's 1 s retransmit.
#[test]
fn dial_races_stalled_connect() {
    let dir = TempDir::new();
    let f = Fake::new();
    let n = FakeNet::new(vec![stall()]);
    let (c, took) = dial_with(&f, &n, Duration::from_millis(50), Some(dir.path()));
    let c = c.unwrap();
    assert!(took <= Duration::from_millis(900), "dial took {took:?}");
    assert_eq!(n.count(), 2, "connect attempts");
    eventually("stalled attempt cancelled", || n.stalled.load(Ordering::SeqCst));
    run(c.list()).unwrap();
    assert!(find(&read_trace(dir.path()), "dial", |_| true).is_some(), "no dial trace event");
    c.close_now();
}

/// The race also covers reconnects: a resume over a raced dial.
#[test]
fn resume_over_raced_dial() {
    let f = Fake::new();
    let id = f.create(40, 10);
    let s = attach(&f.dial("a"), &id, Default::default());
    f.output(&id, "before\r\n");
    s.wait_text("before");
    f.drop_all();
    eventually("disconnect", || !s.connected());
    f.output(&id, "after\r\n");

    let n = FakeNet::new(vec![stall()]);
    let (c, took) = dial_with(&f, &n, Duration::from_millis(50), None);
    let c = c.unwrap();
    run(s.resume(&c)).unwrap();
    s.wait_text("after");
    assert!(took <= Duration::from_millis(900) && n.count() == 2, "dial took {took:?} over {} attempts", n.count());
}

/// Once TCP is through, a slow TLS handshake is left alone: the
/// connection's own retransmits cover it, and a second handshake would only
/// add load.
#[test]
fn dial_no_race_once_connected() {
    let f = Fake::new();
    let n = FakeNet::new(vec![slow_tls(Duration::from_millis(150))]);
    let (c, _) = dial_with(&f, &n, Duration::from_millis(20), None);
    c.unwrap().close_now();
    assert_eq!(n.count(), 1, "connect attempts");
}

/// If the first attempt connects after the race started, whichever finishes
/// TLS first wins and the other connection is closed.
#[test]
fn dial_first_tls_wins() {
    let f = Fake::new();
    // The first connects at 60 ms; the second, started at 20 ms, connects at
    // once but its TLS answer is held for 2 s.
    let n = FakeNet::new(vec![slow_connect(Duration::from_millis(60)), slow_tls(Duration::from_secs(2))]);
    let (c, took) = dial_with(&f, &n, Duration::from_millis(20), None);
    let c = c.unwrap();
    assert!(took <= Duration::from_secs(1), "dial took {took:?}, waited for the slow handshake");
    assert_eq!(n.count(), 2, "connect attempts");
    let conns = n.conns.lock().unwrap().clone();
    assert_eq!(conns.len(), 2, "connections");
    let (loser, winner) = if conns[0].0.is_zero() { (&conns[1], &conns[0]) } else { (&conns[0], &conns[1]) };
    eventually("loser closed", || loser.1.load(Ordering::SeqCst));
    assert!(!winner.1.load(Ordering::SeqCst), "winner closed");
    run(c.list()).unwrap();
    c.close_now();
}

/// A zero race_after never races: a slow connect is waited for.
#[test]
fn dial_race_disabled() {
    let f = Fake::new();
    let n = FakeNet::new(vec![slow_connect(Duration::from_millis(150))]);
    let (c, took) = dial_with(&f, &n, Duration::ZERO, None);
    c.unwrap().close_now();
    assert!(n.count() == 1 && took >= Duration::from_millis(150), "{} attempts in {took:?}, want 1 after 150ms", n.count());
}

/// A connect that fails outright before the race (refused) is not retried.
#[test]
fn dial_refused_not_raced() {
    let f = Fake::new();
    let refused: Step = Box::new(|_, _| Box::pin(async { Err(io::Error::from(io::ErrorKind::ConnectionRefused)) }));
    let n = FakeNet::new(vec![refused]);
    let (c, _) = dial_with(&f, &n, Duration::from_millis(50), None);
    let err = c.unwrap_err();
    assert_eq!(err.io_kind(), Some(io::ErrorKind::ConnectionRefused), "err = {err}");
    sleep(Duration::from_millis(100));
    assert_eq!(n.count(), 1, "connect attempts");
}

/// When both attempts fail, both errors are reported.
#[test]
fn dial_both_fail() {
    let f = Fake::new();
    let first: Step = Box::new(|_, _| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Err(io::Error::other("first lost"))
        })
    });
    let second: Step = Box::new(|_, _| Box::pin(async { Err(io::Error::other("second refused")) }));
    let n = FakeNet::new(vec![first, second]);
    let (c, _) = dial_with(&f, &n, Duration::from_millis(20), None);
    let err = c.unwrap_err().to_string();
    assert!(err.contains("first lost") && err.contains("second refused"), "err = {err}, want both attempts' errors");
}

#[test]
fn parse_urls() {
    use crate::dial::{parse_url, Target};
    let t = |a: &str, h: &str, p: u16| Target { authority: a.into(), host: h.into(), port: p };
    assert_eq!(parse_url("wss://h:1/osewa").unwrap(), t("h:1", "h", 1));
    assert_eq!(parse_url("wss://h/osewa").unwrap(), t("h:443", "h", 443));
    assert_eq!(parse_url("wss://[::1]:7/osewa").unwrap(), t("[::1]:7", "::1", 7));
    for bad in ["ws://h:1/osewa", "wss://:1/osewa", "wss://h:x/osewa", "https://h"] {
        assert!(parse_url(bad).is_err(), "{bad}");
    }
}
