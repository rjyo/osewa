//! TCP dials for forward.open: the deadline
//! split across a name's addresses, and IPv6 raced against IPv4 (RFC 6555)
//! so that an address family that blackholes costs 300 ms, not the whole
//! deadline.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use osewa_execproc::error_text;
use tokio::time::Instant;

/// How long the first address family has before the other starts beside
/// it (RFC 6555's "Happy Eyeballs" delay).
const FALLBACK_DELAY: Duration = Duration::from_millis(300);
/// The least an address gets of the deadline, when that much is left
/// so that one slow address can't leave the next none.
const SANE_MINIMUM: Duration = Duration::from_secs(2);

/// Dials host:port within timeout, with errors worded as v0 servers word
/// them ("dial tcp ...: connect: ..."), since clients show them.
pub(crate) async fn dial_tcp(host: &str, port: u16, timeout: Duration) -> Result<tokio::net::TcpStream, String> {
    let deadline = Instant::now() + timeout;
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => match tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port))).await {
            Err(_) => return Err(format!("dial tcp: lookup {host}: i/o timeout")),
            Ok(Err(_)) => return Err(format!("dial tcp: lookup {host}: no such host")),
            Ok(Ok(a)) => a.collect(),
        },
    };
    if addrs.is_empty() {
        return Err(format!("dial tcp: lookup {host}: no such host"));
    }
    dial_parallel(&addrs, deadline, tokio::net::TcpStream::connect::<SocketAddr>).await
}

/// Races the first address family's addresses (in order) against the
/// other's, which start FALLBACK_DELAY later or as soon as the first family
/// has failed. The first connection wins; with none, the first family's
/// error.
pub(crate) async fn dial_parallel<T, F, Fut>(addrs: &[SocketAddr], deadline: Instant, connect: F) -> Result<T, String>
where
    F: Fn(SocketAddr) -> Fut + Copy,
    Fut: Future<Output = io::Result<T>>,
{
    let v6 = addrs[0].is_ipv6();
    let (primaries, fallbacks): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs.iter().partition(|a| a.is_ipv6() == v6);
    if fallbacks.is_empty() {
        return dial_serial(&primaries, deadline, connect).await;
    }
    let primary = dial_serial(&primaries, deadline, connect);
    tokio::pin!(primary);
    let primary_err = tokio::select! {
        r = &mut primary => match r {
            Ok(c) => return Ok(c),
            Err(e) => Some(e),
        },
        _ = tokio::time::sleep(FALLBACK_DELAY) => None,
    };
    let fallback = dial_serial(&fallbacks, deadline, connect);
    tokio::pin!(fallback);
    match primary_err {
        Some(e) => fallback.await.map_err(|_| e),
        None => tokio::select! {
            r = &mut primary => match r {
                Ok(c) => Ok(c),
                Err(e) => fallback.await.map_err(|_| e),
            },
            r = &mut fallback => match r {
                Ok(c) => Ok(c),
                Err(_) => primary.await,
            },
        },
    }
}

/// Tries addrs in order, each with an equal share of what is left of the
/// deadline (but at least SANE_MINIMUM of it), and returns the first
/// connection or the first error.
async fn dial_serial<T, F, Fut>(addrs: &[SocketAddr], deadline: Instant, connect: F) -> Result<T, String>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let mut first = None;
    for (i, &a) in addrs.iter().enumerate() {
        let left = deadline.saturating_duration_since(Instant::now());
        let err = if left.is_zero() {
            format!("dial tcp {a}: i/o timeout")
        } else {
            let mut share = left / (addrs.len() - i) as u32;
            if share < SANE_MINIMUM {
                share = SANE_MINIMUM.min(left);
            }
            let start = Instant::now();
            match tokio::time::timeout(share, connect(a)).await {
                // A connect that completes past its deadline failed too.
                Ok(Ok(c)) if start.elapsed() <= share => return Ok(c),
                Ok(Ok(_)) | Err(_) => format!("dial tcp {a}: i/o timeout"),
                Ok(Err(e)) => format!("dial tcp {a}: connect: {}", error_text(&e)),
            }
        };
        first.get_or_insert(err);
    }
    Err(first.unwrap_or_else(|| "dial tcp: no addresses".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn paused<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
        rt.block_on(f)
    }

    /// A connect where addresses in hang never answer, those in refuse are
    /// refused, and the rest connect; it records each attempt and when.
    #[derive(Clone)]
    struct Fake {
        hang: Vec<SocketAddr>,
        refuse: Vec<SocketAddr>,
        log: Rc<RefCell<Vec<(SocketAddr, Duration)>>>,
        t0: Instant,
    }

    impl Fake {
        fn new(hang: &[&str], refuse: &[&str]) -> Fake {
            Fake {
                hang: hang.iter().map(|s| addr(s)).collect(),
                refuse: refuse.iter().map(|s| addr(s)).collect(),
                log: Rc::default(),
                t0: Instant::now(),
            }
        }

        async fn connect(&self, a: SocketAddr) -> io::Result<SocketAddr> {
            self.log.borrow_mut().push((a, self.t0.elapsed()));
            if self.hang.contains(&a) {
                std::future::pending::<()>().await;
            }
            if self.refuse.contains(&a) {
                return Err(io::Error::from_raw_os_error(libc::ECONNREFUSED));
            }
            Ok(a)
        }
    }

    // An IPv6 address that blackholes costs the fallback delay, not the
    // deadline.
    #[test]
    fn falls_back_to_ipv4() {
        paused(async {
            let f = Fake::new(&["[::1]:80"], &[]);
            let t0 = Instant::now();
            let ff = &f;
            let r = dial_parallel(&[addr("[::1]:80"), addr("127.0.0.1:80")], t0 + Duration::from_secs(10), |a| ff.connect(a)).await;
            assert_eq!(r.unwrap(), addr("127.0.0.1:80"));
            assert_eq!(t0.elapsed(), FALLBACK_DELAY);
        });
    }

    // A first family that fails starts the other at once.
    #[test]
    fn failure_starts_fallback_at_once() {
        paused(async {
            let f = Fake::new(&[], &["[::1]:80"]);
            let t0 = Instant::now();
            let ff = &f;
            let r = dial_parallel(&[addr("[::1]:80"), addr("127.0.0.1:80")], t0 + Duration::from_secs(10), |a| ff.connect(a)).await;
            assert_eq!(r.unwrap(), addr("127.0.0.1:80"));
            assert_eq!(t0.elapsed(), Duration::ZERO);
        });
    }

    // Addresses of one family share the deadline: a hanging first one
    // leaves the second its turn.
    #[test]
    fn splits_the_deadline() {
        paused(async {
            let f = Fake::new(&["10.0.0.1:80"], &[]);
            let t0 = Instant::now();
            let ff = &f;
            let r = dial_parallel(&[addr("10.0.0.1:80"), addr("10.0.0.2:80")], t0 + Duration::from_secs(10), |a| ff.connect(a)).await;
            assert_eq!(r.unwrap(), addr("10.0.0.2:80"));
            assert_eq!(f.log.borrow()[1], (addr("10.0.0.2:80"), Duration::from_secs(5)));
        });
    }

    // With nothing connecting, the error is the first family's first.
    #[test]
    fn reports_the_first_error() {
        paused(async {
            let f = Fake::new(&[], &["[::1]:80", "127.0.0.1:80"]);
            let ff = &f;
            let r = dial_parallel(&[addr("[::1]:80"), addr("127.0.0.1:80")], Instant::now() + Duration::from_secs(10), |a| {
                ff.connect(a)
            })
            .await;
            assert_eq!(r.unwrap_err(), "dial tcp [::1]:80: connect: connection refused");
            let f = Fake::new(&["10.0.0.1:80"], &[]);
            let ff = &f;
            let r = dial_parallel(&[addr("10.0.0.1:80")], Instant::now() + Duration::from_secs(1), |a| ff.connect(a)).await;
            assert_eq!(r.unwrap_err(), "dial tcp 10.0.0.1:80: i/o timeout");
        });
    }
}
