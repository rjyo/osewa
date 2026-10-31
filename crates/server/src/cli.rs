//! `osewa server`.

use std::future::Future;
use std::rc::Rc;

use tokio::net::TcpListener;

use crate::{Config, Server};

/// The port `osewa server` listens on, TCP and UDP.
pub const DEFAULT_PORT: u16 = 7322;

/// Runs `osewa server [--listen addr] [--name name] [--no-compress]
/// [--no-forward] [--no-exec] [--no-quic]`; args are the arguments after
/// "server". Returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    let (listen, mut cfg) = match parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    cfg.trace_dir = std::env::var("OSEWA_TRACE").unwrap_or_default();
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("osewa server: {e}");
            return 1;
        }
    };
    let local = tokio::task::LocalSet::new();
    match local.block_on(&rt, serve(listen, cfg, stop_signal())) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("osewa server: {e}");
            1
        }
    }
}

/// Resolves on SIGINT or SIGTERM.
async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Reports whether args are valid `osewa server` flags, for commands that
/// start the server later, such as `osewa service install`.
pub fn check_args(args: &[String]) -> Result<(), String> {
    parse(args).map(|_| ())
}

pub(crate) fn parse(args: &[String]) -> Result<(String, Config), String> {
    let mut fs = osewa_flag::FlagSet::new("server");
    fs.value("listen", &format!(":{DEFAULT_PORT}"), "address to listen on")
        .value("name", "", "server name reported to clients (default hostname)")
        .bool("no-compress", false, "don't compress what is sent to clients")
        .bool("no-forward", false, "refuse forward.open (TCP forwarding)")
        .bool("no-exec", false, "refuse exec.start (commands without a terminal)")
        .bool("no-quic", false, "don't serve the QUIC carrier on UDP");
    let p = fs.parse(args).map_err(|e| format!("{e}\n{}", fs.usage()))?;
    if !p.args.is_empty() {
        return Err(format!("unexpected arguments {:?}", p.args));
    }
    let cfg = Config {
        name: p.str("name"),
        no_compress: p.bool("no-compress"),
        no_forward: p.bool("no-forward"),
        no_exec: p.bool("no-exec"),
        no_quic: p.bool("no-quic"),
        ..Default::default()
    };
    Ok((p.str("listen"), cfg))
}

/// Runs a server with the user's identity, devices and run directory until
/// stop resolves.
pub(crate) async fn serve(listen: String, mut cfg: Config, stop: impl Future<Output = ()>) -> Result<(), String> {
    let id = osewa_auth::Identity::load_or_create(&osewa_auth::identity_path()).map_err(|e| e.to_string())?;
    cfg.identity = Some(Rc::new(id));
    cfg.devices = Some(osewa_auth::Devices::new(osewa_auth::devices_path()));
    cfg.run_dir = osewa_paths::run_dir();
    let s: Rc<Server> = Server::new(cfg)?;
    // Worded as v0 servers word it ("listen tcp ..."), which `osewa server
    // --daemon` relays.
    let ln = bind(&listen).await.map_err(|e| {
        let op = if e.raw_os_error().is_some() { "bind: " } else { "" };
        format!("listen tcp {listen}: {op}{}", osewa_execproc::error_text(&e))
    })?;
    let mut quic = None;
    if !s.config().no_quic {
        // QUIC on the UDP port of the same number (docs/QUIC.md). A port
        // UDP can't have only loses the carrier.
        match ln.local_addr().and_then(std::net::UdpSocket::bind) {
            Err(e) => eprintln!("osewa server: no QUIC: listen udp {listen}: {e}"),
            Ok(sock) => {
                let s = s.clone();
                quic = Some(tokio::task::spawn_local(async move {
                    if let Err(e) = s.serve_quic(sock).await {
                        eprintln!("osewa server: no QUIC: {e}");
                    }
                }));
            }
        }
    }
    let r = serve_until(&s, ln, stop).await;
    // QUIC's clients hear of the close only once its endpoint sends it.
    if let Some(q) = quic {
        let _ = q.await;
    }
    r.map_err(|e| e.to_string())
}

/// Serves ln until stop resolves, then closes the server.
pub(crate) async fn serve_until(s: &Rc<Server>, ln: TcpListener, stop: impl Future<Output = ()>) -> std::io::Result<()> {
    let serving = s.serve(ln);
    tokio::pin!(serving);
    tokio::select! {
        r = &mut serving => {
            s.close().await;
            return r;
        }
        _ = stop => {}
    }
    // close marks the server closed before it waits for exec processes
    // (whose SIGKILL needs us alive); serving, polled beside it, drops the
    // listener at once, so new connections are refused meanwhile.
    let (r, ()) = tokio::join!(serving, s.close());
    r
}

/// Listens on addr: an empty host is every
/// address, IPv6 and IPv4 where the system has both.
async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    match addr.strip_prefix(':') {
        Some(port) => match TcpListener::bind(format!("[::]:{port}")).await {
            Ok(ln) => Ok(ln),
            Err(_) => TcpListener::bind(format!("0.0.0.0:{port}")).await,
        },
        None => TcpListener::bind(addr).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_flags() {
        let (listen, cfg) = parse(&args(&["--listen", ":1", "--no-forward", "--no-exec", "--no-compress"])).unwrap();
        assert!(listen == ":1" && cfg.no_forward && cfg.no_exec && cfg.no_compress && !cfg.no_quic);
        assert!(check_args(&args(&["--no-exec"])).is_ok());
        assert!(check_args(&args(&["--bogus"])).is_err());
        assert!(check_args(&args(&["extra"])).is_err());
        let (listen, cfg) = parse(&[]).unwrap();
        assert!(listen == ":7322" && cfg.name.is_empty());
    }
}
