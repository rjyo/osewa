//! quinn's tokio runtime with one change: a socket whose reads keep failing
//! ends its endpoint instead of spinning.
//!
//! quinn's own `poll_recv` (quinn 0.11, runtime/tokio.rs) retries every
//! error but WouldBlock at once, and tokio clears readiness only on
//! WouldBlock. So a socket stuck on an error (an iOS socket defuncted while
//! the app was suspended, say) has recvmsg called in a tight loop. The coop
//! budget lets other tasks run, but the endpoint driver never handles its
//! connections' drained events, so it never exits. That kept a P-core busy
//! for half an hour (rjyo/moshi#690). Here a socket that gives
//! MAX_MISSES reads in a row without a datagram returns the error. The
//! driver ends, its connection dies, and the link redials on a new socket.

use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Instant;

use quinn::udp::{RecvMeta, Transmit, UdpSocketState};
use quinn::{AsyncTimer, AsyncUdpSocket, Runtime, TokioRuntime, UdpPoller};
use tokio::io::Interest;

/// Reads in a row that yield no datagram (an error, or WouldBlock right
/// after the socket polled readable) before the socket counts as stuck. A
/// healthy socket misses once per drain and then reads again; a stuck one
/// reaches this within a millisecond.
pub(crate) const MAX_MISSES: u32 = 256;

#[derive(Debug)]
pub(crate) struct Tokio;

impl Runtime for Tokio {
    fn new_timer(&self, t: Instant) -> Pin<Box<dyn AsyncTimer>> {
        TokioRuntime.new_timer(t)
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        TokioRuntime.spawn(future)
    }

    fn wrap_udp_socket(&self, sock: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Ok(Arc::new(Socket {
            inner: UdpSocketState::new((&sock).into())?,
            io: tokio::net::UdpSocket::from_std(sock)?,
            misses: AtomicU32::new(0),
        }))
    }

    fn now(&self) -> Instant {
        TokioRuntime.now()
    }
}

#[derive(Debug)]
struct Socket {
    io: tokio::net::UdpSocket,
    inner: UdpSocketState,
    misses: AtomicU32,
}

impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Writable { sock: self, fut: None })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.io.try_io(Interest::WRITABLE, || self.inner.send((&self.io).into(), transmit))
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            match self.io.try_io(Interest::READABLE, || self.inner.recv((&self.io).into(), bufs, meta)) {
                Ok(n) => {
                    self.misses.store(0, Ordering::Relaxed);
                    return Poll::Ready(Ok(n));
                }
                Err(e) => {
                    if self.misses.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_MISSES {
                        eprintln!("osewa: quic socket stuck after {MAX_MISSES} failed reads: {e}");
                        let kind = if e.kind() == io::ErrorKind::WouldBlock { io::ErrorKind::BrokenPipe } else { e.kind() };
                        return Poll::Ready(Err(io::Error::new(kind, format!("quic: socket stuck: {e}"))));
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.io.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_gso_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.gro_segments()
    }
}

type WritableFut = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>;

/// A connection's wait for the socket to take a send. It awaits
/// `writable()` rather than calling `poll_send_ready`, which keeps one waker
/// and so would lose all but the last of several connections on a socket.
struct Writable {
    sock: Arc<Socket>,
    fut: Option<WritableFut>,
}

impl fmt::Debug for Writable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writable").finish_non_exhaustive()
    }
}

impl UdpPoller for Writable {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let fut = this.fut.get_or_insert_with(|| {
            let sock = this.sock.clone();
            Box::pin(async move { sock.io.writable().await })
        });
        let r = fut.as_mut().poll(cx);
        if r.is_ready() {
            this.fut = None;
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// A socket whose reads fail for good (its descriptor swapped for
    /// /dev/null while it polls readable) returns an error.
    /// quinn's own runtime never does: it retries until the coop budget
    /// runs out, then again on the next poll, forever.
    async fn stuck_recv(rt: &dyn Runtime) -> Option<io::Result<usize>> {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let fd = sock.as_raw_fd();
        let addr = sock.local_addr().unwrap();
        let s = rt.wrap_udp_socket(sock).unwrap();
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().send_to(b"x", addr).unwrap();
        // Lets the reactor see the datagram, so tokio marks the socket readable.
        let mut buf = [0u8; 2048];
        let mut meta = [RecvMeta::default()];
        std::future::poll_fn(|cx| {
            let _ = s.poll_recv(cx, &mut [IoSliceMut::new(&mut buf)], &mut meta);
            Poll::Ready(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().send_to(b"y", addr).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(unsafe { libc::dup2(null.as_raw_fd(), fd) } >= 0);
        let r = tokio::time::timeout(
            Duration::from_millis(500),
            std::future::poll_fn(|cx| s.poll_recv(cx, &mut [IoSliceMut::new(&mut buf)], &mut meta)),
        )
        .await
        .ok();
        drop(s); // closes /dev/null's copy at fd
        r
    }

    #[tokio::test]
    async fn stuck_socket_errors() {
        let r = stuck_recv(&Tokio).await.expect("returns rather than spinning");
        let e = r.expect_err("a stuck socket is an error");
        assert!(e.to_string().contains("socket stuck"), "{e}");
    }

    #[tokio::test]
    async fn quinn_runtime_spins() {
        assert!(stuck_recv(&TokioRuntime).await.is_none(), "quinn's runtime returned: fixed upstream?");
    }
}
