//! A server on the test's thread, real holders on their own threads (or
//! fake ones on the server's), and a client that speaks frames over an
//! in-memory connection, crediting what it reads as a real client does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use osewa_wire as wire;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

use crate::conn::mem::{self, Msg};
use crate::conn::{Conn, LocalBoxFuture};
use crate::{read_frame, Config, Server, Spawn};

pub const WAIT: Duration = Duration::from_secs(10);

/// Runs f on a current-thread runtime inside a LocalSet, as the server runs.
pub fn run<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, f)
}

/// A short temporary directory: Unix socket paths are capped near 104
/// bytes, and the default temporary directory on macOS is long.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = PathBuf::from(format!("/tmp/osw{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        TempDir(d)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn real(&self) -> String {
        std::fs::canonicalize(&self.0).unwrap().to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Real holders on their own threads, stopped when dropped.
type Holder = (tokio::sync::oneshot::Sender<()>, JoinHandle<Result<(), String>>);

#[derive(Default)]
pub struct Holders(RefCell<Vec<Holder>>);

impl Drop for Holders {
    fn drop(&mut self) {
        let hs = std::mem::take(&mut *self.0.borrow_mut());
        let joins: Vec<_> = hs
            .into_iter()
            .map(|(stop, h)| {
                let _ = stop.send(());
                h
            })
            .collect();
        for h in joins {
            let _ = h.join();
        }
    }
}

/// A Spawn that runs holders in this process, each on its own thread, with
/// a short exit retention.
pub fn inproc(holders: &Rc<Holders>) -> Spawn {
    let holders = holders.clone();
    Rc::new(move |mut cfg: osewa_holder::Config| {
        cfg.exit_retention = Duration::from_millis(200);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let h = std::thread::spawn(move || {
            let r = osewa_holder::run_until(
                cfg,
                move || {
                    let _ = ready_tx.send(());
                },
                false,
                Some(stop_rx),
            );
            if let Err(e) = &r {
                eprintln!("holder: {e}");
            }
            r
        });
        holders.0.borrow_mut().push((stop_tx, h));
        Box::pin(async move {
            match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err("holder exited before ready".into()),
                Err(_) => Err("holder not ready".into()),
            }
        })
    })
}

pub struct Env {
    // Dropped in this order: holders stop before the run dir goes.
    pub holders: Rc<Holders>,
    pub srv: Rc<Server>,
    pub run_dir: TempDir,
    pub server_key: osewa_auth::PublicKey,
    /// An enrolled device.
    pub device: Rc<osewa_auth::Identity>,
    /// Where serve listens, once listen has started it.
    pub addr: Option<std::net::SocketAddr>,
    /// Where serve_quic listens, once listen_quic has started it.
    pub quic_addr: Option<std::net::SocketAddr>,
}

impl Env {
    /// A server whose holders run in this process.
    pub fn new() -> Env {
        Env::with(|_| {})
    }

    /// new, with a config tweak.
    pub fn with(f: impl FnOnce(&mut Config)) -> Env {
        let holders = Rc::new(Holders::default());
        Env::with_spawn(Some(inproc(&holders)), holders, f)
    }

    pub fn with_spawn(spawn: Option<Spawn>, holders: Rc<Holders>, f: impl FnOnce(&mut Config)) -> Env {
        let run_dir = TempDir::new();
        let id = osewa_auth::Identity::generate();
        let server_key = id.public();
        let device = Rc::new(osewa_auth::Identity::generate());
        let devices = osewa_auth::Devices::new(run_dir.0.join("config/devices"));
        devices.add(osewa_auth::Device { name: "test".into(), key: osewa_auth::encode_key(&device.public()) }).unwrap();
        let mut cfg = Config {
            // Sessions and exec run in a sandbox home, never the real one.
            home: Some(osewa_paths::sandbox_home()),
            identity: Some(Rc::new(id)),
            devices: Some(devices),
            run_dir: run_dir.0.join("run"),
            name: "testsrv".into(),
            spawn,
            ..Default::default()
        };
        f(&mut cfg);
        let srv = Server::new(cfg).unwrap();
        Env { holders, srv, run_dir, server_key, device, addr: None, quic_addr: None }
    }

    /// Serves TLS and WebSocket on a loopback port.
    pub async fn listen(&mut self) {
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.addr = Some(ln.local_addr().unwrap());
        let srv = self.srv.clone();
        tokio::task::spawn_local(async move { srv.serve(ln).await.unwrap() });
    }

    /// Serves QUIC on a loopback port; listen's port when it has one.
    pub async fn listen_quic(&mut self) {
        let port = self.addr.map_or(0, |a| a.port());
        let sock = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
        self.quic_addr = Some(sock.local_addr().unwrap());
        let srv = self.srv.clone();
        tokio::task::spawn_local(async move { srv.serve_quic(sock).await.unwrap() });
        while self.srv.carriers().is_empty() {
            tokio::task::yield_now().await;
        }
    }

    /// Dials QUIC as id.
    pub async fn dial_quic(&self, id: &osewa_auth::Identity) -> std::io::Result<Tc> {
        let cfg = osewa_auth::client_config(id, self.server_key);
        let addr = self.quic_addr.unwrap().to_string();
        let dial = osewa_carrier::quic::dial(&addr, cfg, osewa_carrier::quic::Congestion::Cubic);
        let conn = tokio::time::timeout(WAIT, dial).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
        Ok(Tc::new(Transport::Quic(conn)))
    }

    /// dial_quic as the enrolled device, then HELLO/WELCOME.
    pub async fn connect_quic(&self, client_id: &str) -> Tc {
        let mut c = self.dial_quic(&self.device).await.unwrap();
        let hello = wire::Hello {
            client_id: client_id.into(),
            features: vec![wire::FEATURE_FORWARD.into(), wire::FEATURE_EXEC.into()],
            ..Default::default()
        };
        c.send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
        let f = c.next().await;
        assert_eq!(f.ty, wire::TYPE_WELCOME, "got 0x{:02x}, want WELCOME", f.ty);
        c.welcome = f.decode().unwrap();
        c
    }

    /// Opens TLS to the listening server as id, returning the stream and a
    /// count of the bytes the server wrote to it below TLS.
    pub async fn tls(&self, id: &osewa_auth::Identity) -> std::io::Result<(TlsStream, Rc<std::cell::Cell<u64>>)> {
        let tcp = tokio::net::TcpStream::connect(self.addr.unwrap()).await?;
        let (tcp, read) = Counting::new(tcp);
        let cfg = osewa_auth::client_config(id, self.server_key);
        let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cfg));
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap().to_owned();
        Ok((tls.connect(name, tcp).await?, read))
    }

    /// Dials over TLS and WebSocket, as an osewa client does.
    pub async fn dial_ws(&self, id: &osewa_auth::Identity) -> Result<Tc, String> {
        let (s, read) = self.tls(id).await.map_err(|e| e.to_string())?;
        let ws = osewa_carrier::ws::dial(s, "127.0.0.1").await.map_err(|e| e.to_string())?;
        ws.set_read_limit(1 << 30);
        let mut c = Tc::new(Transport::Ws(ws));
        c.link_read = Some(read);
        Ok(c)
    }

    /// dial_ws as the enrolled device, then HELLO/WELCOME.
    pub async fn connect_ws(&self, client_id: &str) -> Tc {
        let mut c = self.dial_ws(&self.device).await.unwrap();
        let hello = wire::Hello { client_id: client_id.into(), ..Default::default() };
        c.send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
        let f = c.next().await;
        assert_eq!(f.ty, wire::TYPE_WELCOME, "got 0x{:02x}, want WELCOME", f.ty);
        c.welcome = f.decode().unwrap();
        c
    }

    pub fn dir(&self) -> &Path {
        &self.srv.cfg.run_dir
    }

    /// The home sessions and exec run in (OSEWA_HOME, canonical).
    pub fn home(&self) -> String {
        osewa_paths::sandbox_home().display().to_string()
    }

    /// Connects without saying HELLO.
    pub fn dial(&self) -> Tc {
        let (end, peer) = mem::pair();
        let srv = self.srv.clone();
        tokio::task::spawn_local(async move { srv.serve_conn(end as Rc<dyn Conn>).await });
        Tc::new(Transport::Mem(peer))
    }

    /// Dials and completes HELLO/WELCOME, listing every feature.
    pub async fn connect(&self, client_id: &str) -> Tc {
        self.connect_with(client_id, &[wire::FEATURE_FORWARD, wire::FEATURE_EXEC]).await
    }

    pub async fn connect_with(&self, client_id: &str, features: &[&str]) -> Tc {
        let mut c = self.dial();
        let hello = wire::Hello {
            version: 0,
            client_id: client_id.into(),
            features: features.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        c.send(wire::json(wire::TYPE_HELLO, 0, &hello)).await;
        let f = c.next().await;
        assert_eq!(f.ty, wire::TYPE_WELCOME, "got 0x{:02x}, want WELCOME", f.ty);
        c.welcome = f.decode().unwrap();
        c
    }
}

static REQ_ID: AtomicU64 = AtomicU64::new(0);

pub type TlsStream = tokio_rustls::client::TlsStream<Counting<tokio::net::TcpStream>>;

/// How a test client reaches the server.
pub enum Transport {
    Mem(mem::Peer),
    Ws(osewa_carrier::ws::Conn<TlsStream>),
    Quic(osewa_carrier::quic::Conn),
}

/// A test client.
pub struct Tc {
    pub peer: Transport,
    /// Bytes the server wrote below TLS, for a client on the network.
    pub link_read: Option<Rc<std::cell::Cell<u64>>>,
    pub welcome: wire::Welcome,
    /// Stops read from crediting CHANNEL_DATA.
    pub hold_channel: bool,
    /// Credits CHANNEL_DATA only once 8 KiB is owed on a stream, the least
    /// PROTOCOL.md "Channel streams" allows (it also credits after 50 ms;
    /// never doing so is stricter). Fake clients that credit every frame
    /// hid stalls only a real client's batching shows.
    pub lazy_channel: bool,
    owed: HashMap<u32, usize>,
}

impl Tc {
    fn new(peer: Transport) -> Tc {
        Tc {
            peer,
            link_read: None,
            welcome: wire::Welcome::default(),
            hold_channel: false,
            lazy_channel: false,
            owed: HashMap::new(),
        }
    }

    /// Sets what the server sees of the path under an in-memory connection.
    pub fn set_path(&self, s: Option<crate::path::Sample>) {
        match &self.peer {
            Transport::Mem(p) => p.set_path(s),
            _ => panic!("set_path on a socket"),
        }
    }

    /// Drops the connection, as a client process exiting does.
    pub fn hang_up(&mut self) {
        match &mut self.peer {
            Transport::Mem(p) => p.close(),
            Transport::Ws(ws) => {
                let _ = ws.close_now();
            }
            Transport::Quic(q) => q.close_now(),
        }
    }
}

#[derive(Debug)]
pub enum ReadErr {
    Timeout,
    Closed,
    Bad(String),
}

impl Tc {
    pub async fn send(&self, f: wire::Frame) {
        self.try_send(f).await.expect("send");
    }

    pub async fn try_send(&self, f: wire::Frame) -> std::io::Result<()> {
        self.send_raw(f.marshal()).await
    }

    pub async fn send_raw(&self, b: Vec<u8>) -> std::io::Result<()> {
        let send = async {
            match &self.peer {
                Transport::Mem(p) => p.send(b).await,
                Transport::Ws(ws) => ws.write(&b).await.map_err(|e| std::io::Error::other(e.to_string())),
                Transport::Quic(q) => q.write(&b).await,
            }
        };
        match tokio::time::timeout(Duration::from_secs(5), send).await {
            Ok(r) => r,
            Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
        }
    }

    pub async fn read(&mut self, d: Duration) -> Result<wire::Frame, ReadErr> {
        let b = match &mut self.peer {
            Transport::Mem(p) => match tokio::time::timeout(d, p.recv()).await.map_err(|_| ReadErr::Timeout)? {
                Msg::Data(b) => b,
                Msg::Close(..) => return Err(ReadErr::Closed),
            },
            Transport::Ws(ws) => match tokio::time::timeout(d, ws.read()).await.map_err(|_| ReadErr::Timeout)? {
                Ok(b) => b,
                Err(_) => return Err(ReadErr::Closed),
            },
            Transport::Quic(q) => match tokio::time::timeout(d, q.read()).await.map_err(|_| ReadErr::Timeout)? {
                Ok(b) => b,
                Err(_) => return Err(ReadErr::Closed),
            },
        };
        let f = wire::Frame::unmarshal(&b).map_err(|e| ReadErr::Bad(e.to_string()))?;
        self.credit(&f).await;
        Ok(f)
    }

    /// Returns flow-control credit for every counted frame read, as a client
    /// must (docs/PROTOCOL.md, Flow control). A Tc that stops reading stops
    /// crediting, like a stalled client.
    async fn credit(&mut self, f: &wire::Frame) {
        match f.ty {
            // SNAPSHOT_DATA goes out uncounted.
            wire::TYPE_SNAPSHOT_HISTORY | wire::TYPE_OUTPUT | wire::TYPE_EVENT => {
                let _ = self.try_send(wire::credit(f.stream, f.payload.len() as u32)).await;
            }
            wire::TYPE_CHANNEL_DATA => {
                if self.hold_channel {
                    return;
                }
                let mut n = f.payload.len();
                if self.lazy_channel {
                    let owed = self.owed.entry(f.stream).or_default();
                    *owed += n;
                    if *owed < 8 << 10 {
                        return;
                    }
                    n = std::mem::take(owed);
                }
                let _ = self.try_send(wire::credit(f.stream, n as u32)).await;
            }
            _ => {}
        }
    }

    pub async fn next(&mut self) -> wire::Frame {
        match self.read(WAIT).await {
            Ok(f) => f,
            Err(e) => panic!("read: {e:?}"),
        }
    }

    /// Reads frames until m returns true, returning the matching one.
    pub async fn until(&mut self, mut m: impl FnMut(&wire::Frame) -> bool) -> wire::Frame {
        loop {
            let f = self.next().await;
            if m(&f) {
                return f;
            }
        }
    }

    /// Sends a request and waits for its response, skipping other frames.
    pub async fn call(&mut self, mut r: wire::Request) -> wire::Response {
        r.id = REQ_ID.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(wire::json(wire::TYPE_REQUEST, 0, &r)).await;
        let mut resp = wire::Response::default();
        self.until(|f| {
            if f.ty != wire::TYPE_RESPONSE {
                return false;
            }
            resp = f.decode().unwrap_or_default();
            resp.id == r.id
        })
        .await;
        resp
    }

    /// Reads OUTPUT for stream until the accumulated bytes contain want,
    /// returning the last end position.
    pub async fn output(&mut self, stream: u32, want: &str) -> u64 {
        let mut buf = Vec::new();
        let mut end = 0;
        self.until(|f| {
            if f.stream != stream || (f.ty != wire::TYPE_OUTPUT && f.ty != wire::TYPE_EVENT) {
                return false;
            }
            let (e, data) = wire::split_end(&f.payload).unwrap();
            end = e;
            if f.ty == wire::TYPE_OUTPUT {
                buf.extend_from_slice(data);
            }
            String::from_utf8_lossy(&buf).contains(want)
        })
        .await;
        end
    }

    /// output that also waits for an INPUT_ACK on stream, which may arrive
    /// before or after the echo.
    pub async fn output_ack(&mut self, stream: u32, want: &str) -> (u64, u64) {
        let mut buf = Vec::new();
        let (mut end, mut acked) = (0, 0);
        self.until(|f| {
            if f.stream != stream {
                return false;
            }
            match f.ty {
                wire::TYPE_INPUT_ACK => acked = wire::parse_u64(&f.payload).unwrap(),
                wire::TYPE_OUTPUT | wire::TYPE_EVENT => {
                    let (e, data) = wire::split_end(&f.payload).unwrap();
                    end = e;
                    if f.ty == wire::TYPE_OUTPUT {
                        buf.extend_from_slice(data);
                    }
                }
                _ => {}
            }
            acked > 0 && String::from_utf8_lossy(&buf).contains(want)
        })
        .await;
        (end, acked)
    }

    pub async fn input(&self, stream: u32, seq: u64, text: &str) {
        let input = wire::Input { seq, ty: "text".into(), text: text.into(), ..Default::default() };
        self.send(wire::json(wire::TYPE_INPUT, stream, &input)).await;
    }

    /// Reads until the server closes the connection.
    pub async fn closed(&mut self) -> bool {
        loop {
            match self.read(Duration::from_secs(5)).await {
                Ok(_) => {}
                Err(ReadErr::Closed) => return true,
                Err(e) => panic!("read: {e:?}"),
            }
        }
    }

    pub async fn create(&mut self, argv: &[&str]) -> String {
        let resp = self
            .call(wire::Request {
                method: wire::METHOD_CREATE.into(),
                cols: 80,
                rows: 24,
                argv: argv.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            })
            .await;
        assert!(resp.error.is_empty() && resp.session_id.len() == 32, "create: {resp:?}");
        resp.session_id
    }

    pub async fn attach(&mut self, session: &str, stream: u32, own: bool) -> wire::Response {
        self.call(wire::Request {
            method: wire::METHOD_ATTACH.into(),
            session: session.into(),
            stream,
            cols: 80,
            rows: 24,
            own,
            ..Default::default()
        })
        .await
    }

    pub async fn list(&mut self) -> wire::Response {
        self.call(req(wire::METHOD_LIST)).await
    }
}

pub fn req(method: &str) -> wire::Request {
    wire::Request { method: method.into(), ..Default::default() }
}

pub fn stream_req(method: &str, stream: u32) -> wire::Request {
    wire::Request { method: method.into(), stream, ..Default::default() }
}

pub fn decode_end(f: &wire::Frame) -> wire::StreamEnd {
    f.decode().unwrap()
}

/// A holder connection handler.
pub type Handler = Rc<dyn Fn(UnixStream) -> LocalBoxFuture<'static, ()>>;

/// Listens on a session socket and runs fn for each connection, until the
/// returned guard is dropped.
pub fn fake_holder(dir: &Path, id: &str, f: Handler) -> FakeHolder {
    let path = osewa_holder::socket_path(dir, id);
    let ln = UnixListener::bind(&path).unwrap();
    let task = tokio::task::spawn_local(async move {
        while let Ok((conn, _)) = ln.accept().await {
            tokio::task::spawn_local(f(conn));
        }
    });
    FakeHolder(task, path)
}

pub struct FakeHolder(tokio::task::JoinHandle<()>, PathBuf);

impl Drop for FakeHolder {
    fn drop(&mut self) {
        self.0.abort();
        let _ = std::fs::remove_file(&self.1);
    }
}

pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(UnixStream) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    Rc::new(move |c| Box::pin(f(c)))
}

pub async fn read_request(conn: &mut UnixStream) -> std::io::Result<wire::Request> {
    loop {
        let f = read_frame(conn).await?;
        if f.ty == wire::TYPE_REQUEST {
            return Ok(f.decode()?);
        }
    }
}

/// A session.info answer as a holder of this build gives it.
pub fn info_response(r: &wire::Request) -> wire::Response {
    let info = wire::SessionInfo { cols: 80, rows: 24, build: Some(crate::own_build()), ..Default::default() };
    wire::Response { id: r.id, sessions: vec![info], ..Default::default() }
}

/// Answers the server's build check as a holder of this build, and returns
/// the request after it: what a holder sees first on an attach.
pub async fn read_attach(conn: &mut UnixStream) -> std::io::Result<wire::Request> {
    let r = read_request(conn).await?;
    if r.method != wire::METHOD_INFO {
        return Ok(r);
    }
    write_frame(conn, wire::json(wire::TYPE_RESPONSE, 0, &info_response(&r))).await?;
    read_request(conn).await
}

pub async fn write_frame(conn: &mut UnixStream, f: wire::Frame) -> std::io::Result<()> {
    let mut b = Vec::new();
    f.encode_to(&mut b)?;
    conn.write_all(&b).await
}

pub fn response(id: u64) -> wire::Response {
    wire::Response { id, ..Default::default() }
}

/// Answers the attach and then never reads again, like a holder whose child
/// stopped reading stdin with 64 KiB of input pending.
pub fn stalled_holder(dir: &Path, id: &str) -> FakeHolder {
    fake_holder(
        dir,
        id,
        handler(|mut conn| async move {
            let Ok(r) = read_attach(&mut conn).await else { return };
            let resp = wire::Response { id: r.id, epoch: "e".into(), ..Default::default() };
            let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &resp)).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }),
    )
}

/// Answers the attach and echoes text input as OUTPUT.
pub fn echo_holder(dir: &Path, id: &str) -> FakeHolder {
    fake_holder(
        dir,
        id,
        handler(|mut conn| async move {
            let mut end = 0u64;
            loop {
                let Ok(f) = read_frame(&mut conn).await else { return };
                match f.ty {
                    wire::TYPE_REQUEST => {
                        let r: wire::Request = f.decode().unwrap();
                        if r.method == wire::METHOD_INFO {
                            let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &info_response(&r))).await;
                            continue;
                        }
                        if r.method == wire::METHOD_DETACH {
                            let se = wire::StreamEnd { reason: wire::END_DETACHED.into(), exit_code: None };
                            let _ = write_frame(&mut conn, wire::json(wire::TYPE_STREAM_END, r.stream, &se)).await;
                        }
                        let resp = wire::Response { id: r.id, epoch: "e".into(), ..Default::default() };
                        let _ = write_frame(&mut conn, wire::json(wire::TYPE_RESPONSE, 0, &resp)).await;
                    }
                    wire::TYPE_INPUT => {
                        let input: wire::Input = f.decode().unwrap();
                        end += wire::record_size(input.text.len());
                        let _ = write_frame(&mut conn, wire::output(f.stream, end, input.text.as_bytes())).await;
                    }
                    _ => {}
                }
            }
        }),
    )
}

/// Creates a socket file with nothing listening yet, as during a holder's
/// bind-to-listen window. The socket closes when the guard drops.
pub fn bind_only(path: &Path) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(fd >= 0);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        let p = path.as_os_str().as_encoded_bytes();
        for (i, b) in p.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let r = libc::bind(fd, (&addr as *const libc::sockaddr_un).cast(), std::mem::size_of::<libc::sockaddr_un>() as _);
        assert_eq!(r, 0, "bind: {}", std::io::Error::last_os_error());
        std::os::fd::OwnedFd::from_raw_fd(fd)
    }
}

/// Sets a file's modification time an hour back.
pub fn age(path: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    let f = std::fs::File::options().write(true).open(path).or_else(|_| std::fs::File::open(path));
    match f {
        Ok(f) => f.set_modified(old).unwrap(),
        Err(_) => {
            // A socket can't be opened: set the time through its path.
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            let t = libc::timeval { tv_sec: old.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as _, tv_usec: 0 };
            assert_eq!(unsafe { libc::utimes(c.as_ptr(), [t, t].as_ptr()) }, 0);
        }
    }
}

pub fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Polls path for a pid written by a test's script.
pub async fn read_pid(path: &Path) -> i32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(p) = s.trim().parse() {
                return p;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no pid in {}", path.display());
}

/// Listens on loopback TCP, runs f for each connection, and returns the
/// port with a guard that stops listening.
pub async fn listen<F, Fut>(f: F) -> (u16, tokio::task::JoinHandle<()>)
where
    F: Fn(tokio::net::TcpStream) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ln.local_addr().unwrap().port();
    let task = tokio::task::spawn_local(async move {
        while let Ok((c, _)) = ln.accept().await {
            tokio::task::spawn_local(f(c));
        }
    });
    (port, task)
}

/// Counts the bytes read through it.
pub struct Counting<S> {
    inner: S,
    read: Rc<std::cell::Cell<u64>>,
}

impl<S> Counting<S> {
    pub fn new(inner: S) -> (Counting<S>, Rc<std::cell::Cell<u64>>) {
        let read = Rc::new(std::cell::Cell::new(0));
        (Counting { inner, read: read.clone() }, read)
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Counting<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        self.read.set(self.read.get() + n as u64);
        r
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Counting<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A config with a fresh identity and an empty devices file in dir.
pub fn auth_config(dir: &Path) -> Config {
    Config {
        identity: Some(Rc::new(osewa_auth::Identity::generate())),
        devices: Some(osewa_auth::Devices::new(dir.join("devices"))),
        home: Some(osewa_paths::sandbox_home()),
        ..Default::default()
    }
}
