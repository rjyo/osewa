//! One client connection: its requests, its session streams and their
//! holder connections.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use osewa_wire as wire;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Notify;

use crate::channel::Channel;
use crate::conn::{Conn, CLOSE_GOING_AWAY, CLOSE_NORMAL, CLOSE_POLICY_VIOLATION};
use crate::flight::Flight;
use crate::limits::{err_size, valid_client_id, valid_size};
use crate::path;
use crate::queue::{Class, SendQueue};
use crate::{read_frame, Server};

/// How long after a keystroke the connection counts as typed on: its
/// forwards and execs keep their in-flight window tight.
const TYPING_SPAN: Duration = Duration::from_secs(2);

/// The largest exec DATA that counts as typing: keys and short pastes, not
/// an upload's full frames.
const TYPED_FRAME_MAX: usize = 256;

pub(crate) struct Client {
    pub srv: Rc<Server>,
    pub(crate) conn: Rc<dyn Conn>,
    pub send: SendQueue,
    /// client_id from HELLO.
    pub id: RefCell<String>,
    /// The features both HELLO and WELCOME list.
    features: RefCell<Vec<String>>,
    closed: Cell<bool>,
    cancel: Notify,
    pub st: RefCell<ClientState>,
    /// When the client last sent a session INPUT or an exec stream DATA (a
    /// control-mode client's keys).
    last_typed: Cell<Option<Instant>>,
    /// Bounds what all its channels have in flight together.
    pub flight: RefCell<Flight>,
    /// Phases the path's probing (path::probe_gain).
    born: Instant,
}

#[derive(Default)]
pub(crate) struct ClientState {
    pub streams: HashMap<u32, Rc<Stream>>,
    pub chans: HashMap<u32, Rc<Channel>>,
    /// Create, list and kill requests in flight; each runs as its own task.
    busy: usize,
}

impl Client {
    pub fn new(srv: Rc<Server>, conn: Rc<dyn Conn>) -> Rc<Client> {
        let t = &srv.cfg.tune;
        let send = SendQueue::new(t.send_soft, t.send_hard, t.chan_queue_bytes);
        Rc::new(Client {
            srv,
            conn,
            send,
            id: RefCell::default(),
            features: RefCell::default(),
            closed: Cell::new(false),
            cancel: Notify::new(),
            st: RefCell::default(),
            last_typed: Cell::new(None),
            flight: RefCell::new(Flight::default()),
            born: Instant::now(),
        })
    }

    /// Resolves once the client is closed.
    pub async fn cancelled(&self) {
        while !self.closed.get() {
            self.cancel.notified().await;
        }
    }

    /// Reports whether someone typed on the connection within TYPING_SPAN.
    pub fn typing(&self, now: Instant) -> bool {
        self.last_typed.get().is_some_and(|t| now.saturating_duration_since(t) < TYPING_SPAN)
    }

    /// While someone types on a TCP connection, how many more bulk bytes the
    /// path may take before they queue ahead of an echo by more than
    /// path::TYPING_QUEUE (see path.rs); None when nothing bounds them
    /// there.
    pub fn path_room(&self, now: Instant) -> Option<i64> {
        if !self.typing(now) {
            return None;
        }
        let s = self.conn.path(now)?;
        let gain = path::probe_gain(now.saturating_duration_since(self.born), s.min_rtt);
        let rate = self.flight.borrow().rate_max(now) * gain;
        Some(path::room(s, rate, self.send.bulk_bytes() as u64))
    }

    /// Records a keystroke; the first after a quiet spell tightens every
    /// channel's in-flight window at once.
    fn typed(&self, now: Instant) {
        let quiet = !self.typing(now);
        self.last_typed.set(Some(now));
        if !quiet {
            return;
        }
        for ch in self.st.borrow().chans.values() {
            ch.s.borrow_mut().fl.tighten();
        }
        self.flight.borrow_mut().tighten();
    }

    /// Wakes its channels waiting for room, all but one.
    pub fn wake_channels(&self, but: Option<&Rc<Channel>>) {
        let chans: Vec<Rc<Channel>> = self.st.borrow().chans.values().cloned().collect();
        for ch in chans {
            if !but.is_some_and(|b| Rc::ptr_eq(b, &ch)) {
                ch.wake();
            }
        }
    }

    pub async fn run(self: &Rc<Self>) {
        tokio::task::spawn_local(self.clone().write_loop());
        self.serve().await;
        self.close(CLOSE_NORMAL, "");
    }

    async fn serve(self: &Rc<Self>) {
        let Ok(f) = self.read().await else { return };
        let hello: wire::Hello = match (f.ty == wire::TYPE_HELLO).then(|| f.decode::<wire::Hello>()) {
            Some(Ok(h)) if !h.client_id.is_empty() => h,
            _ => return self.close(CLOSE_POLICY_VIOLATION, "expected HELLO with client_id"),
        };
        if !valid_client_id(&hello.client_id) {
            return self.close(CLOSE_POLICY_VIOLATION, "client_id must be printable and at most 128 bytes");
        }
        if hello.version != wire::VERSION {
            return self.close(
                CLOSE_POLICY_VIOLATION,
                &format!("protocol {}, server speaks {}", hello.version, wire::VERSION),
            );
        }
        *self.id.borrow_mut() = hello.client_id.clone();
        let offered = self.srv.features();
        *self.features.borrow_mut() = offered.iter().filter(|f| wire::has_feature(&hello.features, f)).cloned().collect();
        self.push(wire::json(
            wire::TYPE_WELCOME,
            0,
            &wire::Welcome {
                version: wire::VERSION,
                server: self.srv.cfg.name.clone(),
                snapshot_version: osewa_ghostty::SNAPSHOT_VERSION,
                ghostty: osewa_ghostty::COMMIT.into(),
                carriers: self.srv.carriers(),
                features: offered,
            },
        ));

        loop {
            let Ok(f) = self.read().await else { return };
            match f.ty {
                wire::TYPE_REQUEST => self.request(f),
                wire::TYPE_INPUT | wire::TYPE_ACK | wire::TYPE_CREDIT | wire::TYPE_DATA | wire::TYPE_EOF => {
                    // Frames for a stream that just ended are in flight by
                    // design, and so are dropped like frames of the wrong
                    // kind.
                    let ch = self.channel(f.stream);
                    let exec_key = f.ty == wire::TYPE_DATA
                        && ch.as_ref().is_some_and(|ch| ch.cls == Class::Exec)
                        && f.payload.len() <= TYPED_FRAME_MAX;
                    if f.ty == wire::TYPE_INPUT || exec_key {
                        self.typed(Instant::now());
                    }
                    if let Some(ch) = ch {
                        if let Err(e) = ch.frame(&f) {
                            return self.close(CLOSE_POLICY_VIOLATION, &e);
                        }
                    } else if f.ty != wire::TYPE_DATA && f.ty != wire::TYPE_EOF {
                        if let Some(st) = self.stream(f.stream) {
                            st.forward(f);
                        }
                    }
                }
                _ => return self.close(CLOSE_POLICY_VIOLATION, "unexpected frame"),
            }
        }
    }

    async fn read(&self) -> io::Result<wire::Frame> {
        let b = tokio::select! {
            r = self.conn.read() => r?,
            _ = self.cancelled() => return Err(io::ErrorKind::ConnectionAborted.into()),
        };
        wire::Frame::unmarshal(&b).map_err(|e| {
            self.close(CLOSE_POLICY_VIOLATION, &e.to_string());
            e.into()
        })
    }

    async fn write_loop(self: Rc<Self>) {
        let mut gens = HashMap::new();
        let timeout = self.srv.cfg.tune.write_timeout;
        while let Some(b) = self.send.pop().await {
            let ok = tokio::select! {
                r = tokio::time::timeout(timeout, self.conn.write(&b)) => matches!(r, Ok(Ok(()))),
                _ = self.cancelled() => return,
            };
            if !ok {
                return self.close(CLOSE_GOING_AWAY, "write failed");
            }
            if let Some(tr) = &self.srv.trace {
                tr.sent(&b, &mut gens);
            }
        }
    }

    /// Queues a connection-level frame, or a stream's INPUT_ACK or GRANT,
    /// ahead of all data without waiting. An overflow drops the client.
    pub fn push(&self, f: wire::Frame) {
        self.push_stream(0, Class::Control, f);
    }

    /// Queues f behind the frames queued for stream (see SendQueue). It
    /// returns false once the client is closed.
    pub fn push_stream(&self, stream: u32, cls: Class, f: wire::Frame) -> bool {
        if !self.send.push(stream, cls, f.marshal()) {
            self.close(CLOSE_POLICY_VIOLATION, "send queue overflow");
            return false;
        }
        true
    }

    /// push_stream, first waiting while the stream's class is over its
    /// bound.
    pub async fn push_stream_wait(&self, stream: u32, cls: Class, f: wire::Frame) -> bool {
        if !self.send.push_wait(stream, cls, f.marshal()).await {
            self.close(CLOSE_POLICY_VIOLATION, "send queue overflow");
            return false;
        }
        true
    }

    pub fn respond(&self, resp: wire::Response) {
        self.push(wire::json(wire::TYPE_RESPONSE, 0, &resp));
    }

    pub fn fail(&self, id: u64, code: &str, msg: &str) {
        self.respond(wire::Response::error(id, code, msg));
    }

    /// Answers a request whose holder couldn't be reached: a missing socket
    /// means the session is gone, anything else that it may come back.
    fn fail_dial(&self, id: u64, e: &io::Error) {
        if e.kind() == io::ErrorKind::NotFound {
            return self.fail(id, wire::ERR_NOT_FOUND, "no such session");
        }
        self.fail(id, wire::ERR_UNAVAILABLE, &e.to_string());
    }

    /// Tears down the client and every holder connection it opened. The
    /// holders keep its input seq and ownership for a resume. Channel
    /// streams end as for stream.close.
    pub fn close(&self, code: u16, reason: &str) {
        if self.closed.replace(true) {
            return;
        }
        self.send.close();
        self.cancel.notify_waiters();
        let (streams, chans) = {
            let mut st = self.st.borrow_mut();
            (std::mem::take(&mut st.streams), std::mem::take(&mut st.chans))
        };
        for st in streams.into_values() {
            st.end();
        }
        for ch in chans.into_values() {
            ch.stop();
        }
        let (conn, reason) = (self.conn.clone(), reason.to_string());
        tokio::task::spawn_local(async move { conn.close(code, reason).await });
    }

    pub fn stream(&self, id: u32) -> Option<Rc<Stream>> {
        self.st.borrow().streams.get(&id).cloned()
    }

    pub fn channel(&self, id: u32) -> Option<Rc<Channel>> {
        self.st.borrow().chans.get(&id).cloned()
    }

    /// Reports whether both ends listed feature f.
    pub fn has(&self, f: &str) -> bool {
        wire::has_feature(&self.features.borrow(), f)
    }

    fn request(self: &Rc<Self>, f: wire::Frame) {
        let Ok(mut req) = f.decode::<wire::Request>() else {
            return self.fail(0, wire::ERR_BAD_REQUEST, "malformed request");
        };
        match req.method.as_str() {
            wire::METHOD_CREATE | wire::METHOD_LIST | wire::METHOD_KILL | wire::METHOD_RECONCILE => {
                if !self.acquire() {
                    return self.fail(req.id, wire::ERR_UNAVAILABLE, "too many requests in flight");
                }
                let c = self.clone();
                tokio::task::spawn_local(async move {
                    match req.method.as_str() {
                        // Not cancelled: a holder half started
                        // is better finished than orphaned.
                        wire::METHOD_CREATE => c.create(req).await,
                        // Holder calls end with the connection.
                        wire::METHOD_LIST => {
                            tokio::select! {
                                _ = c.list(req) => {}
                                _ = c.cancelled() => {}
                            }
                        }
                        wire::METHOD_RECONCILE => {
                            tokio::select! {
                                _ = c.reconcile(req) => {}
                                _ = c.cancelled() => {}
                            }
                        }
                        _ => {
                            tokio::select! {
                                _ = c.kill(req) => {}
                                _ = c.cancelled() => {}
                            }
                        }
                    }
                    c.st.borrow_mut().busy -= 1;
                });
            }
            wire::METHOD_ATTACH => self.attach(req),
            wire::METHOD_FORWARD_OPEN | wire::METHOD_EXEC_START => self.open(req),
            wire::METHOD_STREAM_CLOSE => self.close_stream(req),
            wire::METHOD_DETACH | wire::METHOD_OWN | wire::METHOD_RESIZE | wire::METHOD_COLOR_SCHEME | wire::METHOD_COLORS | wire::METHOD_CLIPBOARD => {
                let Some(st) = self.stream(req.stream) else {
                    return self.fail(req.id, wire::ERR_NOT_FOUND, "no such stream");
                };
                if req.method == wire::METHOD_RESIZE && !valid_size(req.cols, req.rows) {
                    return self.fail(req.id, wire::ERR_BAD_REQUEST, &err_size());
                }
                req.client_id = self.id.borrow().clone();
                st.request(req);
            }
            m => self.fail(req.id, wire::ERR_BAD_REQUEST, &format!("unknown method {m}")),
        }
    }

    /// Reserves one of the client's slots for a background request.
    fn acquire(&self) -> bool {
        let mut st = self.st.borrow_mut();
        if st.busy >= self.srv.cfg.tune.max_busy {
            return false;
        }
        st.busy += 1;
        true
    }

    async fn create(&self, req: wire::Request) {
        if !valid_size(req.cols, req.rows) {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, &err_size());
        }
        let look = wire::Look::of(&req);
        if let Err(e) = look.check() {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, &e);
        }
        let argv = if req.argv.is_empty() { self.srv.cfg.shell.clone() } else { req.argv };
        // A remote login's, as sshd sets them (and mosh's sessions inherit):
        // programs take them to mean the user is on another machine, and
        // copy to the user's clipboard (OSC 52) rather than this one's.
        // The client's own entries come after, so they win.
        let mut env = login_env(self.conn.addrs());
        env.extend(req.env);
        if !req.tag.is_empty() && !valid_client_id(&req.tag) {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, "tag must be printable and at most 128 bytes");
        }
        let creator = self.id.borrow().clone();
        let spec = osewa_holder::spec(&argv, &req.cwd);
        let cfg = osewa_holder::Config {
            run_dir: self.srv.cfg.run_dir.clone(),
            cols: req.cols,
            rows: req.rows,
            width: req.width,
            height: req.height,
            argv,
            cwd: req.cwd,
            env,
            look,
            spec: spec.clone(),
            home: self.srv.cfg.home.clone(),
            ..Default::default()
        };
        if req.tag.is_empty() {
            return self.spawn(req.id, cfg).await;
        }
        let tag = req.tag;
        self.srv
            .with_tag(&creator, &tag, async {
                match self.tagged(&creator, &tag, &spec).await {
                    Ok(Some(id)) => self.respond(wire::Response { id: req.id, session_id: id, reused: true, ..Default::default() }),
                    Ok(None) => self.spawn(req.id, osewa_holder::Config { tag: tag.clone(), creator: creator.clone(), ..cfg }).await,
                    Err(e) => self.fail(req.id, wire::ERR_INTERNAL, &e.to_string()),
                }
            })
            .await
    }

    /// The live session creator made with tag that runs spec, if any. The
    /// rest of creator's sessions with tag are killed: a create of a tag
    /// means the client no longer has them, so they would only pile up.
    async fn tagged(&self, creator: &str, tag: &str, spec: &str) -> io::Result<Option<String>> {
        let h = self.srv.holders();
        let mine: Vec<wire::SessionInfo> = h.list(creator).await?.into_iter().filter(|s| s.creator == creator && s.tag == tag).collect();
        // The newest that can be attached as it is.
        let reuse = mine.iter().rev().find(|s| !s.exited && s.incompatible.is_empty() && s.spec == spec).map(|s| s.id.clone());
        for s in &mine {
            if Some(&s.id) != reuse.as_ref() {
                let _ = h.kill(&s.id, creator).await;
            }
        }
        Ok(reuse)
    }

    /// Starts a holder for cfg under a new id.
    async fn spawn(&self, req_id: u64, cfg: osewa_holder::Config) {
        let id = new_session_id();
        match (self.srv.spawn)(osewa_holder::Config { id: id.clone(), ..cfg }).await {
            Err(e) => self.fail(req_id, wire::ERR_INTERNAL, &e),
            Ok(()) => self.respond(wire::Response { id: req_id, session_id: id, ..Default::default() }),
        }
    }

    async fn list(&self, req: wire::Request) {
        let client_id = self.id.borrow().clone();
        match self.srv.holders().list(&client_id).await {
            Err(e) => self.fail(req.id, wire::ERR_INTERNAL, &e.to_string()),
            Ok(sessions) => self.respond(wire::Response { id: req.id, sessions, ..Default::default() }),
        }
    }

    /// Kills this client's tagged sessions whose tag isn't in req.tags,
    /// and answers with them.
    async fn reconcile(&self, req: wire::Request) {
        if req.tags.len() > crate::local::MAX_RECONCILE_TAGS {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, &format!("more than {} tags", crate::local::MAX_RECONCILE_TAGS));
        }
        let client_id = self.id.borrow().clone();
        let grace = self.srv.cfg.tune.reconcile_grace;
        match self.srv.holders().reconcile(&client_id, &client_id, &req.tags, grace).await {
            Err(e) => self.fail(req.id, wire::ERR_INTERNAL, &e.to_string()),
            Ok(sessions) => self.respond(wire::Response { id: req.id, sessions, ..Default::default() }),
        }
    }

    async fn kill(&self, req: wire::Request) {
        if !valid_id(&req.session) {
            return self.fail(req.id, wire::ERR_NOT_FOUND, "no such session");
        }
        let client_id = self.id.borrow().clone();
        match self.srv.holders().call(&req.session.clone(), &client_id, req.clone()).await {
            Err(e) => self.fail_dial(req.id, &e),
            Ok(resp) => self.respond(resp),
        }
    }

    fn attach(self: &Rc<Self>, mut req: wire::Request) {
        if req.stream == 0 {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, "stream must be nonzero");
        }
        if !valid_id(&req.session) {
            return self.fail(req.id, wire::ERR_NOT_FOUND, "no such session");
        }
        // An attach may leave the size alone (0x0).
        if (req.cols != 0 || req.rows != 0) && !valid_size(req.cols, req.rows) {
            return self.fail(req.id, wire::ERR_BAD_REQUEST, &err_size());
        }
        let st = Stream::new(self.clone(), req.stream, req.id);
        req.client_id = self.id.borrow().clone();
        let session = req.session.clone();
        // Queued before the holder is dialed, so INPUT that follows ATTACH
        // immediately waits behind it instead of being dropped.
        st.forward(wire::json(wire::TYPE_REQUEST, 0, &req));
        let add = st.clone();
        if !self.reserve(&req, |s| {
            s.streams.insert(req.stream, add);
        }) {
            return;
        }
        tokio::task::spawn_local(async move { st.run(session).await });
    }

    /// Claims req.stream, which session and channel streams share, by
    /// calling add, or answers req with why it can't.
    pub fn reserve(&self, req: &wire::Request, add: impl FnOnce(&mut ClientState)) -> bool {
        let err = {
            let mut s = self.st.borrow_mut();
            let busy = s.streams.contains_key(&req.stream) || s.chans.get(&req.stream).is_some_and(|ch| !ch.ended());
            if self.closed.get() {
                Some((wire::ERR_UNAVAILABLE, "connection closing"))
            } else if busy {
                Some((wire::ERR_BAD_REQUEST, "stream already in use"))
            } else if s.streams.len() + s.chans.len() >= self.srv.cfg.tune.max_streams {
                Some((wire::ERR_UNAVAILABLE, "too many streams"))
            } else {
                add(&mut s);
                None
            }
        };
        match err {
            Some((code, msg)) => {
                self.fail(req.id, code, msg);
                false
            }
            None => true,
        }
    }
}

pub(crate) fn new_session_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Keeps session ids from naming paths outside the run directory.
pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One attached stream and its holder connection.
pub(crate) struct Stream {
    c: Rc<Client>,
    id: u32,
    /// The attach request, whose response decides the stream's fate.
    req_id: u64,
    /// Wakes the writer to the holder.
    wake: Notify,
    /// Fires when done or dead becomes true.
    changed: Notify,
    s: RefCell<StreamState>,
}

#[derive(Default)]
struct StreamState {
    /// Waiting to be written to the holder.
    out: Vec<wire::Frame>,
    out_bytes: usize,
    /// Forwarded requests still owed a response.
    pending: HashSet<u64>,
    /// The stream id is free again.
    done: bool,
    /// The holder connection is closed, or must be.
    dead: bool,
    /// A write to the holder failed: the relay's next read fails, as it
    /// would on the closed connection.
    broken: bool,
}

impl Stream {
    fn new(c: Rc<Client>, id: u32, req_id: u64) -> Rc<Stream> {
        Rc::new(Stream { c, id, req_id, wake: Notify::new(), changed: Notify::new(), s: RefCell::default() })
    }

    fn is_done(&self) -> bool {
        self.s.borrow().done
    }

    fn is_dead(&self) -> bool {
        self.s.borrow().dead
    }

    async fn until(&self, cond: impl Fn(&StreamState) -> bool) {
        while !cond(&self.s.borrow()) {
            self.changed.notified().await;
        }
    }

    /// Queues f for the holder without waiting. The holder stops reading
    /// while its child is behind on input, and that must stall only this
    /// stream: the client's read loop serves every stream. A queue past
    /// stream_queue_bytes ends just this stream.
    pub fn forward(self: &Rc<Self>, f: wire::Frame) {
        {
            let mut s = self.s.borrow_mut();
            if s.done {
                return;
            }
            let n = 4 + wire::HEADER_SIZE + f.payload.len();
            if s.out_bytes + n <= self.c.srv.cfg.tune.stream_queue_bytes {
                s.out.push(f);
                s.out_bytes += n;
                drop(s);
                self.wake.notify_one();
                return;
            }
            s.out.clear();
            s.out_bytes = 0;
        }
        self.end();
        self.c.push_stream(self.id, Class::Session, end_frame(self.id, wire::END_ERROR));
    }

    /// Forwards a stream request and remembers that a response is owed.
    fn request(self: &Rc<Self>, req: wire::Request) {
        self.s.borrow_mut().pending.insert(req.id);
        self.forward(wire::json(wire::TYPE_REQUEST, 0, &req));
    }

    /// Records a relayed response and reports whether any are owed.
    fn answered(&self, id: u64) -> bool {
        let mut s = self.s.borrow_mut();
        s.pending.remove(&id);
        !s.pending.is_empty()
    }

    fn owed(&self) -> bool {
        !self.s.borrow().pending.is_empty()
    }

    /// Frees the stream id and stops forwarding to the holder; the
    /// connection may still be draining responses.
    fn finish(self: &Rc<Self>) {
        if std::mem::replace(&mut self.s.borrow_mut().done, true) {
            return;
        }
        {
            let mut cs = self.c.st.borrow_mut();
            if cs.streams.get(&self.id).is_some_and(|s| Rc::ptr_eq(s, self)) {
                cs.streams.remove(&self.id);
            }
        }
        self.changed.notify_waiters();
    }

    /// Fails the holder connection after a write to it failed, as closing it
    /// would: the relay then ends the stream with STREAM_END error, or the
    /// attach with unavailable.
    pub(crate) fn conn_broken(&self) {
        self.s.borrow_mut().broken = true;
        self.changed.notify_waiters();
    }

    /// Finishes the stream and closes the holder connection.
    pub fn end(self: &Rc<Self>) {
        self.finish();
        if !std::mem::replace(&mut self.s.borrow_mut().dead, true) {
            self.changed.notify_waiters();
        }
    }

    async fn run(self: Rc<Self>, session: String) {
        let client_id = self.c.id.borrow().clone();
        let conn = tokio::select! {
            r = self.c.srv.dial_holder(&session, &client_id) => r,
            _ = self.until(|s| s.dead) => return,
        };
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                self.end();
                return self.c.fail_dial(self.req_id, &e);
            }
        };
        let (rd, mut wr) = conn.into_split();
        let mut rd = BufReader::with_capacity(64 << 10, rd);
        // On this connection, before any of the holder's frames can reach
        // the client: a holder of another build would send snapshots it
        // can't decode.
        let checked = tokio::select! {
            r = self.c.srv.check_build(&mut rd, &mut wr, &client_id) => r,
            _ = self.until(|s| s.dead) => return,
        };
        match checked {
            Ok(why) if why.is_empty() => {}
            Ok(why) => {
                self.end();
                return self.c.fail(self.req_id, wire::ERR_INCOMPATIBLE, &format!("session {session} can't be attached: {why}; kill it and start a new one"));
            }
            Err(e) => {
                self.end();
                return self.c.fail(self.req_id, wire::ERR_UNAVAILABLE, &e.to_string());
            }
        }
        tokio::task::spawn_local(self.clone().relay(rd));
        self.write_holder(wr).await;
    }

    /// Writes what is queued for the holder. No write deadline: a holder
    /// that stops reading is applying input backpressure, not dead. A dead
    /// holder shows up as EOF in relay, which ends the stream and so this.
    async fn write_holder(self: &Rc<Self>, mut wr: OwnedWriteHalf) {
        let mut buf = Vec::new();
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = self.until(|s| s.done || s.dead) => {}
            }
            if self.is_dead() {
                return;
            }
            if self.is_done() {
                // The holder still answers what it owes over this
                // connection: keep it open until the relay is done.
                wr.forget();
                return;
            }
            let fs = {
                let mut s = self.s.borrow_mut();
                s.out_bytes = 0;
                std::mem::take(&mut s.out)
            };
            buf.clear();
            for f in &fs {
                let _ = f.encode_to(&mut buf);
            }
            let ok = tokio::select! {
                r = wr.write_all(&buf) => r.is_ok(),
                _ = self.until(|s| s.dead) => return,
            };
            if !ok {
                return self.conn_broken();
            }
            if let Some(tr) = &self.c.srv.trace {
                tr.forwarded(&fs);
            }
        }
    }

    /// Copies holder frames to the client unchanged. It looks only at frame
    /// types and response ids, never at data payloads.
    async fn relay(self: Rc<Self>, mut rd: BufReader<OwnedReadHalf>) {
        let mut attached = false;
        let mut deadline: Option<tokio::time::Instant> = None;
        let c = &self.c;
        loop {
            let read = async {
                match deadline {
                    Some(d) => tokio::time::timeout_at(d, read_frame(&mut rd))
                        .await
                        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into())),
                    None => read_frame(&mut rd).await,
                }
            };
            let f = tokio::select! {
                r = read => r,
                _ = self.until(|s| s.dead || s.broken) => Err(io::ErrorKind::ConnectionAborted.into()),
            };
            let f = match f {
                Ok(f) => f,
                Err(_) => {
                    if !self.is_done() {
                        if attached {
                            c.push_stream(self.id, Class::Session, end_frame(self.id, wire::END_ERROR));
                        } else {
                            c.fail(self.req_id, wire::ERR_UNAVAILABLE, "holder closed the connection");
                        }
                    }
                    return self.end();
                }
            };
            let draining = self.is_done();
            let mut owed = true;
            match f.ty {
                wire::TYPE_WELCOME => continue,
                wire::TYPE_RESPONSE => {
                    if let Ok(resp) = f.decode::<wire::Response>() {
                        if !attached && resp.id == self.req_id {
                            if !resp.error.is_empty() {
                                self.end();
                                c.push(f);
                                return;
                            }
                            attached = true;
                        } else {
                            owed = self.answered(resp.id);
                        }
                    }
                }
                // After STREAM_END only responses still owed get through.
                _ if draining => continue,
                _ => {}
            }
            if self.is_dead() {
                return;
            }
            // INPUT_ACK goes ahead of queued output, as in the holder; a
            // response stays behind a STREAM_END already queued, which is
            // the order the holder sent them in.
            let to_front = f.ty == wire::TYPE_INPUT_ACK || (f.ty == wire::TYPE_RESPONSE && !draining);
            let key = if to_front { 0 } else { self.id };
            let ty = f.ty;
            // Wait while the client is behind: the holder's own queue then
            // lags and it sends a fresh snapshot, rather than us buffering.
            let pushed = tokio::select! {
                ok = c.send.push_wait(key, Class::Session, f.marshal()) => ok,
                _ = self.until(|s| s.dead) => return,
            };
            if !pushed {
                c.close(CLOSE_GOING_AWAY, "client closed");
                return;
            }
            if ty == wire::TYPE_STREAM_END {
                self.finish();
                if !self.owed() {
                    return self.end();
                }
                // The holder answers a detach after its STREAM_END.
                deadline = Some(tokio::time::Instant::now() + c.srv.cfg.tune.holder_timeout);
            } else if draining && !owed {
                return self.end();
            }
        }
    }
}

pub(crate) fn end_frame(stream: u32, reason: &str) -> wire::Frame {
    wire::json(wire::TYPE_STREAM_END, stream, &wire::StreamEnd { reason: reason.into(), exit_code: None })
}

/// SSH_CONNECTION and SSH_CLIENT for a session created over a connection
/// from client to ours, in sshd's format: "client_ip client_port
/// server_ip server_port" and "client_ip client_port server_port". None
/// without the addresses.
pub(crate) fn login_env(addrs: Option<(std::net::SocketAddr, std::net::SocketAddr)>) -> Vec<String> {
    let Some((client, ours)) = addrs else { return Vec::new() };
    // A v4 client on a v6 socket reads as v4, as sshd prints it.
    let ip = |a: std::net::SocketAddr| a.ip().to_canonical();
    vec![
        format!("SSH_CONNECTION={} {} {} {}", ip(client), client.port(), ip(ours), ours.port()),
        format!("SSH_CLIENT={} {} {}", ip(client), client.port(), ours.port()),
    ]
}
