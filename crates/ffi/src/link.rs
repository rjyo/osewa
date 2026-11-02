//! One open link (osewa_link): a connection to a server kept up as a
//! session's is, without a session of its own. It carries forwards, local
//! loopback ports whose connections the server opens onward as `ssh -L`
//! does, and the sessions opened on it (osewa_open_on), so an app reaches
//! one host over one connection.
//!
//! A thread follows the link's state for the callbacks; each forward has a
//! thread accepting on its port, and each connection two pumping bytes.
//! Forwarded connections end with the connection they were opened on (the
//! server's side is a plain TCP connection, which can't be resumed): a
//! client sees them close, and its next connection goes over the new one.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use osewa_client::{Error, Forward, Link};
use osewa_wire as wire;
use tokio::sync::oneshot;

use crate::session::{fatal, Callbacks, Sink, CLOSED, CONNECTED, CONNECTING, RECONNECTING};

/// Bounds getting a connection and the server's dial for one forwarded
/// connection, as `osewa forward` does.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(15);
/// Between attempts at a connection that isn't up yet, within FORWARD_TIMEOUT.
const RETRY_AFTER: Duration = Duration::from_millis(250);

/// An open link (osewa_link).
pub struct LinkHandle {
    sink: Arc<Sink>,
    link: Link,
    /// Sessions opened on the link name themselves by it.
    client_id: String,
    stop: Option<oneshot::Sender<()>>,
    watcher: Option<JoinHandle<()>>,
    forwards: Mutex<HashMap<u16, Listener>>,
}

impl LinkHandle {
    /// Starts connecting in the background.
    pub fn open(mut cfg: osewa_client::Config, cb: Callbacks) -> LinkHandle {
        let sink = Arc::new(Sink::new(cb));
        let client_id = cfg.client_id.clone();
        let csink = sink.clone();
        let known = cfg.quic_port;
        cfg.on_carriers = Some(Arc::new(move |cs: &[wire::Carrier]| {
            if let Some(q) = cs.iter().find(|c| c.carrier == "quic" && c.port > 0) {
                if q.port != i64::from(known) {
                    csink.event(&serde_json::json!({"type": "carriers", "quic_port": q.port}).to_string());
                }
            }
        }));
        let link = Link::new(cfg);
        let (stop, stop_rx) = oneshot::channel();
        let (l, s) = (link.clone(), sink.clone());
        let watcher = std::thread::Builder::new()
            .name("osewa-link".into())
            .spawn(move || {
                osewa_client::block_on(async move {
                    tokio::select! {
                        _ = stop_rx => {}
                        _ = follow(&l, &s) => {}
                    }
                })
            })
            .expect("osewa: link thread");
        LinkHandle { sink, link, client_id, stop: Some(stop), watcher: Some(watcher), forwards: Mutex::default() }
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Listens on 127.0.0.1:local_port (0 picks one) and forwards each
    /// connection to host:port as the server dials it. Returns the port.
    pub fn forward(&self, local_port: u16, host: String, port: u16) -> Result<u16, String> {
        if host.is_empty() || port == 0 {
            return Err("osewa: forward needs a host and a port".into());
        }
        let ln = TcpListener::bind(("127.0.0.1", local_port)).map_err(|e| format!("osewa: listen 127.0.0.1:{local_port}: {e}"))?;
        let addr = ln.local_addr().map_err(|e| format!("osewa: listen: {e}"))?;
        let listener = Listener::start(ln, addr, self.link.clone(), host, port);
        let old = self.forwards.lock().unwrap_or_else(|e| e.into_inner()).insert(addr.port(), listener);
        if let Some(old) = old {
            old.stop();
        }
        Ok(addr.port())
    }

    /// Stops the forward on local_port and ends its connections.
    pub fn unforward(&self, local_port: u16) {
        let l = self.forwards.lock().unwrap_or_else(|e| e.into_inner()).remove(&local_port);
        if let Some(l) = l {
            l.stop();
        }
    }

    /// Stops the callbacks, waiting out one that is running.
    pub fn quiesce(&self) {
        self.sink.off();
    }

    /// Stops following, ends every forward, and closes the link in the
    /// background. Sessions opened on it end with it. Call quiesce first.
    pub fn close(mut self) {
        self.sink.off();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.watcher.take() {
            let _ = t.join();
        }
        let forwards = std::mem::take(&mut *self.forwards.lock().unwrap_or_else(|e| e.into_inner()));
        for (_, l) in forwards {
            l.stop();
        }
        let link = self.link.clone();
        // The network may be gone: the app's thread must not wait on it.
        std::thread::spawn(move || {
            osewa_client::block_on(async {
                let _ = osewa_client::timeout(Duration::from_secs(2), link.close()).await;
            })
        });
    }
}

/// Reports the link's state as it changes: CONNECTING until a first
/// connection is up, then CONNECTED and RECONNECTING, and CLOSED once
/// nothing will get through (incompatible, unauthorized). The carrier comes
/// with stats whenever it changes, srtt_us 0: the link measures none of
/// its own.
async fn follow(link: &Link, sink: &Sink) {
    let mut w = link.watch();
    let mut carrier: Option<&'static str> = None;
    sink.state(CONNECTING, None);
    loop {
        let st = link.state();
        let why = st.fin.as_ref().map(|e| fatal(e).unwrap_or_else(|| e.to_string())).or_else(|| st.err.as_ref().and_then(fatal));
        if let Some(why) = why {
            sink.state(CLOSED, Some(&why));
            link.close().await;
            return;
        }
        if st.up {
            sink.state(CONNECTED, None);
        } else if st.paused {
            // Asked for, not lost: the app keeps the state it had.
        } else if st.connected {
            sink.state(RECONNECTING, st.err.as_ref().map(|e| e.to_string()).as_deref());
        } else {
            sink.state(CONNECTING, st.err.as_ref().map(|e| e.to_string()).as_deref());
        }
        if st.carrier.is_some() && st.carrier != carrier {
            carrier = st.carrier;
            sink.stats(Duration::ZERO, carrier.unwrap_or_default());
        }
        if !w.changed().await {
            return;
        }
    }
}

/// One forward's listening port and the connections it accepted.
struct Listener {
    stopped: Arc<AtomicBool>,
    addr: SocketAddr,
    conns: Arc<Conns>,
    thread: Option<JoinHandle<()>>,
}

/// The open connections of a forward, to end them when it stops.
#[derive(Default)]
struct Conns {
    next: AtomicUsize,
    open: Mutex<HashMap<usize, (TcpStream, Option<Forward>)>>,
}

impl Listener {
    fn start(ln: TcpListener, addr: SocketAddr, link: Link, host: String, port: u16) -> Listener {
        let stopped = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(Conns::default());
        let (st, cs) = (stopped.clone(), conns.clone());
        let thread = std::thread::Builder::new()
            .name("osewa-forward".into())
            .spawn(move || {
                for local in ln.incoming() {
                    if st.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(local) = local else { continue };
                    let _ = local.set_nodelay(true);
                    let (link, host, cs) = (link.clone(), host.clone(), cs.clone());
                    let _ = std::thread::Builder::new().name("osewa-forward-conn".into()).spawn(move || pipe(&link, local, &host, port, &cs));
                }
            })
            .expect("osewa: forward thread");
        Listener { stopped, addr, conns, thread: Some(thread) }
    }

    /// Stops accepting and ends the open connections.
    fn stop(mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Wakes the blocked accept, which then sees stopped.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let open = std::mem::take(&mut *self.conns.open.lock().unwrap_or_else(|e| e.into_inner()));
        for (_, (local, remote)) in open {
            let _ = local.shutdown(Shutdown::Both);
            if let Some(r) = remote {
                r.close();
            }
        }
    }
}

/// Opens the server's side of one forwarded connection: on the current
/// connection, waiting a little for one while the link (re)connects.
async fn dial(link: &Link, host: &str, port: u16) -> Result<Forward, String> {
    let attempt = async {
        loop {
            match link.conn().await {
                Ok(c) => return c.dial_forward(host, port).await.map_err(|e| e.to_string()),
                Err(e) if matches!(e, Error::LinkClosed) || fatal(&e).is_some() => return Err(e.to_string()),
                // Not up yet: the link keeps dialing.
                Err(_) => osewa_client::sleep(RETRY_AFTER).await,
            }
        }
    };
    osewa_client::timeout(FORWARD_TIMEOUT, attempt).await.unwrap_or_else(|| Err("osewa: timed out".into()))
}

/// Copies between local and a new forward until both sides are done,
/// passing half-closes along. A forward that can't be opened closes local.
fn pipe(link: &Link, local: TcpStream, host: &str, port: u16, conns: &Conns) {
    let key = conns.next.fetch_add(1, Ordering::SeqCst);
    let lock = || conns.open.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(tracked) = local.try_clone() else { return };
    lock().insert(key, (tracked, None));
    let remote = osewa_client::block_on(dial(link, host, port));
    let remote = match remote {
        Ok(r) => r,
        Err(_) => {
            lock().remove(&key);
            return;
        }
    };
    match lock().get_mut(&key) {
        Some(e) => e.1 = Some(remote.clone()),
        // The forward stopped meanwhile.
        None => {
            remote.close();
            return;
        }
    }
    let up = {
        let (mut lr, rw) = (local.try_clone(), remote.clone());
        std::thread::spawn(move || {
            if let Ok(lr) = lr.as_mut() {
                let mut buf = vec![0u8; 32 << 10];
                while let Ok(n) = lr.read(&mut buf) {
                    if n == 0 || osewa_client::block_on(rw.write_all(&buf[..n])).is_err() {
                        break;
                    }
                }
            }
            let _ = rw.close_write();
        })
    };
    let mut lw = local;
    let mut buf = vec![0u8; 32 << 10];
    loop {
        match osewa_client::block_on(remote.read(&mut buf)) {
            Ok(n) if n > 0 => {
                if lw.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = lw.shutdown(Shutdown::Write);
    let _ = up.join();
    remote.close();
    lock().remove(&key);
}
