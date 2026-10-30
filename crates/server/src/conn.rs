//! The carrier seam: what the server needs from a client connection. A
//! WebSocket (docs/PROTOCOL.md) or QUIC's main stream (docs/QUIC.md) moves
//! one frame per message either way.

use std::future::Future;
use std::io;
use std::pin::Pin;

pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// WebSocket close codes the server ends connections with; QUIC maps them
/// to application error codes.
pub const CLOSE_NORMAL: u16 = 1000;
pub const CLOSE_GOING_AWAY: u16 = 1001;
pub const CLOSE_POLICY_VIOLATION: u16 = 1008;

/// One client connection. Reads, writes and close may run concurrently from
/// tasks on one thread.
pub trait Conn {
    /// Reads one frame.
    fn read(&self) -> LocalBoxFuture<'_, io::Result<Vec<u8>>>;
    /// Writes one frame.
    fn write<'a>(&'a self, frame: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>>;
    /// Ends the connection with a close code and reason. Reads and writes
    /// fail from the moment it is called.
    fn close(&self, code: u16, reason: String) -> LocalBoxFuture<'_, ()>;
    /// The kernel's view of the path, where the carrier is a TCP socket.
    fn path(&self, _now: std::time::Instant) -> Option<crate::path::Sample> {
        None
    }
}

/// A connection that goes nowhere, for clients built without a peer.
#[cfg(test)]
pub(crate) struct NullConn;

#[cfg(test)]
impl Conn for NullConn {
    fn read(&self) -> LocalBoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(std::future::pending())
    }
    fn write<'a>(&'a self, _: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn close(&self, _: u16, _: String) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// An in-memory connection pair for tests: the server's end is a Conn, the
/// client's a Peer. Each direction holds a few messages, so a peer that
/// stops reading pushes back on the server's writer as a socket would.
#[cfg(test)]
pub(crate) mod mem {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use tokio::sync::{mpsc, Mutex, Notify};

    pub enum Msg {
        Data(Vec<u8>),
        Close(u16),
    }

    const DEPTH: usize = 64;

    struct Shared {
        closed: Cell<bool>,
        notify: Notify,
        /// What the server's end reports as its kernel's view of the path.
        path: Cell<Option<crate::path::Sample>>,
    }

    impl Shared {
        async fn closed(&self) {
            while !self.closed.get() {
                self.notify.notified().await;
            }
        }
        fn close(&self) {
            self.closed.set(true);
            self.notify.notify_waiters();
        }
    }

    pub struct ServerEnd {
        rx: Mutex<mpsc::Receiver<Msg>>,
        tx: mpsc::Sender<Msg>,
        sh: Rc<Shared>,
    }

    pub struct Peer {
        pub rx: mpsc::Receiver<Msg>,
        pub tx: mpsc::Sender<Msg>,
        sh: Rc<Shared>,
    }

    fn closed_err() -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionAborted, "connection closed")
    }

    pub fn pair() -> (Rc<ServerEnd>, Peer) {
        let (ctx, srx) = mpsc::channel(DEPTH);
        let (stx, crx) = mpsc::channel(DEPTH);
        let sh = Rc::new(Shared { closed: Cell::new(false), notify: Notify::new(), path: Cell::new(None) });
        (Rc::new(ServerEnd { rx: Mutex::new(srx), tx: stx, sh: sh.clone() }), Peer { rx: crx, tx: ctx, sh })
    }

    impl Conn for ServerEnd {
        fn read(&self) -> LocalBoxFuture<'_, io::Result<Vec<u8>>> {
            Box::pin(async move {
                if self.sh.closed.get() {
                    return Err(closed_err());
                }
                let mut rx = self.rx.lock().await;
                tokio::select! {
                    m = rx.recv() => match m {
                        Some(Msg::Data(b)) => Ok(b),
                        _ => Err(io::ErrorKind::UnexpectedEof.into()),
                    },
                    _ = self.sh.closed() => Err(closed_err()),
                }
            })
        }

        fn write<'a>(&'a self, frame: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                if self.sh.closed.get() {
                    return Err(closed_err());
                }
                tokio::select! {
                    r = self.tx.send(Msg::Data(frame.to_vec())) => r.map_err(|_| closed_err()),
                    _ = self.sh.closed() => Err(closed_err()),
                }
            })
        }

        fn path(&self, _now: std::time::Instant) -> Option<crate::path::Sample> {
            self.sh.path.get()
        }

        fn close(&self, code: u16, _: String) -> LocalBoxFuture<'_, ()> {
            let first = !self.sh.closed.get();
            self.sh.close();
            if first {
                let _ = self.tx.try_send(Msg::Close(code));
            }
            Box::pin(async {})
        }
    }

    impl Peer {
        /// Reads the server's next message; Close once it has closed.
        pub async fn recv(&mut self) -> Msg {
            tokio::select! {
                biased;
                m = self.rx.recv() => m.unwrap_or(Msg::Close(1006)),
                _ = self.sh.closed() => Msg::Close(1006),
            }
        }

        pub async fn send(&self, b: Vec<u8>) -> io::Result<()> {
            if self.sh.closed.get() {
                return Err(closed_err());
            }
            tokio::select! {
                r = self.tx.send(Msg::Data(b)) => r.map_err(|_| closed_err()),
                _ = self.sh.closed() => Err(closed_err()),
            }
        }

        /// Sets what the server's end reports as its path.
        pub fn set_path(&self, s: Option<crate::path::Sample>) {
            self.sh.path.set(s);
        }

        /// Drops the connection, as a client process exiting does.
        pub fn close(&mut self) {
            self.sh.close();
            self.rx.close();
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            self.sh.close();
        }
    }
}
