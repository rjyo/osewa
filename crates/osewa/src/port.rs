//! The port of this user's server on a host where other users run theirs.
//! Each user has a server, and only the first gets the default port: the
//! next finds it taken. enroll, which login runs before starting the
//! server, picks a free port then and keeps it in osewa_paths::port_path(),
//! which the server and enroll's answer go by from then on. A user whose
//! server has the default port (every host before this) gets no file. Only
//! a running server is seen: one that is down can lose its port to the
//! next user who logs in.

use std::net::{TcpListener, UdpSocket};

/// Tried in order when the port is taken.
const SPARE: std::ops::RangeInclusive<u16> = 7323..=7422;

/// enroll's arguments, once this user's server has a port: an explicit
/// --port is left to enroll.
pub(crate) fn enroll_args(args: &[String]) -> Vec<String> {
    let given = args.iter().any(|a| {
        let a = a.trim_start_matches('-');
        a == "port" || a.starts_with("port=")
    });
    if given {
        return args.to_vec();
    }
    // The saved port too: someone may have taken it while our server was
    // down.
    let port = osewa_paths::server_port();
    if let Some(p) = choose(port, free, ours) {
        match save(p) {
            Ok(()) => eprintln!("osewa enroll: port {port} is in use; this user's server takes {p}"),
            Err(e) => eprintln!("osewa enroll: port {port} is in use, and {} can't be written: {e}", osewa_paths::port_path().display()),
        }
    }
    args.to_vec()
}

/// The port to save, if port won't do: None while it is free or held by
/// this user's own server, and None when no spare is free either (the
/// server then says the port is in use, as before).
pub(crate) fn choose(port: u16, free: impl Fn(u16) -> bool, ours: impl Fn(u16) -> bool) -> Option<u16> {
    if free(port) || ours(port) {
        return None;
    }
    SPARE.into_iter().find(|&p| free(p))
}

/// The server takes the port on TCP and, for QUIC, UDP.
fn free(p: u16) -> bool {
    TcpListener::bind(("0.0.0.0", p)).is_ok() && UdpSocket::bind(("0.0.0.0", p)).is_ok()
}

/// A running server of this user's never moves: it may listen on one
/// address only, where 127.0.0.1 doesn't reach it and the 0.0.0.0 bind
/// test fails.
fn ours(p: u16) -> bool {
    crate::daemon::running_server().is_some() || crate::daemon::our_server_at(&format!("127.0.0.1:{p}"))
}

fn save(p: u16) -> std::io::Result<()> {
    osewa_paths::ensure(&osewa_paths::config_dir())?;
    std::fs::write(osewa_paths::port_path(), format!("{p}\n"))
}
