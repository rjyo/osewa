//! osewa's network server (docs/PROTOCOL.md, Processes): it terminates
//! pinned mutual TLS, speaks the frame protocol over WebSocket, spawns
//! holders and relays frames between clients and holders. It keeps no
//! session state; the holders' sockets in the run directory are the truth.
//!
//! Everything runs on one thread: each client connection, each holder
//! connection and each channel stream is a task of a current-thread runtime
//! sharing state through RefCells, never borrowed across an await.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::time::Duration;

use osewa_wire as wire;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::Notify;

mod channel;
mod client;
pub mod conn;
mod dial;
mod flight;
mod limits;
pub mod local;
mod log;
mod lowat;
pub mod path;
mod cli;
mod queue;
mod trace;

pub use conn::{Conn, LocalBoxFuture};
pub use cli::{check_args, main};
pub use channel::open_channels;

use client::Client;

/// Starts a holder and resolves once its socket accepts connections.
pub type Spawn = Rc<dyn Fn(osewa_holder::Config) -> LocalBoxFuture<'static, Result<(), String>>>;

/// Limits on the per-client send queue, on talking to holders and on what a
/// client may ask for. Tests shrink them.
#[derive(Clone, Debug)]
pub struct Tunables {
    pub send_soft: usize,
    pub send_hard: usize,
    pub write_timeout: Duration,
    pub dial_timeout: Duration,
    pub holder_timeout: Duration,
    pub spawn_timeout: Duration,
    /// Bounds input waiting for one holder that stopped reading; it is far
    /// above the holder's 64 KiB pending-input cap.
    pub stream_queue_bytes: usize,
    /// stale_age and stale_retry decide when a refusing socket is stale: a
    /// holder refuses between bind and listen, and darwin refuses when the
    /// backlog is full, so one refusal proves nothing.
    pub stale_age: Duration,
    pub stale_retry: Duration,
    /// How long close waits for exec streams to end: past execproc's 2 s
    /// SIGHUP-to-SIGKILL grace, whose timer dies with this process.
    pub exec_grace: Duration,
    pub forward_timeout: Duration,
    /// Bounds one channel stream's frames in the send queue. Two frames
    /// keep the link busy; more would only sit ahead of the next
    /// keystroke's echo once the writer picks this stream again.
    pub chan_queue_bytes: usize,
    /// How long an exec stream that signaled its process group waits for
    /// the group to go. It outlasts execproc's SIGKILL at 2 s and stays
    /// inside exec_grace.
    pub group_grace: Duration,
    /// execproc's SIGHUP-to-SIGKILL grace.
    pub kill_grace: Duration,
    pub max_streams: usize,
    /// Create, list and kill requests in flight per client.
    pub max_busy: usize,
    /// How young a tagged session session.reconcile leaves alone.
    pub reconcile_grace: Duration,
    /// Bounds the TLS handshake and the WebSocket upgrade together, as
    /// a slow client could otherwise hold a connection between the two.
    pub handshake_timeout: Duration,
}

impl Default for Tunables {
    fn default() -> Tunables {
        Tunables {
            send_soft: 4 << 20,
            send_hard: 16 << 20,
            write_timeout: Duration::from_secs(30),
            dial_timeout: Duration::from_secs(2),
            holder_timeout: Duration::from_secs(5),
            spawn_timeout: Duration::from_secs(10),
            stream_queue_bytes: 4 << 20,
            stale_age: Duration::from_secs(30),
            stale_retry: Duration::from_millis(250),
            exec_grace: Duration::from_secs(3),
            forward_timeout: Duration::from_secs(10),
            chan_queue_bytes: 2 * (wire::HEADER_SIZE + wire::MAX_CHANNEL_DATA),
            group_grace: Duration::from_millis(2500),
            kill_grace: osewa_execproc::KILL_GRACE,
            max_streams: wire::MAX_STREAMS,
            max_busy: 16,
            reconcile_grace: local::RECONCILE_GRACE,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// Configures a Server.
#[derive(Clone, Default)]
pub struct Config {
    /// The server's key and the enrolled devices; both required.
    pub identity: Option<Rc<osewa_auth::Identity>>,
    pub devices: Option<osewa_auth::Devices>,
    pub run_dir: PathBuf,
    /// Reported in WELCOME; default the hostname.
    pub name: String,
    /// Starts a holder. The default runs `<self> holder ...` detached, or
    /// $OSEWA_HOLDER's holder when that is set.
    pub spawn: Option<Spawn>,
    /// The default argv for session.create; default the account's login
    /// shell (passwd), as a login shell.
    pub shell: Vec<String>,
    /// Sessions' and exec.start's home in place of the account's: an
    /// absolute path. `osewa server` takes it from OSEWA_HOME (tests and
    /// sandboxes); holders it starts get it the same way.
    pub home: Option<PathBuf>,
    /// Turns off permessage-deflate toward clients.
    pub no_compress: bool,
    /// Leave the forward and exec features out of WELCOME, which turns off
    /// forward.open and exec.start.
    pub no_forward: bool,
    pub no_exec: bool,
    /// Leaves the QUIC carrier off.
    pub no_quic: bool,
    /// If set, receives OSEWA_TRACE events in server-<pid>.jsonl; main
    /// takes it from $OSEWA_TRACE.
    pub trace_dir: String,
    pub tune: Tunables,
}

/// Serves osewa clients.
pub struct Server {
    cfg: Config,
    spawn: Spawn,
    trace: Option<Rc<trace::Tracer>>,
    st: RefCell<State>,
    /// Exec streams still running, for close.
    execs: Cell<usize>,
    execs_done: Notify,
    /// Fires once close starts, for listeners.
    closing: Notify,
    /// Anyone who can reach the port can fail a handshake, and each failure
    /// is a log line.
    log: RefCell<limits::LimitedLog<log::Stderr>>,
}

/// How long a stopping server waits for its QUIC clients' closes to be
/// sent and acknowledged.
const QUIC_DRAIN: Duration = Duration::from_secs(1);

/// The WebSocket carrier.
/// The WebSocket carrier, with its TCP socket's probe and (client, ours)
/// addresses.
struct WsConn<S>(osewa_carrier::ws::Conn<S>, Option<path::Probe>, Option<(SocketAddr, SocketAddr)>);

fn ws_err(e: osewa_carrier::ws::Error) -> io::Error {
    match e {
        osewa_carrier::ws::Error::Io(e) => e,
        e => io::Error::other(e.to_string()),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + 'static> Conn for WsConn<S> {
    fn read(&self) -> LocalBoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(async { self.0.read().await.map_err(ws_err) })
    }
    fn write<'a>(&'a self, frame: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>> {
        // A session's frames are what someone is looking at, and a screen
        // is all latency on a slow link: they are worth the denser search.
        // Channel data is bulk, where speed counts.
        let level = match frame.first() {
            Some(&t) if (wire::TYPE_SNAPSHOT_BEGIN..=wire::TYPE_EVENT).contains(&t) => osewa_carrier::ws::Level::Dense,
            _ => osewa_carrier::ws::Level::Fast,
        };
        Box::pin(async move { self.0.write_level(frame, level).await.map_err(ws_err) })
    }
    fn close(&self, code: u16, reason: String) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.0.close(code, &reason).await;
        })
    }
    fn path(&self, now: std::time::Instant) -> Option<path::Sample> {
        self.1.as_ref()?.sample(now)
    }
    fn addrs(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.2
    }
}

/// The QUIC carrier.
/// The QUIC carrier, with its (client, ours) addresses.
struct QuicConn(osewa_carrier::quic::Conn, Option<(SocketAddr, SocketAddr)>);

impl Conn for QuicConn {
    fn read(&self) -> LocalBoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(self.0.read())
    }
    fn write<'a>(&'a self, frame: &'a [u8]) -> LocalBoxFuture<'a, io::Result<()>> {
        Box::pin(self.0.write(frame))
    }
    fn close(&self, code: u16, reason: String) -> LocalBoxFuture<'_, ()> {
        self.0.close(code, &reason);
        Box::pin(async {})
    }
    fn addrs(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.1
    }
}

#[derive(Default)]
struct State {
    clients: HashMap<u64, Weak<Client>>,
    next: u64,
    closed: bool,
    /// The UDP port QUIC serves, advertised in WELCOME; 0 for none.
    quic_port: u16,
    /// One lock per (creator, tag) a create is working on, so two creates
    /// of one tag can't both start a holder.
    tags: HashMap<(String, String), Rc<tokio::sync::Mutex<()>>>,
}

impl Server {
    /// Checks cfg, fills in defaults and creates the run directory.
    pub fn new(mut cfg: Config) -> Result<Rc<Server>, String> {
        if cfg.identity.is_none() || cfg.devices.is_none() {
            return Err("server: identity and devices are required".into());
        }
        if cfg.run_dir.as_os_str().is_empty() {
            cfg.run_dir = osewa_paths::run_dir();
        }
        osewa_paths::ensure(&cfg.run_dir).map_err(|e| e.to_string())?;
        if cfg.name.is_empty() {
            cfg.name = hostname();
        }
        osewa_paths::check_home(cfg.home.as_deref()).map_err(|e| format!("server: {e}"))?;
        if cfg.shell.is_empty() {
            cfg.shell = osewa_paths::account(cfg.home.as_deref()).login_shell();
        }
        let spawn = cfg.spawn.clone().unwrap_or_else(|| {
            let timeout = cfg.tune.spawn_timeout;
            Rc::new(move |h| Box::pin(spawn_process(h, timeout)))
        });
        let trace = trace::Tracer::new(&cfg.trace_dir).map(Rc::new);
        Ok(Rc::new(Server {
            cfg,
            spawn,
            trace,
            st: RefCell::default(),
            execs: Cell::new(0),
            execs_done: Notify::new(),
            closing: Notify::new(),
            log: RefCell::new(limits::LimitedLog::new(log::Stderr, 20, Duration::from_secs(60))),
        }))
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// What WELCOME advertises besides the carrier the client came in on.
    fn carriers(&self) -> Vec<wire::Carrier> {
        let port = self.st.borrow().quic_port;
        if port == 0 {
            return Vec::new();
        }
        vec![wire::Carrier { carrier: "quic".into(), port: port.into(), alpn: vec![wire::SUBPROTOCOL.into()] }]
    }

    /// Sets the UDP port the QUIC carrier serves, for WELCOME.
    pub fn set_quic_port(&self, port: u16) {
        self.st.borrow_mut().quic_port = port;
    }

    /// What WELCOME offers.
    fn features(&self) -> Vec<String> {
        let mut f = Vec::new();
        if !self.cfg.no_forward {
            f.push(wire::FEATURE_FORWARD.to_string());
        }
        if !self.cfg.no_exec {
            f.push(wire::FEATURE_EXEC.to_string());
        }
        f
    }

    pub fn is_closed(&self) -> bool {
        self.st.borrow().closed
    }

    /// Resolves once close has started.
    pub async fn closed(&self) {
        while !self.is_closed() {
            self.closing.notified().await;
        }
    }

    /// Accepts TLS connections on ln until close: pinned mutual TLS (only
    /// enrolled devices get past the handshake), then the WebSocket upgrade
    /// on wire::PATH, then a client.
    pub async fn serve(self: &Rc<Self>, ln: TcpListener) -> io::Result<()> {
        let (id, devs) = (self.cfg.identity.as_ref().unwrap(), self.cfg.devices.as_ref().unwrap());
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(osewa_auth::server_config(id, devs)));
        let ws = osewa_carrier::ws::Acceptor { compress: !self.cfg.no_compress, ..Default::default() };
        loop {
            let accepted = tokio::select! {
                r = ln.accept() => r,
                _ = self.closed() => return Ok(()),
            };
            let (tcp, peer) = match accepted {
                Ok(a) => a,
                Err(e) => {
                    // Out of descriptors and the like: back off rather than
                    // spin.
                    self.log.borrow_mut().line(&format!("osewa server: accept: {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            // TCP_NODELAY on every TCP connection: without it a
            // keystroke's echo waits on Nagle behind delayed ACKs (~40 ms).
            let _ = tcp.set_nodelay(true);
            lowat::tune(tcp.as_raw_fd());
            let probe = path::Probe::new(tcp.as_raw_fd());
            let addrs = tcp.local_addr().ok().map(|local| (peer, local));
            let (srv, tls, ws) = (self.clone(), tls.clone(), ws.clone());
            tokio::task::spawn_local(async move {
                let deadline = tokio::time::Instant::now() + srv.cfg.tune.handshake_timeout;
                let stream = match tokio::time::timeout_at(deadline, tls.accept(tcp)).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => return srv.log.borrow_mut().line(&format!("http: TLS handshake error from {peer}: {e}")),
                    Err(_) => return srv.log.borrow_mut().line(&format!("http: TLS handshake error from {peer}: timeout")),
                };
                // A refused upgrade has had its answer; nothing to log.
                let Ok(Ok(conn)) = tokio::time::timeout_at(deadline, ws.accept(stream)).await else { return };
                srv.serve_conn(Rc::new(WsConn(conn, Some(probe), addrs))).await;
            });
        }
    }

    /// Serves the QUIC carrier (docs/QUIC.md) on sock behind the same
    /// identity and device check as serve, and advertises its port in
    /// WELCOME, until close. $OSEWA_QUIC_CC picks the congestion
    /// controller.
    pub async fn serve_quic(self: &Rc<Self>, sock: std::net::UdpSocket) -> io::Result<()> {
        use osewa_carrier::quic;
        let (id, devs) = (self.cfg.identity.as_ref().unwrap(), self.cfg.devices.as_ref().unwrap());
        let tls = osewa_auth::server_config(id, devs);
        let cfg = quic::server_config(tls, !self.cfg.no_compress, quic::Congestion::from_env())?;
        let port = sock.local_addr()?.port();
        let ep = quic::endpoint(sock, Some(cfg))?;
        if self.is_closed() {
            return Ok(());
        }
        self.set_quic_port(port);
        loop {
            let incoming = tokio::select! {
                i = ep.accept() => i,
                _ = self.closed() => None,
            };
            let Some(incoming) = incoming else { break };
            let srv = self.clone();
            tokio::task::spawn_local(async move {
                // One deadline for the handshake (where a refused device
                // fails) and the main stream's first frame, as over TCP.
                let deadline = tokio::time::Instant::now() + srv.cfg.tune.handshake_timeout;
                let Ok(Ok(qc)) = tokio::time::timeout_at(deadline, incoming).await else { return };
                let addrs = qc.local_ip().map(|ip| (qc.remote_address(), SocketAddr::new(ip, port)));
                let Ok(Ok(conn)) = tokio::time::timeout_at(deadline, quic::accept(qc.clone())).await else {
                    qc.close(0u32.into(), b"");
                    return;
                };
                srv.serve_conn(Rc::new(QuicConn(conn, addrs))).await;
            });
        }
        // close is telling every client (going away); give their
        // CONNECTION_CLOSEs a moment to go out before the socket does, and
        // refuse new handshakes meanwhile instead of leaving them to time
        // out.
        let drain = tokio::time::sleep(QUIC_DRAIN);
        tokio::pin!(drain);
        loop {
            tokio::select! {
                _ = &mut drain => break,
                _ = ep.wait_idle() => break,
                Some(incoming) = ep.accept() => incoming.refuse(),
            }
        }
        ep.close(conn::CLOSE_GOING_AWAY.into(), b"server closing");
        Ok(())
    }

    /// Runs a client on a carrier connection until it ends.
    pub async fn serve_conn(self: &Rc<Self>, conn: Rc<dyn Conn>) {
        let c = Client::new(self.clone(), conn.clone());
        let key = {
            let mut st = self.st.borrow_mut();
            if st.closed {
                None
            } else {
                st.next += 1;
                let key = st.next;
                st.clients.insert(key, Rc::downgrade(&c));
                Some(key)
            }
        };
        let Some(key) = key else {
            conn.close(conn::CLOSE_GOING_AWAY, "server closing".into()).await;
            return;
        };
        c.run().await;
        self.st.borrow_mut().clients.remove(&key);
    }

    /// Drops every client and stops the listeners. Holders keep running;
    /// exec processes get SIGHUP, and close waits up to exec_grace for them
    /// to end, so that a process ignoring SIGHUP still gets its SIGKILL.
    pub async fn close(&self) {
        let clients: Vec<Rc<Client>> = {
            let mut st = self.st.borrow_mut();
            st.closed = true;
            std::mem::take(&mut st.clients).into_values().filter_map(|w| w.upgrade()).collect()
        };
        self.closing.notify_waiters();
        for c in clients {
            c.close(conn::CLOSE_GOING_AWAY, "server closing");
        }
        let wait = async {
            while self.execs.get() > 0 {
                self.execs_done.notified().await;
            }
        };
        let _ = tokio::time::timeout(self.cfg.tune.exec_grace, wait).await;
    }

    fn exec_started(&self) {
        self.execs.set(self.execs.get() + 1);
    }

    fn exec_done(&self) {
        self.execs.set(self.execs.get() - 1);
        self.execs_done.notify_waiters();
    }

    /// Holds (creator, tag)'s lock while f runs.
    pub(crate) async fn with_tag<T>(&self, creator: &str, tag: &str, f: impl std::future::Future<Output = T>) -> T {
        let key = (creator.to_string(), tag.to_string());
        let lock = self.st.borrow_mut().tags.entry(key.clone()).or_default().clone();
        let r = {
            let _held = lock.lock().await;
            f.await
        };
        let mut st = self.st.borrow_mut();
        // Ours and the map's: nobody else is waiting.
        if st.tags.get(&key).is_some_and(|l| Rc::strong_count(l) == 2) {
            st.tags.remove(&key);
        }
        drop(st);
        drop(lock);
        r
    }

    /// This server's holders.
    pub(crate) fn holders(&self) -> local::Holders<'_> {
        local::Holders { run_dir: &self.cfg.run_dir, tune: &self.cfg.tune }
    }

    /// Connects to a session's holder and introduces client_id.
    async fn dial_holder(&self, id: &str, client_id: &str) -> io::Result<UnixStream> {
        self.holders().dial(id, client_id).await
    }

    /// Asks the holder at the other end of a fresh connection for its
    /// session's info, over that connection, before anything else goes
    /// over it: why this server can't serve it, or empty. Checking on
    /// another connection would let a holder that took the socket in
    /// between through. No answer with the session's info counts as
    /// another build.
    async fn check_build<R, W>(&self, rd: &mut R, wr: &mut W, client_id: &str) -> io::Result<String>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let req = wire::Request { id: 1, method: wire::METHOD_INFO.into(), client_id: client_id.into(), ..Default::default() };
        let call = async {
            let mut b = Vec::new();
            wire::json(wire::TYPE_REQUEST, 0, &req).encode_to(&mut b)?;
            wr.write_all(&b).await?;
            loop {
                let f = read_frame(rd).await?;
                if f.ty != wire::TYPE_RESPONSE {
                    continue;
                }
                let r: wire::Response = f.decode()?;
                if r.id == req.id {
                    return Ok::<_, io::Error>(r);
                }
            }
        };
        let r: wire::Response = tokio::time::timeout(self.cfg.tune.holder_timeout, call)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "holder: i/o timeout"))??;
        Ok(match r.sessions.into_iter().next() {
            Some(s) if r.error.is_empty() => incompatible(s.build.as_ref()),
            _ if r.error.is_empty() => "its holder didn't report the session".into(),
            _ => format!("its holder didn't report the session: {}: {}", r.error, r.message),
        })
    }
}

/// This server's build, as its WELCOME gives it.
pub(crate) fn own_build() -> wire::Build {
    wire::Build { version: wire::VERSION, ghostty: osewa_ghostty::COMMIT.into(), snapshot_version: osewa_ghostty::SNAPSHOT_VERSION }
}

/// Why a holder of build `b` can't be served here, or empty. A holder
/// outlives the server that started it, so after an update the new server
/// finds the old build's holders: their snapshots are in a format other
/// than the one WELCOME promised the client.
pub(crate) fn incompatible(b: Option<&wire::Build>) -> String {
    let own = own_build();
    match b {
        Some(b) if b.compatible(&own) => String::new(),
        Some(b) => format!("its holder is build {}, the server {}", b.id(), own.id()),
        None => format!("its holder doesn't report its build (an older osewa), the server is {}", own.id()),
    }
}

/// Reports a socket file with nothing accepting on it right now.
fn refused(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::ConnectionRefused
}

/// Reads one length-prefixed frame from a holder socket.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> io::Result<wire::Frame> {
    let mut lb = [0u8; 4];
    r.read_exact(&mut lb).await?;
    let n = u32::from_le_bytes(lb) as usize;
    wire::check_len(n)?;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await?;
    Ok(wire::Frame::unmarshal(&b)?)
}

fn hostname() -> String {
    let mut b = [0u8; 256];
    if unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return String::new();
    }
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// The holder's command line after the binary.
fn holder_args(cfg: &osewa_holder::Config) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "holder".into(),
        "--run".into(),
        cfg.run_dir.to_string_lossy().into_owned(),
        "--id".into(),
        cfg.id.clone(),
        "--cols".into(),
        cfg.cols.to_string(),
        "--rows".into(),
        cfg.rows.to_string(),
    ];
    if cfg.width > 0 && cfg.height > 0 {
        a.extend(["--width".into(), cfg.width.to_string(), "--height".into(), cfg.height.to_string()]);
    }
    if !cfg.cwd.is_empty() {
        a.push("--cwd".into());
        a.push(cfg.cwd.clone());
    }
    // The env (and a look) go on stdin (holder_stdin), never on this line,
    // which every user on the host can read. The env alone keeps the array
    // a holder of another build ($OSEWA_HOLDER) reads.
    a.push(if stdin_json(cfg) { "--stdin-json" } else { "--env-stdin" }.into());
    a.push("--".into());
    a.extend(cfg.argv.iter().cloned());
    a
}

/// What the holder reads on stdin (holder_args' flag): the env as a JSON
/// array (--env-stdin), or with a look an object {env, look}
/// (--stdin-json).
fn holder_stdin(cfg: &osewa_holder::Config) -> serde_json::Result<Vec<u8>> {
    if !stdin_json(cfg) {
        return serde_json::to_vec(&cfg.env);
    }
    let mut v = serde_json::json!({"env": cfg.env});
    if !cfg.look.is_empty() {
        v["look"] = serde_json::to_value(&cfg.look)?;
    }
    if !cfg.tag.is_empty() {
        v["tag"] = cfg.tag.clone().into();
        v["creator"] = cfg.creator.clone().into();
    }
    serde_json::to_vec(&v)
}

/// Whether the holder takes stdin as an object (a look or a tag to give),
/// or the env array alone.
fn stdin_json(cfg: &osewa_holder::Config) -> bool {
    !cfg.look.is_empty() || !cfg.tag.is_empty()
}

/// The binary whose holder runs a session: $OSEWA_HOLDER, which lets one
/// build's server spawn another's holders, else this one.
fn holder_binary() -> io::Result<PathBuf> {
    match std::env::var_os("OSEWA_HOLDER") {
        Some(p) if !p.is_empty() => Ok(p.into()),
        _ => std::env::current_exe(),
    }
}

/// Runs `<binary> holder ...` in its own session, so the holder outlives
/// the server, and waits for its "ready" line.
async fn spawn_process(mut cfg: osewa_holder::Config, timeout: Duration) -> Result<(), String> {
    if std::env::var_os("OSEWA_HOLDER").is_some_and(|p| !p.is_empty()) {
        // Another build's holder may predate --stdin-json; the look is
        // cosmetic, so it goes, and the env takes the array every build
        // reads. So does the tag: that holder is never reused or
        // reconciled, as before tags.
        cfg.look = Default::default();
        cfg.tag.clear();
        cfg.creator.clear();
    }
    let mut cmd = tokio::process::Command::new(holder_binary().map_err(|e| e.to_string())?);
    cmd.args(holder_args(&cfg)).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // The home is the server's to give, not whatever its environment has.
    match &cfg.home {
        Some(h) => cmd.env(osewa_paths::HOME_OVERRIDE, h),
        None => cmd.env_remove(osewa_paths::HOME_OVERRIDE),
    };
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("server: start holder: {e}"))?;
    // The client's env (and look) go in on stdin, while await_ready drains
    // stdout and stderr, under its one deadline: a holder that writes
    // first, or never reads, can't hang the create.
    let env = holder_stdin(&cfg).map_err(|e| e.to_string())?;
    let stdin = child.stdin.take();
    let deliver = async move {
        let Some(mut stdin) = stdin else { return Ok(()) };
        stdin.write_all(&env).await?;
        stdin.shutdown().await
    };
    await_ready(child, deliver, timeout).await
}

/// How much of a holder's stderr an early exit reports.
const STDERR_TAIL: usize = 2 << 10;
/// Waits for the started child to print "ready", with deliver (its stdin)
/// running alongside, all within timeout. Afterwards it keeps draining
/// stdout, so a later write can't kill the holder with SIGPIPE, and reaps
/// the process when it exits.
async fn await_ready(
    mut child: tokio::process::Child,
    deliver: impl std::future::Future<Output = io::Result<()>>,
    timeout: Duration,
) -> Result<(), String> {
    let Some(out) = child.stdout.take() else { return Err("server: holder has no stdout".into()) };
    // The holder's stderr goes on to ours, its tail kept for an early exit
    // whatever becomes of the forward.
    let tail: Rc<std::cell::RefCell<Vec<u8>>> = Rc::default();
    let (err_done, err_eof) = tokio::sync::oneshot::channel::<()>();
    if let Some(mut err) = child.stderr.take() {
        let tail = tail.clone();
        tokio::task::spawn_local(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                {
                    let mut t = tail.borrow_mut();
                    t.extend_from_slice(&buf[..n]);
                    let cut = t.len().saturating_sub(STDERR_TAIL);
                    t.drain(..cut);
                }
                crate::log::bytes(buf[..n].to_vec());
            }
            let _ = err_done.send(());
        });
    }
    let mut br = BufReader::new(out);
    let mut line = String::new();
    let mut delivered: Option<io::Result<()>> = None;
    tokio::pin!(deliver);
    let deadline = tokio::time::Instant::now() + timeout;
    let read = async {
        let read = br.read_line(&mut line);
        tokio::pin!(read);
        loop {
            tokio::select! {
                d = &mut deliver, if delivered.is_none() => delivered = Some(d),
                r = &mut read => return r,
            }
        }
    };
    let read = tokio::time::timeout_at(deadline, read).await;
    // A holder that died mid-delivery: let the write see it, to say so.
    if delivered.is_none() && !matches!(read, Ok(Ok(_)) if line.trim() == "ready") {
        if let Ok(d) = tokio::time::timeout(Duration::from_millis(200), &mut deliver).await {
            delivered = Some(d);
        }
    }
    let mut r = match read {
        Err(_) => Err("server: holder did not become ready".to_string()),
        Ok(_) if line.trim() == "ready" => Ok(()),
        // EOF before a whole line: the holder died before saying ready.
        Ok(Ok(_)) if !line.ends_with('\n') => Err("server: holder exited before ready".into()),
        Ok(Err(e)) => Err(format!("server: holder exited before ready: {e}")),
        Ok(Ok(_)) => Err(format!("server: holder said {}", wire::quote(&line))),
    };
    if let (Err(e), Some(Err(w))) = (&mut r, &delivered) {
        *e += &format!(" (writing its env: {w})");
    }
    if let Err(e) = &mut r {
        let _ = child.start_kill();
        // Why it died: its status and what it said last.
        if let Ok(Ok(st)) = tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
            *e += &format!(": {st}");
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), err_eof).await;
        let said = String::from_utf8_lossy(&tail.borrow()).trim().to_string();
        if !said.is_empty() {
            *e += &format!(": {}", osewa_wire::quote(&said));
        }
    }
    tokio::task::spawn_local(async move {
        let _ = tokio::io::copy(&mut br, &mut tokio::io::sink()).await;
        let _ = child.wait().await;
    });
    r
}

#[cfg(test)]
mod tests;
