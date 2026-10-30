//! Channel streams (Go's client/channel.go, PROTOCOL.md "Channel
//! streams"): bytes from the server on channels 1 and 2, bytes to it as
//! DATA. Received data is bounded by the credit returned as the reader
//! consumes, and sends by the server's GRANTs, so neither side queues more
//! than a window.

use std::collections::VecDeque;
use std::fmt;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use osewa_wire as wire;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::AbortHandle;

use crate::conn::{Conn, Stream, CREDIT_DELAY};
use crate::rt::{self, Latch};
use crate::{Error, Result};

/// How much a channel's reader consumes before it returns credit; less goes
/// back after CREDIT_DELAY. Small steps matter: the server keeps uncredited
/// data within an in-flight window as small as 16 KiB, so a larger batch
/// could wait for bytes the server may never send, and slow credit would
/// inflate its round trip samples.
pub(crate) const CHAN_CREDIT_BYTES: usize = 8 << 10;

/// Bounds Exec::close's wait for the server to end the process (SIGHUP,
/// then SIGKILL 2 s later).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Channel {
    c: Conn,
    id: u32,
    me: Weak<Channel>,
    st: Mutex<ChState>,
    /// Set at STREAM_END or connection loss.
    done: Latch,
}

pub(crate) struct ChState {
    /// By channel number; 0 unused.
    bufs: [ChanBuf; 3],
    /// DATA bytes the server still accepts.
    grant: usize,
    /// DATA sent and not yet granted back.
    pub(crate) fly: Flight,
    /// Consumed, not yet credited.
    owed: usize,
    /// Returns owed after CREDIT_DELAY.
    credit_timer: Option<AbortHandle>,
    eof_sent: bool,
    /// Closed locally.
    closed: bool,
    /// The server's STREAM_END.
    pub(crate) end: Option<wire::StreamEnd>,
    /// The connection died first.
    err: Option<Error>,
    wakers: Vec<Waker>,
}

#[derive(Default)]
struct ChanBuf {
    chunks: VecDeque<Vec<u8>>,
    /// Read offset into the first chunk.
    off: usize,
    eof: bool,
}

fn closed_err() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "osewa: use of closed stream")
}

fn conn_err(e: &Error) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, e.clone())
}

impl Channel {
    pub(crate) fn new(c: &Conn, id: u32) -> Arc<Channel> {
        Arc::new_cyclic(|me| Channel {
            c: c.clone(),
            id,
            me: me.clone(),
            st: Mutex::new(ChState {
                bufs: Default::default(),
                grant: wire::CHANNEL_WINDOW as usize,
                fly: Flight::default(),
                owed: 0,
                credit_timer: None,
                eof_sent: false,
                closed: false,
                end: None,
                err: None,
                wakers: Vec::new(),
            }),
            done: Latch::default(),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, ChState> {
        self.st.lock().unwrap()
    }

    fn notify_locked(st: &mut ChState) {
        for w in st.wakers.drain(..) {
            w.wake();
        }
    }

    fn park(st: &mut ChState, cx: &Context<'_>) {
        if !st.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            st.wakers.push(cx.waker().clone());
        }
    }

    /// Returns n counted bytes to the server: at once when enough is owed,
    /// else soon after.
    fn consumed_locked(&self, st: &mut ChState, n: usize) {
        st.owed += n;
        if st.end.is_some() || st.err.is_some() {
            return;
        }
        if st.owed >= CHAN_CREDIT_BYTES {
            self.credit_locked(st);
            return;
        }
        if st.credit_timer.is_none() {
            let me = self.me.clone();
            st.credit_timer = Some(rt::spawn(async move {
                tokio::time::sleep(CREDIT_DELAY).await;
                if let Some(ch) = me.upgrade() {
                    let mut st = ch.lock();
                    st.credit_timer = None;
                    ch.credit_locked(&mut st);
                }
            }));
        }
    }

    fn credit_locked(&self, st: &mut ChState) {
        if let Some(t) = st.credit_timer.take() {
            t.abort();
        }
        if st.owed > 0 && st.end.is_none() && st.err.is_none() {
            let _ = self.c.send(wire::credit(self.id, st.owed as u32));
            st.owed = 0;
        }
    }

    /// Reads from channel n.
    fn poll_read(&self, n: u8, cx: &Context<'_>, p: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut st = self.lock();
        if st.closed {
            return Poll::Ready(Err(closed_err()));
        }
        let st = &mut *st;
        let b = &mut st.bufs[n as usize];
        if let Some(front) = b.chunks.front() {
            if p.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let k = p.len().min(front.len() - b.off);
            p[..k].copy_from_slice(&front[b.off..b.off + k]);
            b.off += k;
            let mut counted = k;
            if b.off == front.len() {
                b.chunks.pop_front();
                b.off = 0;
                // The frame's channel byte.
                counted += 1;
            }
            self.consumed_locked(st, counted);
            return Poll::Ready(Ok(k));
        }
        if b.eof || st.end.is_some() {
            return Poll::Ready(Ok(0));
        }
        if let Some(e) = &st.err {
            return Poll::Ready(Err(conn_err(e)));
        }
        Self::park(st, cx);
        Poll::Pending
    }

    /// Sends what of p the server's grant and the in-flight window allow
    /// now, as DATA.
    fn poll_write(&self, cx: &Context<'_>, p: &[u8]) -> Poll<io::Result<usize>> {
        let mut st = self.lock();
        if st.closed {
            return Poll::Ready(Err(closed_err()));
        }
        if st.eof_sent {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "osewa: write after close_write")));
        }
        if st.end.is_some() {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "osewa: write on an ended stream")));
        }
        if let Some(e) = &st.err {
            return Poll::Ready(Err(conn_err(e)));
        }
        if p.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let room = st.grant.min(st.fly.room().max(0) as usize);
        if room == 0 {
            Self::park(&mut st, cx);
            return Poll::Pending;
        }
        let mut sent = 0;
        let now = Instant::now();
        while sent < p.len().min(room) {
            let n = (p.len().min(room) - sent).min(wire::MAX_CHANNEL_DATA);
            let _ = self.c.send(wire::Frame::new(wire::TYPE_DATA, self.id, p[sent..sent + n].to_vec()));
            st.grant -= n;
            st.fly.sent(n as i64, now);
            sent += n;
        }
        Poll::Ready(Ok(sent))
    }

    /// Sends EOF once.
    fn close_write(&self) -> io::Result<()> {
        let mut st = self.lock();
        if st.closed {
            return Err(closed_err());
        }
        if st.eof_sent || st.end.is_some() {
            return Ok(());
        }
        if let Some(e) = &st.err {
            return Err(conn_err(e));
        }
        st.eof_sent = true;
        self.c.send(wire::Frame::new(wire::TYPE_EOF, self.id, Vec::new())).map_err(|e| conn_err(&e))
    }

    /// Ends the stream locally; true if the server must be asked to
    /// (stream.close) because it hasn't ended it.
    fn close_local(&self) -> bool {
        let mut st = self.lock();
        if st.closed {
            return false;
        }
        st.closed = true;
        Self::notify_locked(&mut st);
        st.end.is_none() && st.err.is_none()
    }

    /// Opens a channel stream with req, once WELCOME says the server offers
    /// feature. Refusing here when it doesn't saves a round trip to the same
    /// bad_request.
    pub(crate) async fn open(c: &Conn, feature: &str, mut req: wire::Request) -> Result<Arc<Channel>> {
        if !c.has(feature).await? {
            return Err(Error::Response {
                code: wire::ERR_BAD_REQUEST.into(),
                message: format!("the server does not offer {feature}"),
            });
        }
        let ch = c.add_stream_with(|id| Channel::new(c, id))?;
        req.stream = ch.id;
        // Dropped before the answer: the server may still open it.
        struct Undo<'a>(&'a Conn, u32, bool);
        impl Drop for Undo<'_> {
            fn drop(&mut self) {
                if self.2 {
                    self.0.drop_stream(self.1);
                    self.0.notify(wire::Request {
                        method: wire::METHOD_STREAM_CLOSE.into(),
                        stream: self.1,
                        ..Default::default()
                    });
                }
            }
        }
        let mut undo = Undo(c, ch.id, true);
        let r = c.request(req, None).await;
        undo.2 = false;
        if let Err(e) = r {
            c.drop_stream(ch.id);
            if !matches!(e, Error::Response { .. }) {
                c.notify(wire::Request { method: wire::METHOD_STREAM_CLOSE.into(), stream: ch.id, ..Default::default() });
            }
            return Err(e);
        }
        Ok(ch)
    }

    fn exit(&self) -> Result<i64> {
        let st = self.lock();
        let Some(end) = &st.end else { return Err(st.err.clone().unwrap_or(Error::Closed(None))) };
        if matches!(end.reason.as_str(), wire::END_EXITED | wire::END_KILLED) {
            if let Some(code) = end.exit_code {
                return Ok(code);
            }
        }
        Err(Error::Other(format!("osewa: exec ended: {}", end.reason)))
    }
}

impl Stream for Channel {
    fn handle(&self, c: &Conn, f: wire::Frame) -> Result<()> {
        let mut st = self.lock();
        match f.ty {
            wire::TYPE_CHANNEL_DATA | wire::TYPE_CHANNEL_EOF => {
                let (n, data) = wire::split_channel(&f.payload).map_err(|e| Error::Other(e.to_string()))?;
                if n != wire::CHANNEL_STDOUT && n != wire::CHANNEL_STDERR {
                    return Err(Error::Other(format!("osewa: channel {n}")));
                }
                if f.ty == wire::TYPE_CHANNEL_EOF {
                    st.bufs[n as usize].eof = true;
                } else if data.is_empty() {
                    // The frame's channel byte counts too; it is owed once the
                    // data is read, or now if there is none.
                    self.consumed_locked(&mut st, 1);
                } else {
                    st.bufs[n as usize].chunks.push_back(data.to_vec());
                }
            }
            wire::TYPE_GRANT => {
                let n = wire::parse_credit(&f.payload).map_err(|e| Error::Other(e.to_string()))?;
                st.grant += n as usize;
                st.fly.granted(n as i64, Instant::now());
            }
            wire::TYPE_STREAM_END => {
                let e: wire::StreamEnd = f.decode().map_err(|e| Error::Other(e.to_string()))?;
                st.end = Some(e);
                c.drop_stream(f.stream);
                self.done.set();
            }
            t => return Err(Error::Other(format!("osewa: unexpected frame 0x{t:02x} on a channel stream"))),
        }
        Self::notify_locked(&mut st);
        Ok(())
    }

    fn lost(&self, c: &Conn) {
        let mut st = self.lock();
        if st.end.is_some() || st.err.is_some() {
            return;
        }
        st.err = Some(c.err().unwrap_or(Error::Closed(None)));
        self.done.set();
        Self::notify_locked(&mut st);
    }

    /// A channel can't move to another connection, so a suspect one is left
    /// to recover or die.
    fn suspect(&self) {}

    fn credits_on_receipt(&self) -> bool {
        false
    }
}

/// A TCP connection the server opened for us, as `ssh -L` does
/// ([`Conn::dial_forward`]). Clones share it: one task may read while
/// another writes. It implements tokio's AsyncRead and AsyncWrite
/// (shutdown half-closes); it ends with its Conn and is not resumed.
#[derive(Clone)]
pub struct Forward {
    ch: Arc<Channel>,
    remote: String,
}

impl fmt::Debug for Forward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Forward").field("remote", &self.remote).finish()
    }
}

impl Forward {
    /// The target as the server dials it.
    pub fn remote_addr(&self) -> &str {
        &self.remote
    }

    pub async fn read(&self, p: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.ch.poll_read(wire::CHANNEL_STDOUT, cx, p)).await
    }

    /// Writes what the window allows now, waiting while it allows nothing.
    pub async fn write(&self, p: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.ch.poll_write(cx, p)).await
    }

    pub async fn write_all(&self, mut p: &[u8]) -> io::Result<()> {
        while !p.is_empty() {
            let n = self.write(p).await?;
            p = &p[n..];
        }
        Ok(())
    }

    /// Half-closes: the server shuts down writing to the target.
    pub fn close_write(&self) -> io::Result<()> {
        self.ch.close_write()
    }

    /// Closes the forwarded connection. It does not wait for the server.
    pub fn close(&self) {
        if self.ch.close_local() {
            self.ch.c.notify(wire::Request { method: wire::METHOD_STREAM_CLOSE.into(), stream: self.ch.id, ..Default::default() });
        }
    }
}

impl AsyncRead for Forward {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        read_into(&self.ch, wire::CHANNEL_STDOUT, cx, buf)
    }
}

impl AsyncWrite for Forward {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.ch.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.ch.close_write())
    }
}

fn read_into(ch: &Channel, n: u8, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    let k = std::task::ready!(ch.poll_read(n, cx, buf.initialize_unfilled()))?;
    buf.advance(k);
    Poll::Ready(Ok(()))
}

/// Describes a command to run on the host without a PTY.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Run directly: ["sh", "-lc", cmd] for a shell command.
    pub argv: Vec<String>,
    /// Empty is the home directory.
    pub cwd: String,
    /// KEY=VALUE, added to the server's environment.
    pub env: Vec<String>,
}

/// A command running on the host. Read stdout and stderr concurrently, as
/// with process pipes: the window is shared, so output left unread on one
/// stops the other.
#[derive(Clone)]
pub struct Exec {
    ch: Arc<Channel>,
}

/// The command's standard input; shutdown sends EOF.
#[derive(Clone)]
pub struct ExecStdin {
    ch: Arc<Channel>,
}

/// The command's standard output.
#[derive(Clone)]
pub struct ExecStdout {
    ch: Arc<Channel>,
}

/// The command's standard error.
#[derive(Clone)]
pub struct ExecStderr {
    ch: Arc<Channel>,
}

/// What a command printed, and its exit code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// 128 + the signal if a signal ended it.
    pub code: i64,
}

impl Exec {
    pub fn stdin(&self) -> ExecStdin {
        ExecStdin { ch: self.ch.clone() }
    }

    pub fn stdout(&self) -> ExecStdout {
        ExecStdout { ch: self.ch.clone() }
    }

    pub fn stderr(&self) -> ExecStderr {
        ExecStderr { ch: self.ch.clone() }
    }

    /// Waits for the command to end and returns its exit code, 128 + the
    /// signal if it was killed by one. It fails if the connection drops
    /// first.
    pub async fn wait(&self) -> Result<i64> {
        self.ch.done.wait().await;
        self.ch.exit()
    }

    /// Ends the command early (SIGHUP to its process group, SIGKILL 2 s
    /// later) and waits for the server to report it gone. Unread output is
    /// dropped.
    pub async fn close(&self) -> Result<()> {
        if !self.ch.close_local() {
            return Ok(());
        }
        let req = wire::Request { method: wire::METHOD_STREAM_CLOSE.into(), stream: self.ch.id, ..Default::default() };
        match rt::timeout(CLOSE_TIMEOUT, self.ch.c.request(req, None)).await {
            None => Err(Error::Io(Arc::new(io::Error::new(io::ErrorKind::TimedOut, "osewa: stream.close timed out")))),
            // It ended on its own meanwhile.
            Some(Err(e)) if e.is_code(wire::ERR_NOT_FOUND) => Ok(()),
            Some(r) => r.map(drop),
        }
    }
}

#[cfg(test)]
impl Exec {
    pub(crate) fn from_channel(ch: Arc<Channel>) -> Exec {
        Exec { ch }
    }

    pub(crate) fn stream_id(&self) -> u32 {
        self.ch.id
    }

    pub(crate) fn is_over(&self) -> bool {
        self.ch.done.is_set()
    }

    /// Wait's answer for a stream already over.
    pub(crate) fn wait_now(&self) -> Result<i64> {
        self.ch.exit()
    }
}

impl ExecStdin {
    pub async fn write(&self, p: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.ch.poll_write(cx, p)).await
    }

    pub async fn write_all(&self, mut p: &[u8]) -> io::Result<()> {
        while !p.is_empty() {
            let n = self.write(p).await?;
            p = &p[n..];
        }
        Ok(())
    }

    /// Closes stdin: the command reads EOF.
    pub fn close(&self) -> io::Result<()> {
        self.ch.close_write()
    }
}

impl AsyncWrite for ExecStdin {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.ch.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.ch.close_write())
    }
}

macro_rules! exec_out {
    ($t:ident, $n:expr) => {
        impl $t {
            pub async fn read(&self, p: &mut [u8]) -> io::Result<usize> {
                poll_fn(|cx| self.ch.poll_read($n, cx, p)).await
            }

            /// Reads to EOF.
            pub async fn read_to_end(&self) -> io::Result<Vec<u8>> {
                let mut out = Vec::new();
                let mut buf = vec![0; 32 << 10];
                loop {
                    let k = self.read(&mut buf).await?;
                    if k == 0 {
                        return Ok(out);
                    }
                    out.extend_from_slice(&buf[..k]);
                }
            }
        }

        impl AsyncRead for $t {
            fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
                read_into(&self.ch, $n, cx, buf)
            }
        }
    };
}

exec_out!(ExecStdout, wire::CHANNEL_STDOUT);
exec_out!(ExecStderr, wire::CHANNEL_STDERR);

impl Conn {
    /// Opens a TCP connection from the server to host:port, as `ssh -L`
    /// does ("localhost" is the server's loopback). It fails with
    /// "unreachable" when the server can't connect, and "bad_request" when
    /// it doesn't offer forwarding.
    pub async fn dial_forward(&self, host: &str, port: u16) -> Result<Forward> {
        let req = wire::Request { method: wire::METHOD_FORWARD_OPEN.into(), host: host.into(), port: port.into(), ..Default::default() };
        let ch = Channel::open(self, wire::FEATURE_FORWARD, req).await?;
        let remote = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        Ok(Forward { ch, remote })
    }

    /// Starts a command. It fails with "not_found" for a missing program
    /// and "bad_request" when the server doesn't offer exec. The command
    /// ends with the connection (the server ends it as for close).
    pub async fn exec(&self, o: ExecOptions) -> Result<Exec> {
        let req = wire::Request { method: wire::METHOD_EXEC_START.into(), argv: o.argv, cwd: o.cwd, env: o.env, ..Default::default() };
        Ok(Exec { ch: Channel::open(self, wire::FEATURE_EXEC, req).await? })
    }

    /// Runs a command with no input and returns what it printed and its
    /// exit code. Dropping the future ends the command.
    pub async fn output(&self, o: ExecOptions) -> Result<Output> {
        let e = self.exec(o).await?;
        struct CloseOnDrop(Option<Exec>);
        impl Drop for CloseOnDrop {
            fn drop(&mut self) {
                if let Some(e) = self.0.take() {
                    rt::spawn(async move {
                        let _ = e.close().await;
                    });
                }
            }
        }
        let mut guard = CloseOnDrop(Some(e.clone()));
        let _ = e.stdin().close();
        let (stdout, stderr) = (e.stdout(), e.stderr());
        let (out, errb) = tokio::join!(stdout.read_to_end(), stderr.read_to_end());
        let code = e.wait().await;
        guard.0 = None;
        let code = code?;
        Ok(Output { stdout: out.map_err(Error::from)?, stderr: errb.map_err(Error::from)?, code })
    }
}

/// In-flight window for DATA (PROTOCOL.md "In flight"). The grant alone is
/// megabytes, seconds of queue on a slow link, and once bytes are queued
/// nothing passes them. So DATA not yet granted back is also kept within
/// 2 x what grants show the link carrying per round trip, timed by the
/// minimum round trip of the last 10 s: about one bandwidth-delay product,
/// so little of it stands in a queue ahead of the next keystroke.
pub(crate) const FLIGHT_MIN: i64 = 16 << 10;
pub(crate) const FLIGHT_MAX: i64 = 4 << 20;
pub(crate) const FLIGHT_INITIAL: i64 = 64 << 10;
const RTT_WINDOW: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(crate) struct Flight {
    pub in_flight: i64,
    pub window: i64,
    /// Sent offsets awaiting a grant, oldest first.
    sends: VecDeque<(Instant, i64)>,
    sent_off: i64,
    ack_off: i64,
    /// (time, round trip) samples within RTT_WINDOW.
    pub rtts: VecDeque<(Instant, Duration)>,
    /// (time, bytes) grants within the last round trip.
    grants: VecDeque<(Instant, i64)>,
}

impl Flight {
    pub fn room(&mut self) -> i64 {
        if self.window == 0 {
            self.window = FLIGHT_INITIAL;
        }
        self.window - self.in_flight
    }

    pub fn sent(&mut self, n: i64, now: Instant) {
        self.sent_off += n;
        self.in_flight += n;
        self.sends.push_back((now, self.sent_off));
    }

    /// Takes n bytes granted back: a round trip sample from the oldest send
    /// it covers, and a new window from what the link carried.
    pub fn granted(&mut self, n: i64, now: Instant) {
        self.ack_off += n;
        self.in_flight = (self.in_flight - n).max(0);
        let mut at = None;
        while let Some(&(t, off)) = self.sends.front() {
            if off > self.ack_off {
                break;
            }
            at = Some(t);
            self.sends.pop_front();
        }
        if let Some(at) = at {
            self.rtts.push_back((now, now.saturating_duration_since(at)));
        }
        while self.rtts.front().is_some_and(|&(t, _)| now.saturating_duration_since(t) > RTT_WINDOW) {
            self.rtts.pop_front();
        }
        let Some(rtt) = self.rtts.iter().map(|&(_, d)| d).min() else { return };
        self.grants.push_back((now, n));
        while self.grants.front().is_some_and(|&(t, _)| now.saturating_duration_since(t) > rtt) {
            self.grants.pop_front();
        }
        let carried: i64 = self.grants.iter().map(|&(_, n)| n).sum();
        self.window = (carried * 2).clamp(FLIGHT_MIN, FLIGHT_MAX);
    }
}
