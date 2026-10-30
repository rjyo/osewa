//! Channel streams (docs/PROTOCOL.md, Channel streams): forwards and
//! processes without a PTY. The server is their far end; no holder is
//! involved.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use osewa_execproc as execproc;
use osewa_wire as wire;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::client::{Client, ClientState};
use crate::conn::LocalBoxFuture;
use crate::flight::{conn_room, Flight};
use crate::queue::Class;

/// Keeps a client's CREDIT from overflowing the counter; what the server
/// sends is bounded by the send queue anyway.
const MAX_CREDIT: i64 = 1 << 40;

/// A forward or exec stream.
pub(crate) struct Channel {
    c: Rc<Client>,
    pub id: u32,
    /// Class::Forward or Class::Exec.
    pub cls: Class,
    /// Fires on every change a task may wait for.
    cond: Notify,
    pub s: RefCell<ChanState>,
}

pub(crate) struct ChanState {
    /// The CHANNEL_DATA payload the client accepts.
    credit: i64,
    /// The DATA the client may still send before a GRANT.
    window: i64,
    pub fl: Flight,
    /// DATA not yet written to the sink.
    inq: VecDeque<Vec<u8>>,
    /// The client sent EOF.
    eof: bool,
    /// stream.close, the connection dropping or the end: read and send
    /// nothing more.
    stopped: bool,
    /// The connection dropped: nothing more is sent.
    gone: bool,
    /// The sink is closed or failed; later DATA is discarded.
    sink_done: bool,
    /// Ending: no more GRANT. GRANT jumps the stream's queue, and one
    /// written after STREAM_END would credit a new stream reusing the id.
    done: bool,
    /// Closes the connection or stops the process, once started.
    kill: Option<Rc<dyn Fn()>>,
    /// The stream's last frame (STREAM_END, or the failed open's response)
    /// is queued; stream.close is answered only after it.
    pushed: bool,
    closers: Vec<u64>,
    /// reaped and signaled tell exited from killed: a process reaped before
    /// stream.close got no signal from us.
    reaped: bool,
    signaled: bool,
    /// The process group was sent SIGHUP; over means the exec stream is
    /// past that point, so a late kill sends nothing.
    hupped: bool,
    over: bool,
}

/// Where a channel's outgoing bytes come from.
trait Source {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> LocalBoxFuture<'a, io::Result<usize>>;
}

/// Where the client's DATA goes.
trait Sink {
    fn write_all<'a>(&'a mut self, b: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>>;
    /// The client's EOF.
    fn half_close(&mut self) -> LocalBoxFuture<'_, ()>;
}

impl Source for tokio::net::tcp::OwnedReadHalf {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> LocalBoxFuture<'a, io::Result<usize>> {
        Box::pin(AsyncReadExt::read(self, buf))
    }
}

impl Sink for tokio::net::tcp::OwnedWriteHalf {
    fn write_all<'a>(&'a mut self, b: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>> {
        Box::pin(AsyncWriteExt::write_all(self, b))
    }
    fn half_close(&mut self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {
            let _ = AsyncWriteExt::shutdown(self).await;
        })
    }
}

impl Source for execproc::PipeReader {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> LocalBoxFuture<'a, io::Result<usize>> {
        Box::pin(execproc::PipeReader::read(self, buf))
    }
}

impl Sink for execproc::PipeWriter {
    fn write_all<'a>(&'a mut self, b: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>> {
        Box::pin(execproc::PipeWriter::write_all(self, b))
    }
    fn half_close(&mut self) -> LocalBoxFuture<'_, ()> {
        self.close();
        Box::pin(async {})
    }
}

/// Counts an exec stream as running for Server::close until dropped.
struct ExecGuard(Rc<Client>);

impl Drop for ExecGuard {
    fn drop(&mut self) {
        self.0.srv.exec_done();
    }
}

impl Channel {
    pub fn new(c: Rc<Client>, id: u32, cls: Class) -> Rc<Channel> {
        Rc::new(Channel {
            c,
            id,
            cls,
            cond: Notify::new(),
            s: RefCell::new(ChanState {
                credit: wire::CHANNEL_WINDOW as i64,
                window: wire::CHANNEL_WINDOW as i64,
                fl: Flight::default(),
                inq: VecDeque::new(),
                eof: false,
                stopped: false,
                gone: false,
                sink_done: false,
                done: false,
                kill: None,
                pushed: false,
                closers: Vec::new(),
                reaped: false,
                signaled: false,
                hupped: false,
                over: false,
            }),
        })
    }

    pub fn wake(&self) {
        self.cond.notify_waiters();
    }

    async fn until(&self, cond: impl Fn(&ChanState) -> bool) {
        while !cond(&self.s.borrow()) {
            self.cond.notified().await;
        }
    }

    fn is_stopped(&self) -> bool {
        self.s.borrow().stopped
    }

    /// Ends the channel because its connection dropped.
    pub fn stop(&self) {
        self.halt(true)
    }

    fn halt(&self, gone: bool) {
        let kill = {
            let mut s = self.s.borrow_mut();
            s.stopped = true;
            s.gone |= gone;
            s.kill.clone()
        };
        self.wake();
        if let Some(kill) = kill {
            kill();
        }
    }

    /// Records how to end the channel. It reports false, after ending it,
    /// when the channel was stopped while starting.
    fn started(&self, kill: Rc<dyn Fn()>) -> bool {
        let stopped = {
            let mut s = self.s.borrow_mut();
            s.kill = Some(kill.clone());
            s.stopped
        };
        if stopped {
            kill();
        }
        !stopped
    }

    /// Answers an open that didn't start, and any stream.close sent
    /// meanwhile. There is no STREAM_END: the stream never existed.
    fn fail_start(self: &Rc<Self>, req_id: u64, code: &str, msg: &str) {
        self.finish();
        self.forget();
        self.c.fail(req_id, code, msg);
        for id in self.last_frame_queued() {
            self.c.respond(wire::Response { id, ..Default::default() });
        }
    }

    /// Marks the channel as ending: nothing more is read, written or
    /// granted.
    pub fn finish(&self) {
        {
            let mut s = self.s.borrow_mut();
            s.done = true;
            s.stopped = true;
        }
        self.wake();
    }

    /// Records that the stream's last frame is queued and returns the
    /// stream.close requests to answer behind it. Later ones are answered
    /// by close_stream itself.
    fn last_frame_queued(&self) -> Vec<u64> {
        let mut s = self.s.borrow_mut();
        s.pushed = true;
        std::mem::take(&mut s.closers)
    }

    fn forget(self: &Rc<Self>) {
        {
            let mut cs = self.c.st.borrow_mut();
            if cs.chans.get(&self.id).is_some_and(|ch| Rc::ptr_eq(ch, self)) {
                cs.chans.remove(&self.id);
            }
        }
        // What it still had in flight will never be credited: the other
        // channels get that room back.
        let out = {
            let mut s = self.s.borrow_mut();
            s.kill = None;
            s.fl.in_flight()
        };
        if out > 0 {
            self.c.flight.borrow_mut().release(out);
            self.c.wake_channels(None);
        }
    }

    /// Sends STREAM_END, then answers stream.close, then frees the stream
    /// id. reserve already treats the id as free once STREAM_END is queued,
    /// so a client that reuses it on seeing STREAM_END never races the
    /// forget.
    pub fn end(self: &Rc<Self>, se: wire::StreamEnd) {
        self.finish();
        let gone = self.s.borrow().gone;
        if !gone {
            self.c.push_stream(self.id, self.cls, wire::json(wire::TYPE_STREAM_END, self.id, &se));
        }
        for id in self.last_frame_queued() {
            if !gone {
                self.c.push_stream(self.id, self.cls, response(id));
            }
        }
        self.forget();
    }

    /// Reports whether the stream's id is free for reuse.
    pub fn ended(&self) -> bool {
        self.s.borrow().pushed
    }

    /// Returns n bytes of window. Not once the stream is ending: GRANT
    /// jumps the stream's queue, and could reach the client after its
    /// STREAM_END, crediting a new stream that reused the id.
    pub fn grant(&self, n: usize) {
        let done = {
            let mut s = self.s.borrow_mut();
            s.window += n as i64;
            s.done
        };
        if !done {
            self.c.push(wire::grant(self.id, n as u32));
        }
    }

    /// Takes a frame the client sent on this stream. An error is a protocol
    /// violation, which closes the connection.
    pub fn frame(self: &Rc<Self>, f: &wire::Frame) -> Result<(), String> {
        let (regrant, r) = self.take(f);
        if regrant > 0 {
            // Nobody will write it: give the window straight back, so a
            // client never waits on a sink that is gone.
            self.grant(regrant);
        }
        if f.ty == wire::TYPE_CREDIT {
            // The connection's window has room again for every channel.
            self.c.wake_channels(Some(self));
        }
        r
    }

    fn take(&self, f: &wire::Frame) -> (usize, Result<(), String>) {
        let r = {
            let mut s = self.s.borrow_mut();
            match f.ty {
                wire::TYPE_CREDIT => {
                    let n = match wire::parse_credit(&f.payload) {
                        Ok(n) => n,
                        Err(e) => return (0, Err(e.to_string())),
                    };
                    s.credit = (s.credit + n as i64).min(MAX_CREDIT);
                    let now = Instant::now();
                    let typing = self.c.typing(now);
                    s.fl.credit(n, now, typing);
                    self.c.flight.borrow_mut().credit(n, now, typing);
                    (0, Ok(()))
                }
                wire::TYPE_DATA => {
                    let n = f.payload.len();
                    if s.eof {
                        return (0, Err(format!("DATA after EOF on stream {}", self.id)));
                    }
                    if n > wire::MAX_CHANNEL_DATA {
                        return (0, Err(format!("DATA over {} bytes", wire::MAX_CHANNEL_DATA)));
                    }
                    if n as i64 > s.window {
                        return (0, Err(format!("DATA beyond GRANT on stream {}", self.id)));
                    }
                    s.window -= n as i64;
                    if s.sink_done || s.stopped {
                        return (n, Ok(()));
                    }
                    s.inq.push_back(f.payload.clone());
                    (0, Ok(()))
                }
                wire::TYPE_EOF => {
                    s.eof = true;
                    (0, Ok(()))
                }
                _ => (0, Ok(())),
            }
        };
        self.wake();
        r
    }

    /// What credit, the channel's window and the connection's all allow.
    fn avail(&self, s: &ChanState) -> i64 {
        let cf = self.c.flight.borrow();
        s.credit.min(s.fl.room()).min(conn_room(&cf, s.fl.window))
    }

    /// Waits until credit and the in-flight windows allow at least one
    /// byte, spends up to want bytes and their channel byte, and returns how
    /// many bytes to send, or 0 once the channel stopped. Spending without
    /// an await between the check and the spend keeps two pumps from both
    /// sending into the same room.
    async fn await_room(&self, want: usize) -> usize {
        loop {
            {
                let mut s = self.s.borrow_mut();
                if s.stopped {
                    return 0;
                }
                let avail = self.avail(&s);
                if avail >= 2 {
                    let m = ((avail - 1) as usize).min(want);
                    s.credit -= m as i64 + 1;
                    let now = Instant::now();
                    s.fl.send(m + 1, now);
                    self.c.flight.borrow_mut().send(m + 1, now);
                    return m;
                }
                if s.credit >= 2 {
                    // Whichever window held it back resizes for that.
                    if s.fl.room() < 2 {
                        s.fl.blocked = true;
                    } else {
                        self.c.flight.borrow_mut().blocked = true;
                    }
                }
            }
            self.cond.notified().await;
        }
    }

    /// Sends src as CHANNEL_DATA on channel n, then CHANNEL_EOF. It reads
    /// one frame ahead of what the client's credit allows and reserves
    /// nothing while it waits in read: exec's stdout and stderr share one
    /// budget, and an idle stderr must not pin room stdout needs. With
    /// stop_reading it also stops reading when the channel stops (a forward,
    /// whose connection the stop closes); a process's pipes are read until
    /// they close, so output after a stop is discarded rather than refused.
    /// It returns the read error that ended it, if not EOF.
    async fn pump(&self, n: u8, src: &mut dyn Source, stop_reading: bool) -> io::Result<()> {
        let mut buf = vec![0u8; wire::MAX_CHANNEL_DATA - 1];
        loop {
            if self.is_stopped() {
                return Ok(());
            }
            let r = if stop_reading {
                tokio::select! {
                    r = src.read(&mut buf) => r,
                    _ = self.until(|s| s.stopped) => return Ok(()),
                }
            } else {
                src.read(&mut buf).await
            };
            let k = *r.as_ref().unwrap_or(&0);
            // The window can be smaller than a frame: send what fits, then
            // wait for room for the rest.
            let mut b = &buf[..k];
            while !b.is_empty() {
                let m = self.await_room(b.len()).await;
                if m == 0 {
                    return Ok(());
                }
                if !self.c.push_stream_wait(self.id, self.cls, wire::channel_data(self.id, n, &b[..m])).await {
                    return Ok(());
                }
                b = &b[m..];
            }
            if k == 0 {
                if !self.is_stopped() {
                    self.c.push_stream(self.id, self.cls, wire::channel_eof(self.id, n));
                }
                return r.map(|_| ());
            }
        }
    }

    /// Writes the client's DATA to w, granting each chunk back once it is
    /// written, and half-closes w after EOF.
    async fn sink(self: Rc<Self>, mut w: Box<dyn Sink>, stop_writing: bool) {
        // failed is a chunk the write failed on. It and every chunk still
        // queued are granted back, as take regrants DATA arriving after
        // this: a client that spent its window on them would otherwise wait
        // forever.
        let mut failed = 0;
        loop {
            self.until(|s| s.stopped || !s.inq.is_empty() || s.eof).await;
            let b = {
                let mut s = self.s.borrow_mut();
                if s.stopped {
                    break;
                }
                s.inq.pop_front()
            };
            let Some(b) = b else {
                w.half_close().await;
                break;
            };
            let ok = if stop_writing {
                tokio::select! {
                    r = w.write_all(&b) => r.is_ok(),
                    _ = self.until(|s| s.stopped) => false,
                }
            } else {
                w.write_all(&b).await.is_ok()
            };
            if !ok {
                failed = b.len();
                break;
            }
            self.grant(b.len());
        }
        drop(w);
        {
            let mut s = self.s.borrow_mut();
            s.sink_done = true;
            failed += s.inq.drain(..).map(|b| b.len()).sum::<usize>();
        }
        self.wake();
        if failed > 0 {
            self.grant(failed);
        }
    }

    async fn forward(self: Rc<Self>, req: wire::Request) {
        let timeout = self.c.srv.cfg.tune.forward_timeout;
        let dial = tokio::select! {
            r = crate::dial::dial_tcp(&req.host, req.port as u16, timeout) => r,
            _ = self.until(|s| s.stopped) => Err("dial tcp: operation was canceled".into()),
        };
        let conn = match dial {
            Ok(c) => c,
            Err(e) => return self.fail_start(req.id, wire::ERR_UNREACHABLE, &e),
        };
        self.c.respond(response_ok(req.id));
        // The pump and the sink stop on their own when the channel stops,
        // and dropping both halves closes the connection.
        if !self.started(Rc::new(|| {})) {
            return self.end(closed());
        }
        let (mut rd, wr) = conn.into_split();
        tokio::task::spawn_local(self.clone().sink(Box::new(wr), true));
        // A reset breaks both directions: stop waiting for the client's EOF.
        if self.pump(wire::CHANNEL_STDOUT, &mut rd, true).await.is_err() {
            self.halt(false);
        }
        self.until(|s| s.sink_done || s.stopped).await;
        drop(rd);
        self.end(closed());
    }

    async fn exec(self: Rc<Self>, req: wire::Request) {
        let _guard = ExecGuard(self.c.clone());
        let opts = execproc::Options { argv: req.argv.clone(), dir: req.cwd.clone(), env: req.env.clone() };
        let mut p = match execproc::Proc::start(opts) {
            Ok(p) => p,
            Err(execproc::Error::NotFound(m)) => return self.fail_start(req.id, wire::ERR_NOT_FOUND, &m),
            Err(e) => return self.fail_start(req.id, wire::ERR_BAD_REQUEST, &e.to_string()),
        };
        self.c.respond(response_ok(req.id));
        let pid = p.pid();
        let (stopper, closer) = (p.stopper(), p.closer());
        let release = closer.clone();
        let (stdin, mut stdout, mut stderr) = (p.take_stdin(), p.take_stdout(), p.take_stderr());
        let reaped = Rc::new(Notify::new());
        let code = Rc::new(std::cell::Cell::new(None::<i32>));
        {
            let (ch, reaped, code) = (self.clone(), reaped.clone(), code.clone());
            tokio::task::spawn_local(async move {
                let (c, _) = p.wait().await;
                code.set(Some(c));
                ch.s.borrow_mut().reaped = true;
                reaped.notify_waiters();
            });
        }
        let wait_reaped = {
            let (reaped, code) = (reaped.clone(), code.clone());
            move || {
                let (reaped, code) = (reaped.clone(), code.clone());
                async move {
                    loop {
                        if let Some(c) = code.get() {
                            return c;
                        }
                        reaped.notified().await;
                    }
                }
            }
        };
        let kill: Rc<dyn Fn()> = {
            let (weak, grace, wait_reaped) = (Rc::downgrade(&self), self.c.srv.cfg.tune.kill_grace, wait_reaped.clone());
            Rc::new(move || {
                let Some(ch) = Weak::upgrade(&weak) else { return };
                {
                    let mut s = ch.s.borrow_mut();
                    // A process already reaped exited on its own. Its group
                    // still gets the signal while members remain; an empty
                    // group's id may already belong to someone else.
                    if s.over {
                    } else if !s.reaped {
                        s.signaled = true;
                        s.hupped = true;
                        stopper.stop(grace);
                    } else if execproc::group_alive(pid) {
                        s.hupped = true;
                        stopper.stop(grace);
                    }
                }
                // Output after a stop is discarded; closing the pipes once
                // the process is gone unblocks readers that a child outside
                // the group, still holding them open, would keep waiting.
                let (wait, closer) = (wait_reaped(), closer.clone());
                tokio::task::spawn_local(async move {
                    wait.await;
                    closer.close();
                });
            })
        };
        self.started(kill);
        if let Some(w) = stdin {
            tokio::task::spawn_local(self.clone().sink(Box::new(w), false));
        }
        let out = async {
            if let Some(r) = stdout.as_mut() {
                let _ = self.pump(wire::CHANNEL_STDOUT, r, false).await;
            }
        };
        let err = async {
            if let Some(r) = stderr.as_mut() {
                let _ = self.pump(wire::CHANNEL_STDERR, r, false).await;
            }
        };
        tokio::join!(out, err);
        let code = wait_reaped().await;
        let reason = if self.s.borrow().signaled { wire::END_KILLED } else { wire::END_EXITED };
        // The pipes go with the process: a sink blocked on a stdin that a
        // background child holds and never reads would wait forever.
        release.close();
        drop((stdout, stderr));
        self.end(wire::StreamEnd { reason: reason.into(), exit_code: Some(code as i64) });
        let hupped = {
            let mut s = self.s.borrow_mut();
            s.over = true;
            s.hupped
        };
        if hupped {
            // The leader can die on SIGHUP and be reaped while members that
            // ignore it live on until execproc's SIGKILL. Keep this stream
            // counted until the group is gone, or Server::close returns and
            // `osewa server` exits before that SIGKILL fires.
            await_group(pid, self.c.srv.cfg.tune.group_grace).await;
        }
    }
}

/// Waits until process group pgid has no members, zombies included, or d
/// has passed.
async fn await_group(pgid: i32, d: Duration) {
    let deadline = Instant::now() + d;
    while execproc::group_alive(pgid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn response_ok(id: u64) -> wire::Response {
    wire::Response { id, ..Default::default() }
}

fn response(id: u64) -> wire::Frame {
    wire::json(wire::TYPE_RESPONSE, 0, &response_ok(id))
}

fn closed() -> wire::StreamEnd {
    wire::StreamEnd { reason: wire::END_CLOSED.into(), exit_code: None }
}

impl Client {
    /// Handles forward.open and exec.start.
    pub fn open(self: &Rc<Self>, req: wire::Request) {
        let (feature, cls) = if req.method == wire::METHOD_EXEC_START {
            (wire::FEATURE_EXEC, Class::Exec)
        } else {
            (wire::FEATURE_FORWARD, Class::Forward)
        };
        let bad = if !self.has(feature) {
            Some(format!("feature {feature} is not enabled"))
        } else if req.stream == 0 {
            Some("stream must be nonzero".into())
        } else if cls == Class::Forward && (req.host.is_empty() || req.port < 1 || req.port > 65535) {
            Some("forward.open needs host and a port 1..65535".into())
        } else if cls == Class::Exec && req.argv.is_empty() {
            Some("exec.start needs argv".into())
        } else {
            None
        };
        if let Some(msg) = bad {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, &msg);
        }
        let ch = Channel::new(self.clone(), req.stream, cls);
        let add = ch.clone();
        let srv = self.srv.clone();
        // Counted as it is added, before close can take the map: close's
        // wait then never misses it.
        if !self.reserve(&req, |s: &mut ClientState| {
            s.chans.insert(req.stream, add);
            if cls == Class::Exec {
                srv.exec_started();
            }
        }) {
            return;
        }
        if cls == Class::Forward {
            tokio::task::spawn_local(ch.forward(req));
        } else {
            tokio::task::spawn_local(ch.exec(req));
        }
    }

    /// Handles stream.close.
    pub fn close_stream(&self, req: wire::Request) {
        let Some(ch) = self.channel(req.stream) else {
            return if self.stream(req.stream).is_some() {
                self.fail(req.id, wire::ERR_BAD_REQUEST, "not a channel stream; use session.detach")
            } else {
                self.fail(req.id, wire::ERR_NOT_FOUND, "no such stream")
            };
        };
        let done = {
            let mut s = ch.s.borrow_mut();
            if s.pushed {
                drop(s);
                // The STREAM_END is queued already; answer behind it.
                self.push_stream(ch.id, ch.cls, response(req.id));
                return;
            }
            s.closers.push(req.id);
            s.done
        };
        if !done {
            ch.halt(false);
        }
    }
}
