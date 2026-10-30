//! `osewa server`.

use std::rc::Rc;

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

fn parse(args: &[String]) -> Result<(String, Config), String> {
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
async fn serve(listen: String, cfg: Config, stop: impl std::future::Future<Output = ()>) -> Result<(), String> {
    let s: Rc<Server> = Server::new(cfg)?;
    let _ = (listen, stop);
    s.close().await;
    Err("TLS is not wired into the Rust server yet".into())
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
