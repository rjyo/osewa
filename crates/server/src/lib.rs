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
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime};

use osewa_wire as wire;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::Notify;

mod channel;
mod client;
pub mod conn;
mod flight;
mod limits;
mod lowat;
mod cli;
mod queue;
mod trace;

pub use conn::{Conn, LocalBoxFuture};
pub use cli::{check_args, main};

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
    /// The default argv for session.create; default $SHELL -l, else
    /// /bin/sh.
    pub shell: Vec<String>,
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
    log: RefCell<limits::LimitedLog<io::Stderr>>,
}

/// Bounds the TLS handshake, as Go's http.Server does with its
/// ReadHeaderTimeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The WebSocket carrier.
struct WsConn<S>(osewa_carrier::ws::Conn<S>);

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
        Box::pin(async { self.0.write(frame).await.map_err(ws_err) })
    }
    fn close(&self, code: u16, reason: String) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.0.close(code, &reason).await;
        })
    }
}

#[derive(Default)]
struct State {
    clients: HashMap<u64, Weak<Client>>,
    next: u64,
    closed: bool,
    /// The UDP port QUIC serves, advertised in WELCOME; 0 for none.
    quic_port: u16,
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
        if cfg.shell.is_empty() {
            cfg.shell = default_shell();
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
            log: RefCell::new(limits::LimitedLog::new(io::stderr(), 20, Duration::from_secs(60))),
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
                    // Out of descriptors and the like: back off, as Go's
                    // http.Server does, rather than spin.
                    self.log.borrow_mut().line(&format!("osewa server: accept: {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            lowat::tune(tcp.as_raw_fd());
            let (srv, tls, ws) = (self.clone(), tls.clone(), ws.clone());
            tokio::task::spawn_local(async move {
                let t = tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(tcp)).await;
                let stream = match t {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => return srv.log.borrow_mut().line(&format!("http: TLS handshake error from {peer}: {e}")),
                    Err(_) => return srv.log.borrow_mut().line(&format!("http: TLS handshake error from {peer}: timeout")),
                };
                // A refused upgrade has had its answer; nothing to log.
                let Ok(conn) = ws.accept(stream).await else { return };
                srv.serve_conn(Rc::new(WsConn(conn))).await;
            });
        }
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

    fn socket_path(&self, id: &str) -> PathBuf {
        osewa_holder::socket_path(&self.cfg.run_dir, id)
    }

    /// Connects to a session's holder and introduces client_id.
    async fn dial_holder(&self, id: &str, client_id: &str) -> io::Result<UnixStream> {
        let t = &self.cfg.tune;
        let mut conn = tokio::time::timeout(t.dial_timeout, UnixStream::connect(self.socket_path(id)))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial holder: i/o timeout"))??;
        let hello = wire::Hello { version: wire::VERSION, client_id: client_id.into(), ..Default::default() };
        let mut b = Vec::new();
        wire::json(wire::TYPE_HELLO, 0, &hello).encode_to(&mut b)?;
        tokio::time::timeout(t.holder_timeout, conn.write_all(&b))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write holder: i/o timeout"))??;
        Ok(conn)
    }

    /// Makes one request on a short-lived holder connection.
    async fn holder_call(&self, id: &str, client_id: &str, mut req: wire::Request) -> io::Result<wire::Response> {
        let mut conn = self.dial_holder(id, client_id).await?;
        req.client_id = client_id.into();
        let call = async {
            let mut b = Vec::new();
            wire::json(wire::TYPE_REQUEST, 0, &req).encode_to(&mut b)?;
            conn.write_all(&b).await?;
            let mut rd = BufReader::new(&mut conn);
            loop {
                let f = read_frame(&mut rd).await?;
                if f.ty != wire::TYPE_RESPONSE {
                    continue;
                }
                let resp: wire::Response = f.decode()?;
                if resp.id == req.id {
                    return Ok(resp);
                }
            }
        };
        tokio::time::timeout(self.cfg.tune.holder_timeout, call)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "holder: i/o timeout"))?
    }

    /// Unlinks a socket that refused a connection, but only if it is older
    /// than any holder still starting up and refuses again after a pause.
    /// Unlinking a live holder's socket would orphan it.
    async fn remove_if_stale(&self, path: &Path) {
        let t = &self.cfg.tune;
        let young = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|m| SystemTime::now().duration_since(m).unwrap_or_default() < t.stale_age)
            .unwrap_or(true);
        if young {
            return;
        }
        tokio::time::sleep(t.stale_retry).await;
        match tokio::time::timeout(t.dial_timeout, UnixStream::connect(path)).await {
            Ok(Err(e)) if refused(&e) => {
                let _ = std::fs::remove_file(path);
            }
            _ => {}
        }
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

fn default_shell() -> Vec<String> {
    match std::env::var("SHELL") {
        Ok(sh) if !sh.is_empty() => vec![sh, "-l".into()],
        _ => vec!["/bin/sh".into()],
    }
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
    if !cfg.cwd.is_empty() {
        a.push("--cwd".into());
        a.push(cfg.cwd.clone());
    }
    a.push("--".into());
    a.extend(cfg.argv.iter().cloned());
    a
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
async fn spawn_process(cfg: osewa_holder::Config, timeout: Duration) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(holder_binary().map_err(|e| e.to_string())?);
    cmd.args(holder_args(&cfg))
        // The holder CLI has no --env; the child gets the holder's own
        // environment instead.
        .envs(cfg.env.iter().filter_map(|kv| kv.split_once('=')))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("server: start holder: {e}"))?;
    await_ready(child, timeout).await
}

/// Waits for the started child to print "ready". Afterwards it keeps
/// draining stdout, so a later write can't kill the holder with SIGPIPE,
/// and reaps the process when it exits.
async fn await_ready(mut child: tokio::process::Child, timeout: Duration) -> Result<(), String> {
    let Some(out) = child.stdout.take() else { return Err("server: holder has no stdout".into()) };
    let mut br = BufReader::new(out);
    let mut line = String::new();
    let r = match tokio::time::timeout(timeout, br.read_line(&mut line)).await {
        Err(_) => Err("server: holder did not become ready".to_string()),
        Ok(_) if line.trim() == "ready" => Ok(()),
        Ok(Ok(_)) if line.is_empty() => Err("server: holder exited before ready: EOF".into()),
        Ok(Err(e)) => Err(format!("server: holder exited before ready: {e}")),
        Ok(Ok(_)) => Err(format!("server: holder said {:?}", line)),
    };
    if r.is_err() {
        let _ = child.start_kill();
    }
    tokio::task::spawn_local(async move {
        let _ = tokio::io::copy(&mut br, &mut tokio::io::sink()).await;
        let _ = child.wait().await;
    });
    r
}

#[cfg(test)]
mod tests;
