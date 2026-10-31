//! The fake server: the
//! server side of PROTOCOL.md with in-memory sessions, each backed by a
//! real libghostty terminal so snapshots are the real format, and channel
//! streams with real processes and TCP connections, flow-controlled as
//! PROTOCOL.md says so tests see what a real server would allow.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::process::ExitStatusExt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use osewa_carrier::ws;
use osewa_ghostty as ghostty;
use osewa_wire as wire;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

use super::*;
use crate::hosts::testutil::TempDir;

/// A libghostty terminal the fake shares between its thread and the
/// test's. It has no callbacks, libghostty keeps no thread affinity, and
/// every use is under the fake's lock.
pub(crate) struct Term(pub ghostty::Terminal);
unsafe impl Send for Term {}

pub(crate) struct FakeSession {
    pub id: String,
    pub term: Term,
    pub epoch: String,
    pub log: Vec<Record>,
    /// Log end.
    pub next: u64,
    /// Positions before this are gone.
    pub evicted: u64,
    pub owner: String,
    pub last_seq: HashMap<String, u64>,
    pub applied: Vec<wire::Input>,
    pub dupes: usize,
    pub acked: u64,
    /// The generation of every ACK, in order.
    pub ack_gens: Vec<u32>,
    /// conn -> stream -> next generation.
    pub streams: HashMap<u64, HashMap<u32, u32>>,
}

#[derive(Clone)]
pub(crate) struct Record {
    pub end: u64,
    pub ty: u8,
    pub data: Vec<u8>,
}

pub(crate) enum Out {
    Frame(Vec<u8>),
    CloseNow,
}

pub(crate) struct FakeConn {
    out: mpsc::UnboundedSender<Out>,
    pub client_id: String,
    /// stream -> session id
    pub streams: HashMap<u32, String>,
    /// stream -> channel key
    pub chans: HashMap<u32, u64>,
    /// Flow control bookkeeping per stream: counted bytes sent, credited.
    pub sent: HashMap<u32, usize>,
    pub credited: HashMap<u32, usize>,
}

pub(crate) enum Sink {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    Stdin(tokio::process::ChildStdin),
}

pub(crate) struct FakeChan {
    pub conn: u64,
    pub stream: u32,
    /// CHANNEL_DATA bytes the client still accepts.
    pub credit: i64,
    /// DATA bytes taken, before granting them back.
    pub received: i64,
    /// The client sent DATA beyond its grant.
    pub over: bool,
    pub ended: bool,
    /// Most DATA bytes ever outstanding (sent, not granted).
    pub max_fly: i64,
    /// CHANNEL_EOFs sent.
    pub out_eof: usize,
    pub in_eof: bool,
    pub forward: bool,
    pub pid: Option<i32>,
    sink: Option<Sink>,
    tasks: Vec<AbortHandle>,
    /// Withholds GRANT, so the client's DATA stops at its window.
    pub no_grant: bool,
    /// Requests answered once the STREAM_END is out.
    on_end: Vec<u64>,
    /// Bytes sent as CHANNEL_DATA.
    pub pumped: usize,
    /// Counted bytes sent and not yet credited, all credit received.
    pub uncredited: i64,
    pub credited: i64,
    grant_timer: bool,
    pumps_done: usize,
}

#[derive(Default)]
pub(crate) struct FState {
    pub sessions: HashMap<String, FakeSession>,
    pub conns: HashMap<u64, FakeConn>,
    next_conn: u64,
    /// WELCOME's ghostty commit.
    pub commit: String,
    /// Withhold INPUT_ACK.
    pub no_ack: bool,
    /// Queue post-attach INPUT_ACKs until release_base.
    pub hold_base: bool,
    /// Cut the connection after READY and one history chunk.
    pub drop_after_ready: bool,
    /// Before handling each attach request.
    pub attach_delay: Duration,
    /// While set, connections stop reading.
    stalled: Option<watch::Receiver<bool>>,
    /// Send WELCOME only after the first REQUEST.
    pub late_welcome: bool,
    /// Connections whose first REQUEST beat WELCOME.
    pub pipelined: usize,
    held: Vec<(u64, wire::Frame)>,
    pub attaches: Vec<wire::Response>,
    /// WELCOME's.
    pub features: Vec<String>,
    /// When set, keeps each channel's uncredited CHANNEL_DATA within it, as
    /// the server's in-flight window does at its smallest.
    pub chan_fly: i64,
    /// Grants DATA only as PROTOCOL.md requires at the least: every 8 KiB
    /// written, or 50 ms after.
    pub lazy_grant: bool,
    /// Answer stream.close before the channel's last data and STREAM_END,
    /// as a QUIC server may: its streams don't keep order between them.
    pub close_response_first: bool,
    /// What stream.close sends an exec's process group (default SIGKILL).
    pub close_signal: Option<i32>,
    pub chans: HashMap<u64, FakeChan>,
    /// Every channel stream opened, in order.
    pub chan_order: Vec<u64>,
    next_chan: u64,
    pub hellos: Vec<wire::Hello>,
    pub creates: Vec<wire::Request>,
}

pub(crate) struct Fake {
    pub inner: Arc<FakeInner>,
    pub addr: SocketAddr,
    tls: Arc<rustls::ClientConfig>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _dir: TempDir,
}

impl FakeInner {
    /// A failed test may have poisoned the lock; the fake carries on.
    fn lock(&self) -> MutexGuard<'_, FState> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(crate) struct FakeInner {
    mu: Mutex<FState>,
    /// Bumped whenever a channel gets credit or ends.
    chan_wake: watch::Sender<u64>,
}

fn rand_hex(n: usize) -> String {
    hex::encode((0..n).map(|_| rand::random::<u8>()).collect::<Vec<_>>())
}

fn send(conns: &mut HashMap<u64, FakeConn>, cid: u64, f: wire::Frame) {
    let Some(c) = conns.get_mut(&cid) else { return };
    if matches!(f.ty, wire::TYPE_SNAPSHOT_HISTORY | wire::TYPE_OUTPUT | wire::TYPE_EVENT) {
        *c.sent.entry(f.stream).or_default() += f.payload.len();
    }
    let _ = c.out.send(Out::Frame(f.marshal()));
}

fn respond(conns: &mut HashMap<u64, FakeConn>, cid: u64, r: wire::Response) {
    send(conns, cid, wire::json(wire::TYPE_RESPONSE, 0, &r));
}

fn record_frame(stream: u32, rec: &Record) -> wire::Frame {
    if rec.ty == wire::TYPE_OUTPUT {
        wire::output(stream, rec.end, &rec.data)
    } else {
        wire::event(stream, rec.end, &rec.data)
    }
}

impl Fake {
    pub fn new() -> Fake {
        let dir = TempDir::new();
        let server = osewa_auth::Identity::generate();
        let client = osewa_auth::Identity::generate();
        let devices = osewa_auth::Devices::new(dir.path().join("devices"));
        devices.add(osewa_auth::Device { name: "test".into(), key: osewa_auth::encode_key(&client.public()) }).unwrap();
        let server_tls = Arc::new(osewa_auth::server_config(&server, &devices));
        let tls = Arc::new(osewa_auth::client_config(&client, server.public()));
        let inner = Arc::new(FakeInner {
            mu: Mutex::new(FState {
                commit: crate::GHOSTTY_COMMIT.into(),
                features: vec![wire::FEATURE_FORWARD.into(), wire::FEATURE_EXEC.into()],
                ..Default::default()
            }),
            chan_wake: watch::Sender::new(0),
        });
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ln.set_nonblocking(true).unwrap();
        let addr = ln.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let fi = inner.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let ln = tokio::net::TcpListener::from_std(ln).unwrap();
                let acceptor = tokio_rustls::TlsAcceptor::from(server_tls);
                let serve = async {
                    loop {
                        let Ok((tcp, _)) = ln.accept().await else { return };
                        let _ = tcp.set_nodelay(true);
                        let (fi, acceptor) = (fi.clone(), acceptor.clone());
                        tokio::task::spawn_local(async move {
                            let Ok(tls) = acceptor.accept(tcp).await else { return };
                            let Ok(ws) = ws::accept(tls, false).await else { return };
                            serve_conn(fi, ws).await;
                        });
                    }
                };
                tokio::select! {
                    _ = serve => {}
                    _ = stop_rx => {}
                }
            });
        });
        Fake { inner, addr, tls, stop: Some(stop_tx), _dir: dir }
    }

    pub fn lock(&self) -> MutexGuard<'_, FState> {
        self.inner.lock()
    }

    pub fn config(&self, client_id: &str) -> Config {
        Config {
            url: format!("wss://{}{}", self.addr, wire::PATH),
            tls: Some(self.tls.clone()),
            client_id: client_id.into(),
            name: "test".into(),
            ..Default::default()
        }
    }

    /// Dials as client_id and, unless WELCOME comes late, waits for it:
    /// tests that act on the fake right away need it to know the
    /// connection.
    pub fn dial(&self, client_id: &str) -> Conn {
        let c = run(Conn::dial(self.config(client_id))).unwrap();
        if !self.lock().late_welcome {
            run(c.welcome()).unwrap();
        }
        c
    }

    pub fn set(&self, f: impl FnOnce(&mut FState)) {
        f(&mut self.lock())
    }

    pub fn with<R>(&self, id: &str, f: impl FnOnce(&mut FakeSession) -> R) -> R {
        f(self.lock().sessions.get_mut(id).unwrap())
    }

    pub fn create(&self, cols: usize, rows: usize) -> String {
        let s = new_session(cols, rows);
        let id = s.id.clone();
        self.lock().sessions.insert(id.clone(), s);
        id
    }

    pub fn output(&self, id: &str, data: &str) {
        let mut st = self.lock();
        let st = &mut *st;
        let s = st.sessions.get_mut(id).unwrap();
        s.term.0.write(data.as_bytes());
        append_record(&mut st.conns, s, wire::TYPE_OUTPUT, data.as_bytes().to_vec());
    }

    pub fn emit(&self, id: &str, e: wire::EventMsg) {
        let mut st = self.lock();
        let st = &mut *st;
        append_record(&mut st.conns, st.sessions.get_mut(id).unwrap(), wire::TYPE_EVENT, wire::marshal_event(&e));
    }

    pub fn exit(&self, id: &str, code: i64) {
        let mut st = self.lock();
        let st = &mut *st;
        let s = st.sessions.get_mut(id).unwrap();
        let e = wire::EventMsg { code: Some(code), ..wire::EventMsg::kind(wire::EVENT_EXIT) };
        append_record(&mut st.conns, s, wire::TYPE_EVENT, wire::marshal_event(&e));
        end_session(&mut st.conns, s, &wire::StreamEnd { reason: wire::END_EXITED.into(), exit_code: Some(code) });
    }

    /// Forgets the whole log, so any resume needs a snapshot.
    pub fn evict(&self, id: &str) {
        self.with(id, |s| {
            s.evicted = s.next + 1;
            s.log.clear();
        })
    }

    /// Sends STREAM_END with reason to every stream of session id.
    pub fn end_streams(&self, id: &str, reason: &str) {
        let mut st = self.lock();
        let st = &mut *st;
        end_session(&mut st.conns, st.sessions.get_mut(id).unwrap(), &wire::StreamEnd { reason: reason.into(), exit_code: None });
    }

    /// Counts the streams attached to session id.
    pub fn stream_count(&self, id: &str) -> usize {
        self.with(id, |s| s.streams.values().map(HashMap::len).sum())
    }

    /// Counted bytes sent and credited over every connection.
    pub fn flow(&self) -> (usize, usize) {
        let st = self.lock();
        let sent = st.conns.values().flat_map(|c| c.sent.values()).sum();
        let credited = st.conns.values().flat_map(|c| c.credited.values()).sum();
        (sent, credited)
    }

    /// What session id's streams were sent and credited.
    pub fn credited(&self, id: &str) -> (usize, usize) {
        let st = self.lock();
        let (mut sent, mut credited) = (0, 0);
        for (cid, streams) in &st.sessions[id].streams {
            let c = &st.conns[cid];
            for stream in streams.keys() {
                sent += c.sent.get(stream).copied().unwrap_or(0);
                credited += c.credited.get(stream).copied().unwrap_or(0);
            }
        }
        (sent, credited)
    }

    /// Sends every stream of session id an INPUT_ACK for what it applied,
    /// as a holder does once the child reads.
    pub fn ack_inputs(&self, id: &str) {
        let mut st = self.lock();
        let st = &mut *st;
        let s = &st.sessions[id];
        for (cid, streams) in &s.streams {
            let seq = s.last_seq.get(&st.conns[cid].client_id).copied().unwrap_or(0);
            for &stream in streams.keys() {
                send(&mut st.conns, *cid, wire::input_ack(stream, seq));
            }
        }
    }

    /// Sums the text of applied inputs.
    pub fn input_bytes(&self, id: &str) -> usize {
        self.with(id, |s| s.applied.iter().map(|i| i.text.len()).sum())
    }

    pub fn release_base(&self) {
        let mut st = self.lock();
        let st = &mut *st;
        for (cid, f) in std::mem::take(&mut st.held) {
            send(&mut st.conns, cid, f);
        }
        st.hold_base = false;
    }

    /// Sends a fresh snapshot to every stream, like a lagging client.
    pub fn resnapshot(&self, id: &str) {
        self.resnapshot_with(id, true)
    }

    /// Sends a catch-up screen snapshot (no history) to every stream.
    pub fn screen_snapshot(&self, id: &str) {
        self.resnapshot_with(id, false)
    }

    fn resnapshot_with(&self, id: &str, history: bool) {
        let mut st = self.lock();
        let st = &mut *st;
        let s = st.sessions.get_mut(id).unwrap();
        let targets: Vec<(u64, u32)> = s.streams.iter().flat_map(|(c, m)| m.keys().map(|k| (*c, *k))).collect();
        for (cid, stream) in targets {
            snapshot(&mut st.conns, cid, stream, s, history, &mut st.drop_after_ready);
        }
    }

    /// Starts a catch-up screen snapshot of session id on every stream and
    /// stops part-way through its SNAPSHOT_DATA, as a holder does when a
    /// newer screen preempts it. The bytes sent are counted, like any later
    /// snapshot's.
    pub fn abandon(&self, id: &str) {
        let mut st = self.lock();
        let st = &mut *st;
        let s = st.sessions.get_mut(id).unwrap();
        let snap = s.term.0.snapshot().unwrap();
        let part = snap[..ghostty::ready_end(&snap) / 2].to_vec();
        let (cols, rows) = s.term.0.size();
        for (cid, streams) in s.streams.iter_mut() {
            for (&stream, gen) in streams.iter_mut() {
                let g = *gen;
                *gen += 1;
                let begin = wire::SnapshotBegin {
                    generation: g,
                    epoch: s.epoch.clone(),
                    position: s.next,
                    cols: cols as i64,
                    rows: rows as i64,
                    history: Some(false),
                };
                send(&mut st.conns, *cid, wire::json(wire::TYPE_SNAPSHOT_BEGIN, stream, &begin));
                *st.conns.get_mut(cid).unwrap().sent.entry(stream).or_default() += part.len();
                send(&mut st.conns, *cid, wire::Frame::new(wire::TYPE_SNAPSHOT_DATA, stream, part.clone()));
            }
        }
    }

    /// Stops every connection's reader until the returned guard is released
    /// or dropped. Not reading also means not answering pings.
    pub fn stall(&self) -> Stall {
        let (tx, rx) = watch::channel(false);
        self.lock().stalled = Some(rx);
        Stall { inner: self.inner.clone(), tx: Some(tx) }
    }

    /// Cuts every connection, like a network loss.
    pub fn drop_all(&self) {
        let ids: Vec<u64> = self.lock().conns.keys().copied().collect();
        for id in ids {
            drop_conn(&self.inner, id);
        }
    }

    /// The last channel stream opened.
    pub fn last_chan(&self) -> u64 {
        *self.lock().chan_order.last().unwrap()
    }

    pub fn chan<R>(&self, key: u64, f: impl FnOnce(&mut FakeChan) -> R) -> R {
        f(self.lock().chans.get_mut(&key).unwrap())
    }

    /// Sends what no_grant withheld.
    pub fn grant_all(&self, key: u64) {
        let mut st = self.lock();
        let st = &mut *st;
        let ch = st.chans.get_mut(&key).unwrap();
        ch.no_grant = false;
        if ch.received > 0 {
            send(&mut st.conns, ch.conn, wire::grant(ch.stream, ch.received as u32));
            ch.received = 0;
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.drop_all();
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

pub(crate) struct Stall {
    inner: Arc<FakeInner>,
    tx: Option<watch::Sender<bool>>,
}

impl Stall {
    pub fn release(&mut self) {
        if let Some(tx) = self.tx.take() {
            self.inner.lock().stalled = None;
            let _ = tx.send(true);
        }
    }
}

impl Drop for Stall {
    fn drop(&mut self) {
        self.release();
    }
}

fn new_session(cols: usize, rows: usize) -> FakeSession {
    let term = ghostty::Terminal::new(cols, rows, ghostty::Options { continuation: true, ..Default::default() }).unwrap();
    FakeSession {
        id: rand_hex(16),
        term: Term(term),
        epoch: rand_hex(8),
        log: Vec::new(),
        next: 0,
        evicted: 0,
        owner: String::new(),
        last_seq: HashMap::new(),
        applied: Vec::new(),
        dupes: 0,
        acked: 0,
        ack_gens: Vec::new(),
        streams: HashMap::new(),
    }
}

fn append_record(conns: &mut HashMap<u64, FakeConn>, s: &mut FakeSession, ty: u8, data: Vec<u8>) {
    s.next += wire::record_size(data.len());
    let rec = Record { end: s.next, ty, data };
    for (cid, streams) in &s.streams {
        for &stream in streams.keys() {
            send(conns, *cid, record_frame(stream, &rec));
        }
    }
    s.log.push(rec);
}

fn event(conns: &mut HashMap<u64, FakeConn>, s: &mut FakeSession, e: &wire::EventMsg) {
    append_record(conns, s, wire::TYPE_EVENT, wire::marshal_event(e));
}

fn end_session(conns: &mut HashMap<u64, FakeConn>, s: &mut FakeSession, e: &wire::StreamEnd) {
    for (cid, streams) in std::mem::take(&mut s.streams) {
        for stream in streams.keys() {
            send(conns, cid, wire::json(wire::TYPE_STREAM_END, *stream, e));
            if let Some(c) = conns.get_mut(&cid) {
                c.streams.remove(stream);
            }
        }
    }
}

/// Sends a snapshot of s at the log end, split at READY; without history it
/// stops at READY, as a catch-up screen snapshot does. Chunks are small so
/// the client sees several DATA and HISTORY frames.
fn snapshot(conns: &mut HashMap<u64, FakeConn>, cid: u64, stream: u32, s: &mut FakeSession, history: bool, drop_after_ready: &mut bool) {
    let snap = s.term.0.snapshot().unwrap();
    let ready = ghostty::ready_end(&snap);
    let gens = s.streams.get_mut(&cid).unwrap();
    let gen = gens[&stream];
    gens.insert(stream, gen + 1);
    let (cols, rows) = s.term.0.size();
    let begin = wire::SnapshotBegin {
        generation: gen,
        epoch: s.epoch.clone(),
        position: s.next,
        cols: cols as i64,
        rows: rows as i64,
        history: (!history).then_some(false),
    };
    send(conns, cid, wire::json(wire::TYPE_SNAPSHOT_BEGIN, stream, &begin));
    let chunks = |conns: &mut HashMap<u64, FakeConn>, ty: u8, b: &[u8]| {
        for c in b.chunks(4096) {
            send(conns, cid, wire::Frame::new(ty, stream, c.to_vec()));
        }
    };
    if gen != 0 {
        // Only a stream's first snapshot sends its screen uncounted.
        *conns.get_mut(&cid).unwrap().sent.entry(stream).or_default() += ready;
    }
    chunks(conns, wire::TYPE_SNAPSHOT_DATA, &snap[..ready]);
    send(conns, cid, wire::Frame::new(wire::TYPE_SNAPSHOT_READY, stream, Vec::new()));
    if *drop_after_ready {
        *drop_after_ready = false;
        let end = snap.len().min(ready + 4096);
        send(conns, cid, wire::Frame::new(wire::TYPE_SNAPSHOT_HISTORY, stream, snap[ready..end].to_vec()));
        if let Some(c) = conns.get(&cid) {
            let _ = c.out.send(Out::CloseNow);
        }
        return;
    }
    if history {
        chunks(conns, wire::TYPE_SNAPSHOT_HISTORY, &snap[ready..]);
    }
    send(conns, cid, wire::Frame::new(wire::TYPE_SNAPSHOT_END, stream, Vec::new()));
}

type WsConn = ws::Conn<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

/// What handling a frame left for the reader to do outside the lock.
enum Action {
    /// Write DATA to a channel's sink, then grant it.
    Write(u64, Vec<u8>),
    /// Half-close a channel's sink.
    Eof(u64),
}

async fn serve_conn(fi: Arc<FakeInner>, ws: WsConn) {
    let Ok(b) = ws.read().await else { return };
    let hello = wire::Frame::unmarshal(&b).ok().filter(|f| f.ty == wire::TYPE_HELLO).and_then(|f| f.decode::<wire::Hello>().ok());
    let Some(hello) = hello else {
        let _ = ws.close(ws::STATUS_PROTOCOL_ERROR, "hello").await;
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let wc = ws.clone();
    tokio::task::spawn_local(async move {
        while let Some(o) = rx.recv().await {
            match o {
                Out::Frame(b) => {
                    if wc.write(&b).await.is_err() {
                        return;
                    }
                }
                Out::CloseNow => {
                    let _ = wc.close_now();
                    return;
                }
            }
        }
    });
    let (cid, late) = {
        let mut st = fi.lock();
        st.next_conn += 1;
        let cid = st.next_conn;
        st.conns.insert(
            cid,
            FakeConn {
                out: tx,
                client_id: hello.client_id.clone(),
                streams: HashMap::new(),
                chans: HashMap::new(),
                sent: HashMap::new(),
                credited: HashMap::new(),
            },
        );
        st.hellos.push(hello);
        (cid, st.late_welcome)
    };
    struct DropConn(Arc<FakeInner>, u64);
    impl Drop for DropConn {
        fn drop(&mut self) {
            drop_conn(&self.0, self.1);
        }
    }
    let _drop = DropConn(fi.clone(), cid);
    let mut first = None;
    if late {
        // Hold WELCOME until the first request is in: a client that waits
        // for WELCOME before sending never gets one.
        let Ok(b) = ws.read().await else { return };
        match wire::Frame::unmarshal(&b) {
            Ok(f) if f.ty == wire::TYPE_REQUEST => first = Some(f),
            _ => return,
        }
        fi.lock().pipelined += 1;
    }
    let mut actions = {
        let mut st = fi.lock();
        let w = wire::Welcome {
            version: wire::VERSION,
            server: "fake".into(),
            snapshot_version: crate::SNAPSHOT_VERSION,
            ghostty: st.commit.clone(),
            features: st.features.clone(),
            ..Default::default()
        };
        send(&mut st.conns, cid, wire::json(wire::TYPE_WELCOME, 0, &w));
        match first {
            Some(f) => handle(&fi, &mut st, cid, f),
            None => Vec::new(),
        }
    };
    loop {
        for a in actions.drain(..) {
            act(&fi, a).await;
        }
        let stall = fi.lock().stalled.clone();
        if let Some(mut rx) = stall {
            let _ = rx.wait_for(|v| *v).await;
        }
        let Ok(b) = ws.read().await else { return };
        let Ok(f) = wire::Frame::unmarshal(&b) else { return };
        if f.ty == wire::TYPE_REQUEST && String::from_utf8_lossy(&f.payload).contains(wire::METHOD_ATTACH) {
            let d = fi.lock().attach_delay;
            tokio::time::sleep(d).await;
        }
        let mut st = fi.lock();
        actions = handle(&fi, &mut st, cid, f);
    }
}

fn drop_conn(fi: &Arc<FakeInner>, cid: u64) {
    let mut st = fi.lock();
    let st = &mut *st;
    let Some(c) = st.conns.remove(&cid) else { return };
    for s in st.sessions.values_mut() {
        s.streams.remove(&cid);
    }
    // Channel streams belong to their connection.
    for key in c.chans.values() {
        if let Some(ch) = st.chans.get_mut(key) {
            // Nothing more reaches the client.
            ch.ended = true;
            stop(ch);
        }
    }
    fi.chan_wake.send_modify(|v| *v += 1);
    let _ = c.out.send(Out::CloseNow);
}

/// stream.close and connection loss: close or kill.
fn stop(ch: &mut FakeChan) {
    if ch.forward {
        close_forward(ch);
    } else if let Some(pid) = ch.pid {
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

fn close_forward(ch: &mut FakeChan) {
    ch.sink = None;
    for t in ch.tasks.drain(..) {
        t.abort();
    }
}

fn end_chan(fi: &FakeInner, conns: &mut HashMap<u64, FakeConn>, ch: &mut FakeChan, e: &wire::StreamEnd) {
    if ch.ended {
        return;
    }
    ch.ended = true;
    fi.chan_wake.send_modify(|v| *v += 1);
    if ch.forward {
        close_forward(ch);
    }
    if let Some(c) = conns.get_mut(&ch.conn) {
        c.chans.remove(&ch.stream);
    }
    send(conns, ch.conn, wire::json(wire::TYPE_STREAM_END, ch.stream, e));
    for id in std::mem::take(&mut ch.on_end) {
        respond(conns, ch.conn, wire::Response { id, ..Default::default() });
    }
}

fn grant_locked(conns: &mut HashMap<u64, FakeConn>, ch: &mut FakeChan) {
    if ch.received > 0 && !ch.ended {
        send(conns, ch.conn, wire::grant(ch.stream, ch.received as u32));
        ch.received = 0;
    }
}

async fn act(fi: &Arc<FakeInner>, a: Action) {
    match a {
        Action::Write(key, data) => {
            let sink = fi.lock().chans.get_mut(&key).and_then(|c| c.sink.take());
            let Some(mut sink) = sink else { return };
            let _ = match &mut sink {
                Sink::Tcp(w) => w.write_all(&data).await,
                Sink::Stdin(w) => w.write_all(&data).await,
            };
            let mut st = fi.lock();
            let st = &mut *st;
            let Some(ch) = st.chans.get_mut(&key) else { return };
            if !ch.ended || !ch.forward {
                ch.sink = Some(sink);
            }
            if ch.no_grant {
            } else if st.lazy_grant && ch.received < 8 << 10 {
                if !ch.grant_timer {
                    ch.grant_timer = true;
                    let fi = fi.clone();
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let mut st = fi.lock();
                        let st = &mut *st;
                        if let Some(ch) = st.chans.get_mut(&key) {
                            ch.grant_timer = false;
                            grant_locked(&mut st.conns, ch);
                        }
                    });
                }
            } else {
                grant_locked(&mut st.conns, ch);
            }
        }
        Action::Eof(key) => {
            let sink = fi.lock().chans.get_mut(&key).and_then(|c| c.sink.take());
            match sink {
                Some(Sink::Tcp(mut w)) => {
                    let _ = w.shutdown().await;
                    // Keep the half alive: dropping an OwnedWriteHalf
                    // shuts down writing, which is what we did.
                    let mut st = fi.lock();
                    if let Some(ch) = st.chans.get_mut(&key) {
                        if !ch.ended {
                            ch.sink = Some(Sink::Tcp(w));
                        }
                    }
                }
                // Dropping stdin closes it.
                Some(Sink::Stdin(_)) | None => {}
            }
        }
    }
}

fn handle(fi: &Arc<FakeInner>, st: &mut FState, cid: u64, f: wire::Frame) -> Vec<Action> {
    let Some(c) = st.conns.get(&cid) else { return Vec::new() };
    if f.stream != 0 {
        if let Some(&key) = c.chans.get(&f.stream) {
            return handle_chan(fi, st, key, f);
        }
    }
    match f.ty {
        wire::TYPE_REQUEST => {
            if let Ok(req) = f.decode::<wire::Request>() {
                return request(fi, st, cid, req);
            }
        }
        wire::TYPE_INPUT => {
            let Some(sid) = st.conns[&cid].streams.get(&f.stream).cloned() else { return Vec::new() };
            let Ok(input) = f.decode::<wire::Input>() else { return Vec::new() };
            let client = st.conns[&cid].client_id.clone();
            let s = st.sessions.get_mut(&sid).unwrap();
            let last = s.last_seq.get(&client).copied().unwrap_or(0);
            if input.seq <= last {
                s.dupes += 1;
            } else {
                s.last_seq.insert(client.clone(), input.seq);
                if s.owner.is_empty() || s.owner == client {
                    s.applied.push(input);
                }
            }
            if !st.no_ack {
                let seq = s.last_seq.get(&client).copied().unwrap_or(0);
                send(&mut st.conns, cid, wire::input_ack(f.stream, seq));
            }
        }
        wire::TYPE_CREDIT => {
            let n = wire::parse_credit(&f.payload).unwrap_or(0) as usize;
            *st.conns.get_mut(&cid).unwrap().credited.entry(f.stream).or_default() += n;
        }
        wire::TYPE_ACK => {
            if let Some(sid) = st.conns[&cid].streams.get(&f.stream) {
                let s = st.sessions.get_mut(sid).unwrap();
                let (gen, end) = wire::parse_ack(&f.payload).unwrap();
                s.acked = s.acked.max(end);
                s.ack_gens.push(gen);
            }
        }
        _ => {}
    }
    Vec::new()
}

fn request(fi: &Arc<FakeInner>, st: &mut FState, cid: u64, req: wire::Request) -> Vec<Action> {
    let mut r = wire::Response { id: req.id, ..Default::default() };
    match req.method.as_str() {
        wire::METHOD_LIST => {
            for s in st.sessions.values() {
                let (cols, rows) = s.term.0.size();
                r.sessions.push(wire::SessionInfo {
                    id: s.id.clone(),
                    cols: cols as i64,
                    rows: rows as i64,
                    owner: s.owner.clone(),
                    ..Default::default()
                });
            }
        }
        wire::METHOD_CREATE => {
            st.creates.push(req.clone());
            let s = new_session(req.cols as usize, req.rows as usize);
            r.session_id = s.id.clone();
            st.sessions.insert(s.id.clone(), s);
        }
        wire::METHOD_KILL => match st.sessions.remove(&req.session) {
            None => r.error = wire::ERR_NOT_FOUND.into(),
            Some(mut s) => {
                respond(&mut st.conns, cid, r);
                end_session(&mut st.conns, &mut s, &wire::StreamEnd { reason: wire::END_KILLED.into(), exit_code: None });
                return Vec::new();
            }
        },
        wire::METHOD_ATTACH => {
            attach(st, cid, req);
            return Vec::new();
        }
        wire::METHOD_FORWARD_OPEN => {
            open_forward(fi, st, cid, req);
            return Vec::new();
        }
        wire::METHOD_EXEC_START => {
            start_exec(fi, st, cid, req);
            return Vec::new();
        }
        wire::METHOD_STREAM_CLOSE => {
            match st.conns[&cid].chans.get(&req.stream).copied() {
                None => r.error = wire::ERR_NOT_FOUND.into(),
                Some(key) => {
                    let ch = st.chans.get_mut(&key).unwrap();
                    if st.close_response_first {
                        respond(&mut st.conns, cid, wire::Response { id: req.id, ..Default::default() });
                    } else {
                        ch.on_end.push(req.id);
                    }
                    if ch.forward {
                        end_chan(fi, &mut st.conns, ch, &wire::StreamEnd { reason: wire::END_CLOSED.into(), exit_code: None });
                    } else if let Some(pid) = ch.pid {
                        unsafe { libc::kill(-pid, st.close_signal.unwrap_or(libc::SIGKILL)) };
                    }
                    return Vec::new();
                }
            }
        }
        wire::METHOD_DETACH => match st.conns.get_mut(&cid).unwrap().streams.remove(&req.stream) {
            None => r.error = wire::ERR_NOT_FOUND.into(),
            Some(sid) => {
                respond(&mut st.conns, cid, r);
                let end = wire::StreamEnd { reason: wire::END_DETACHED.into(), exit_code: None };
                send(&mut st.conns, cid, wire::json(wire::TYPE_STREAM_END, req.stream, &end));
                if let Some(m) = st.sessions.get_mut(&sid).and_then(|s| s.streams.get_mut(&cid)) {
                    m.remove(&req.stream);
                }
                return Vec::new();
            }
        },
        wire::METHOD_OWN => match st.conns[&cid].streams.get(&req.stream).cloned() {
            None => r.error = wire::ERR_NOT_FOUND.into(),
            Some(sid) => {
                let client = st.conns[&cid].client_id.clone();
                let s = st.sessions.get_mut(&sid).unwrap();
                s.owner = client;
                r.owner = s.owner.clone();
                respond(&mut st.conns, cid, r);
                let e = wire::EventMsg { owner: s.owner.clone(), ..wire::EventMsg::kind(wire::EVENT_OWNER) };
                event(&mut st.conns, s, &e);
                return Vec::new();
            }
        },
        wire::METHOD_RESIZE => match st.conns[&cid].streams.get(&req.stream).cloned() {
            None => r.error = wire::ERR_NOT_FOUND.into(),
            Some(sid) => {
                let client = st.conns[&cid].client_id.clone();
                let s = st.sessions.get_mut(&sid).unwrap();
                if s.owner != client {
                    r.error = wire::ERR_NOT_OWNER.into();
                    r.message = "not the owner".into();
                } else {
                    s.term.0.resize(req.cols as usize, req.rows as usize).unwrap();
                    respond(&mut st.conns, cid, r);
                    let e = wire::EventMsg { cols: req.cols, rows: req.rows, ..wire::EventMsg::kind(wire::EVENT_RESIZE) };
                    event(&mut st.conns, s, &e);
                    return Vec::new();
                }
            }
        },
        _ => r.error = wire::ERR_BAD_REQUEST.into(),
    }
    respond(&mut st.conns, cid, r);
    Vec::new()
}

fn attach(st: &mut FState, cid: u64, req: wire::Request) {
    let mut r = wire::Response { id: req.id, ..Default::default() };
    match st.sessions.get(&req.session) {
        None => r.error = wire::ERR_NOT_FOUND.into(),
        Some(s) if !req.epoch.is_empty() && req.epoch != s.epoch => r.error = wire::ERR_ENDED.into(),
        Some(_) if st.conns[&cid].streams.contains_key(&req.stream) => r.error = wire::ERR_BAD_REQUEST.into(),
        Some(_) => {}
    }
    if !r.error.is_empty() {
        respond(&mut st.conns, cid, r);
        return;
    }
    let client = st.conns[&cid].client_id.clone();
    let s = st.sessions.get_mut(&req.session).unwrap();
    if req.own {
        s.owner = client.clone();
    }
    r.epoch = s.epoch.clone();
    r.owner = s.owner.clone();
    r.resumed = req.position.is_some_and(|p| p >= s.evicted && p <= s.next);
    r.head = Some(s.next);
    st.attaches.push(r.clone());
    let resumed = r.resumed;
    respond(&mut st.conns, cid, r);
    let base = wire::input_ack(req.stream, s.last_seq.get(&client).copied().unwrap_or(0));
    if st.hold_base {
        st.held.push((cid, base));
    } else {
        send(&mut st.conns, cid, base);
    }
    st.conns.get_mut(&cid).unwrap().streams.insert(req.stream, s.id.clone());
    s.streams.entry(cid).or_default().insert(req.stream, 0);
    if resumed {
        let p = req.position.unwrap();
        for rec in s.log.clone() {
            if rec.end > p {
                send(&mut st.conns, cid, record_frame(req.stream, &rec));
            }
        }
        return;
    }
    snapshot(&mut st.conns, cid, req.stream, s, true, &mut st.drop_after_ready);
}

fn new_chan(st: &mut FState, cid: u64, stream: u32, forward: bool, sink: Sink) -> u64 {
    st.next_chan += 1;
    let key = st.next_chan;
    st.chans.insert(
        key,
        FakeChan {
            conn: cid,
            stream,
            credit: wire::CHANNEL_WINDOW as i64,
            received: 0,
            over: false,
            ended: false,
            max_fly: 0,
            out_eof: 0,
            in_eof: false,
            forward,
            pid: None,
            sink: Some(sink),
            tasks: Vec::new(),
            no_grant: false,
            on_end: Vec::new(),
            pumped: 0,
            uncredited: 0,
            credited: 0,
            grant_timer: false,
            pumps_done: 0,
        },
    );
    st.conns.get_mut(&cid).unwrap().chans.insert(stream, key);
    st.chan_order.push(key);
    key
}

fn open_forward(fi: &Arc<FakeInner>, st: &mut FState, cid: u64, req: wire::Request) {
    let mut r = wire::Response { id: req.id, ..Default::default() };
    let c = &st.conns[&cid];
    if !wire::has_feature(&st.features, wire::FEATURE_FORWARD) || c.chans.contains_key(&req.stream) || c.streams.contains_key(&req.stream) {
        r.error = wire::ERR_BAD_REQUEST.into();
        respond(&mut st.conns, cid, r);
        return;
    }
    let tcp = match std::net::TcpStream::connect((req.host.as_str(), req.port as u16)) {
        Ok(t) => t,
        Err(e) => {
            r.error = wire::ERR_UNREACHABLE.into();
            r.message = e.to_string();
            respond(&mut st.conns, cid, r);
            return;
        }
    };
    tcp.set_nonblocking(true).unwrap();
    let (rd, wr) = tokio::net::TcpStream::from_std(tcp).unwrap().into_split();
    let key = new_chan(st, cid, req.stream, true, Sink::Tcp(wr));
    respond(&mut st.conns, cid, r);
    let t = tokio::task::spawn_local(pump(fi.clone(), key, wire::CHANNEL_STDOUT, Box::new(rd)));
    st.chans.get_mut(&key).unwrap().tasks.push(t.abort_handle());
}

fn start_exec(fi: &Arc<FakeInner>, st: &mut FState, cid: u64, req: wire::Request) {
    let mut r = wire::Response { id: req.id, ..Default::default() };
    if !wire::has_feature(&st.features, wire::FEATURE_EXEC) || req.argv.is_empty() {
        r.error = wire::ERR_BAD_REQUEST.into();
        respond(&mut st.conns, cid, r);
        return;
    }
    let mut cmd = tokio::process::Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
    if !req.cwd.is_empty() {
        cmd.current_dir(&req.cwd);
    }
    for kv in &req.env {
        if let Some((k, v)) = kv.split_once('=') {
            cmd.env(k, v);
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            r.error = if e.kind() == std::io::ErrorKind::NotFound { wire::ERR_NOT_FOUND } else { wire::ERR_INTERNAL }.into();
            respond(&mut st.conns, cid, r);
            return;
        }
    };
    let (stdin, stdout, stderr) = (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let key = new_chan(st, cid, req.stream, false, Sink::Stdin(stdin));
    st.chans.get_mut(&key).unwrap().pid = child.id().map(|p| p as i32);
    respond(&mut st.conns, cid, r);
    tokio::task::spawn_local(pump(fi.clone(), key, wire::CHANNEL_STDOUT, Box::new(stdout)));
    tokio::task::spawn_local(pump(fi.clone(), key, wire::CHANNEL_STDERR, Box::new(stderr)));
    let fi = fi.clone();
    tokio::task::spawn_local(async move {
        let status = child.wait().await;
        // Both pipes drained first, so no output is lost after the exit.
        let mut rx = fi.chan_wake.subscribe();
        loop {
            {
                let st = fi.lock();
                if st.chans.get(&key).is_none_or(|c| c.pumps_done >= 2 || c.ended) {
                    break;
                }
            }
            let _ = rx.changed().await;
        }
        let (mut code, mut reason) = (-1, wire::END_EXITED);
        if let Ok(s) = status {
            match (s.code(), s.signal()) {
                (Some(c), _) => code = c as i64,
                (None, Some(sig)) => (code, reason) = (128 + sig as i64, wire::END_KILLED),
                _ => {}
            }
        }
        let mut st = fi.lock();
        let st = &mut *st;
        if let Some(ch) = st.chans.get_mut(&key) {
            end_chan(&fi, &mut st.conns, ch, &wire::StreamEnd { reason: reason.into(), exit_code: Some(code) });
        }
    });
}

type Src = Box<dyn tokio::io::AsyncRead + Unpin>;

/// Sends src as CHANNEL_DATA on channel n within the client's credit,
/// reading no further ahead than that.
async fn pump(fi: Arc<FakeInner>, key: u64, n: u8, mut src: Src) {
    let mut buf = vec![0u8; wire::MAX_CHANNEL_DATA];
    let finish = |fi: &FakeInner| {
        let mut st = fi.lock();
        if let Some(ch) = st.chans.get_mut(&key) {
            ch.pumps_done += 1;
        }
        fi.chan_wake.send_modify(|v| *v += 1);
    };
    loop {
        let room = loop {
            let mut rx = fi.chan_wake.subscribe();
            {
                let st = fi.lock();
                let ch = &st.chans[&key];
                if ch.ended {
                    drop(st);
                    finish(&fi);
                    return;
                }
                let fly = st.chan_fly;
                if !(ch.credit <= 1 || fly > 0 && ch.uncredited + 1 >= fly) {
                    let mut room = (ch.credit - 1).min(buf.len() as i64);
                    if fly > 0 {
                        room = room.min(fly - ch.uncredited - 1);
                    }
                    break room as usize;
                }
            }
            let _ = rx.changed().await;
        };
        let k = src.read(&mut buf[..room]).await.unwrap_or(0);
        let mut st = fi.lock();
        let st = &mut *st;
        let ch = st.chans.get_mut(&key).unwrap();
        if ch.ended {
            ch.pumps_done += 1;
            fi.chan_wake.send_modify(|v| *v += 1);
            return;
        }
        if k > 0 {
            ch.credit -= 1 + k as i64;
            ch.uncredited += 1 + k as i64;
            ch.pumped += k;
            send(&mut st.conns, ch.conn, wire::channel_data(ch.stream, n, &buf[..k]));
            continue;
        }
        send(&mut st.conns, ch.conn, wire::channel_eof(ch.stream, n));
        ch.out_eof += 1;
        ch.pumps_done += 1;
        fi.chan_wake.send_modify(|v| *v += 1);
        // A forward ends once both directions are done.
        if ch.forward && ch.in_eof {
            end_chan(&fi, &mut st.conns, ch, &wire::StreamEnd { reason: wire::END_CLOSED.into(), exit_code: None });
        }
        return;
    }
}

/// Takes a client frame for a channel; the lock is held.
fn handle_chan(fi: &Arc<FakeInner>, st: &mut FState, key: u64, f: wire::Frame) -> Vec<Action> {
    let ch = st.chans.get_mut(&key).unwrap();
    match f.ty {
        wire::TYPE_CREDIT => {
            let n = wire::parse_credit(&f.payload).unwrap_or(0) as i64;
            ch.credit += n;
            ch.credited += n;
            ch.uncredited = (ch.uncredited - n).max(0);
            fi.chan_wake.send_modify(|v| *v += 1);
        }
        wire::TYPE_DATA => {
            ch.received += f.payload.len() as i64;
            if ch.received > wire::CHANNEL_WINDOW as i64 {
                ch.over = true;
            }
            ch.max_fly = ch.max_fly.max(ch.received);
            return vec![Action::Write(key, f.payload)];
        }
        wire::TYPE_EOF => {
            ch.in_eof = true;
            if ch.forward && ch.out_eof > 0 {
                // Half-close, then end: both directions are done.
                let out = vec![Action::Eof(key)];
                end_chan(fi, &mut st.conns, ch, &wire::StreamEnd { reason: wire::END_CLOSED.into(), exit_code: None });
                return out;
            }
            if !ch.forward {
                // Closing stdin.
                ch.sink = None;
                return Vec::new();
            }
            return vec![Action::Eof(key)];
        }
        _ => {}
    }
    Vec::new()
}
