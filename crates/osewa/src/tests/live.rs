//! A real server for the live tests (Go's live_test.go liveHost): the Rust
//! server in this process, with holders on threads of their own, and a
//! client config directory enrolled on it as host "h".

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use osewa_client::hosts;

use super::*;

enum Ctl {
    Restart(mpsc::Sender<()>),
    /// Stops serving, holders kept, until Stop.
    Close(mpsc::Sender<()>),
    Stop,
}

pub(crate) struct LiveHost {
    pub addr: String,
    /// The client config directory: hosts.json with "h", client.key.
    pub dir: TempDir,
    ctl: tokio::sync::mpsc::UnboundedSender<Ctl>,
    thread: Option<JoinHandle<()>>,
    _host: TempDir,
}

type Holder = (tokio::sync::oneshot::Sender<()>, JoinHandle<Result<(), String>>);

/// Holders run in this process, each on its own thread.
fn inproc(holders: &Rc<RefCell<Vec<Holder>>>) -> osewa_server::Spawn {
    let holders = holders.clone();
    Rc::new(move |mut cfg: osewa_holder::Config| {
        cfg.exit_retention = Duration::from_millis(200);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let h = std::thread::spawn(move || {
            osewa_holder::run_until(
                cfg,
                move || {
                    let _ = ready_tx.send(());
                },
                false,
                Some(stop_rx),
            )
        });
        holders.borrow_mut().push((stop_tx, h));
        Box::pin(async move {
            match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
                Ok(Ok(())) => Ok(()),
                _ => Err("holder not ready".into()),
            }
        })
    })
}

impl LiveHost {
    pub fn new() -> LiveHost {
        let (host, dir) = (TempDir::new(), TempDir::new());
        let sid = osewa_auth::Identity::load_or_create(&host.path().join("server.key")).unwrap();
        let devices = osewa_auth::Devices::new(host.path().join("devices"));
        let cid = osewa_auth::Identity::load_or_create(&hosts::key_path(dir.path())).unwrap();
        devices.add(osewa_auth::Device { name: "test".into(), key: osewa_auth::encode_key(&cid.public()) }).unwrap();
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap().to_string();
        drop(ln);
        hosts::save(
            dir.path(),
            hosts::Host { name: "h".into(), addr: addr.clone(), server_key: osewa_auth::encode_key(&sid.public()), ..Default::default() },
        )
        .unwrap();
        let run_dir = host.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::set_permissions(&run_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let (ctl, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let a = addr.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            tokio::task::LocalSet::new().block_on(&rt, async move {
                let holders: Rc<RefCell<Vec<Holder>>> = Rc::default();
                let sid = Rc::new(sid);
                let mut ready = Some(ready_tx);
                loop {
                    let cfg = osewa_server::Config {
                        identity: Some(sid.clone()),
                        devices: Some(devices.clone()),
                        run_dir: run_dir.clone(),
                        name: "live".into(),
                        spawn: Some(inproc(&holders)),
                        shell: vec!["/bin/sh".into()],
                        ..Default::default()
                    };
                    let s = osewa_server::Server::new(cfg).unwrap();
                    let mut ln = None;
                    for _ in 0..100 {
                        if let Ok(l) = tokio::net::TcpListener::bind(&a).await {
                            ln = Some(l);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    let ln = ln.expect("listen");
                    // QUIC too, on the UDP port of the same number.
                    let quic = std::net::UdpSocket::bind(&a).ok().map(|sock| {
                        let s = s.clone();
                        tokio::task::spawn_local(async move {
                            let _ = s.serve_quic(sock).await;
                        })
                    });
                    if let Some(r) = ready.take() {
                        let _ = r.send(());
                    }
                    let serving = s.serve(ln);
                    tokio::pin!(serving);
                    let cmd = tokio::select! {
                        _ = &mut serving => None,
                        c = rx.recv() => c,
                    };
                    s.close().await;
                    if let Some(q) = quic {
                        let _ = q.await;
                    }
                    match cmd {
                        Some(Ctl::Restart(done)) => {
                            let _ = done.send(());
                        }
                        Some(Ctl::Close(done)) => {
                            let _ = done.send(());
                            while let Some(c) = rx.recv().await {
                                if matches!(c, Ctl::Stop) {
                                    break;
                                }
                            }
                            break;
                        }
                        _ => break,
                    }
                }
                for (stop, h) in holders.borrow_mut().drain(..) {
                    let _ = stop.send(());
                    let _ = h.join();
                }
            });
        });
        ready_rx.recv().unwrap();
        LiveHost { addr, dir, ctl, thread: Some(thread), _host: host }
    }

    /// Drops every client connection and serves again on the same address;
    /// holders survive.
    pub fn restart(&self) {
        let (tx, rx) = mpsc::channel();
        self.ctl.send(Ctl::Restart(tx)).unwrap();
        rx.recv().unwrap();
    }

    /// Stops serving, as Go's srv.Close: client connections drop, holders
    /// stay.
    pub fn stop_serving(&self) {
        let (tx, rx) = mpsc::channel();
        self.ctl.send(Ctl::Close(tx)).unwrap();
        rx.recv().unwrap();
    }

    pub fn env(&self, stdin: &str) -> (Env, Buf, Buf) {
        env(self.dir.path(), stdin)
    }

    pub fn run(&self, a: &[&str]) -> (i32, String, String) {
        run(self.dir.path(), a)
    }

    #[track_caller]
    pub fn run_ok(&self, a: &[&str]) -> String {
        let (code, out, err) = self.run(a);
        assert_eq!(code, 0, "{a:?}: {err}");
        out
    }
}

impl Drop for LiveHost {
    fn drop(&mut self) {
        let _ = self.ctl.send(Ctl::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn live_new_ls_kill() {
    let h = LiveHost::new();
    let id = h.run_ok(&["new", "h", "--", "/bin/sh"]).trim().to_string();
    assert_eq!(id.len(), 32, "id = {id:?}");
    let out = h.run_ok(&["ls", "h"]);
    assert!(out.contains(&id) && out.contains("running"), "ls:\n{out}");
    h.run_ok(&["kill", "h", &id]);
    let (code, _, err) = h.run(&["kill", "h", "0123"]);
    assert_eq!(code, 1, "kill unknown: {err}");
}

/// Login's check reaches a real server, and fails on a port nothing serves.
#[test]
fn login_checks_host() {
    let h = LiveHost::new();
    let mut rec = hosts::find(h.dir.path(), "h").unwrap();
    block(osewa_client::login::check_host(h.dir.path(), &rec, Duration::from_secs(5))).unwrap();
    rec.addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    assert!(block(osewa_client::login::check_host(h.dir.path(), &rec, Duration::from_secs(1))).is_err(), "reached a closed port");
}

/// The same commands over the QUIC carrier (docs/QUIC.md): the server serves
/// it on the UDP port of the TCP one's number, behind the same device
/// check.
#[test]
fn live_quic() {
    let h = LiveHost::new();
    let (mut e, out, err) = h.env("");
    e.carrier = Some("quic".into());
    let c = block(cli::run_client(&ctx(), "new", &args(&["h", "--", "/bin/sh"]), &e));
    assert_eq!(c, 0, "new over QUIC: {}", err.string());
    let id = out.string().trim().to_string();
    out.reset();
    assert_eq!(block(cli::run_client(&ctx(), "ls", &args(&["h"]), &e)), 0, "{}", err.string());
    assert!(out.string().contains(&id), "ls over QUIC:\n{}", out.string());

    // A device that isn't enrolled is refused, as over TCP.
    let rec = hosts::find(h.dir.path(), "h").unwrap();
    let stranger = osewa_auth::Identity::generate();
    let key = osewa_auth::decode_key(&rec.server_key).unwrap();
    let mut cfg = osewa_client::Config::default();
    cfg.url = format!("wss://{}/osewa", h.addr);
    cfg.tls = Some(Arc::new(osewa_auth::client_config(&stranger, key)));
    cfg.client_id = "x".into();
    cfg.carrier = "quic".into();
    let r = block(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let c = osewa_client::Conn::dial(cfg).await?;
            let w = c.welcome().await;
            c.close().await;
            w
        })
        .await
    });
    assert!(!matches!(r, Ok(Ok(_))), "an unenrolled device got a WELCOME over QUIC");
}

/// update copies this build to the host over osewa, checked, and asks it to
/// restart. The host here is this machine: HOME is a temporary one, and the
/// restart a no-op, or it would replace and restart the real server.
#[test]
fn live_update() {
    let home = TempDir::new();
    let mut g = EnvGuard::new();
    g.set("HOME", home.path());
    let h = LiveHost::new();
    let (mut e, out, err) = h.env("");
    e.update_restart = "echo restart-asked >> \"$HOME/restarted\"".into();
    let r = block(async { tokio::time::timeout(Duration::from_secs(60), e.update(&ctx(), &args(&["h"]))).await });
    assert!(matches!(r, Ok(Ok(()))), "update: {:?} {}", r.map(|r| r.map_err(|e| e.to_string())), err.string());
    let want = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let got = std::fs::read(home.path().join(".local/bin/osewa")).unwrap();
    assert!(got == want, "copied {} bytes, want this build's {}", got.len(), want.len());
    let asked = std::fs::read_to_string(home.path().join("restarted")).unwrap_or_default();
    assert!(asked.contains("restart-asked"), "no restart asked");
    assert!(out.string().contains("restarted with it"), "{}", out.string());
    drop(h);
}

/// A host's server advertises QUIC in WELCOME; the client remembers its
/// port in hosts.json, and the next connection races QUIC and the WebSocket
/// and lands on QUIC. With UDP dead it falls back to the WebSocket within
/// the race delay.
#[test]
fn live_quic_discovery() {
    let h = LiveHost::new();
    let dir = h.dir.path();
    let dial = || {
        let rec = hosts::find(dir, "h").unwrap();
        let mut cfg = rec.config(dir, "disco", "disco").unwrap();
        cfg.carrier = String::new(); // the default: discover and race
        let start = Instant::now();
        let c = block(async {
            let c = osewa_client::Conn::dial(cfg).await.unwrap();
            c.welcome().await.unwrap();
            c
        });
        (c, start.elapsed())
    };
    let (c, _) = dial();
    assert_eq!(c.carrier(), "ws", "first connection, before anything was advertised");
    block(c.close());
    eventually(|| hosts::find(dir, "h").unwrap().quic_port != 0);
    let (c, _) = dial();
    assert_eq!(c.carrier(), "quic", "second connection");
    block(c.close());

    // UDP dead (a port nothing serves): the WebSocket wins the race.
    let mut rec = hosts::find(dir, "h").unwrap();
    rec.quic_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() as i64;
    hosts::save(dir, rec).unwrap();
    let (c, d) = dial();
    assert!(c.carrier() == "ws" && d <= Duration::from_secs(2), "with UDP dead: {} after {d:?}", c.carrier());
    block(c.close());
}
