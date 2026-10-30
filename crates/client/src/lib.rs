//! osewa's client library: speaks the protocol (docs/PROTOCOL.md) over a
//! WebSocket (QUIC once the carrier lands): it lists, creates and kills
//! sessions, attaches to them with resume across reconnects, forwards TCP
//! connections and runs commands on the host. It never links libghostty;
//! `osewa-mirror` adds a libghostty terminal as a [`Renderer`]. [`hosts`]
//! is the device's hosts.json and [`login`] enrolls a device over ssh.
//!
//! # Threading
//!
//! Every handle ([`Conn`], [`Session`], [`Link`], channel streams) is
//! `Send + Sync` and cheap to clone, and every `async` method returns a
//! `Send` future that awaits only cross-thread primitives, so any executor
//! can drive it: a tokio runtime, or [`block_on`] for a blocking caller
//! such as an FFI. Cancelling is dropping the future; timeouts are the
//! caller's (`tokio::time::timeout`).
//!
//! The network runs on one background thread per process, a current-thread
//! tokio runtime started on first use: carriers, keepalive, the writers,
//! credit and ack timers and [`Link`]'s reconnect loop. Each connection
//! also has an applier thread that applies received frames in order and
//! runs the [`Renderer`] and [`AttachOptions`] callbacks, so a slow render
//! never delays reading, pongs or credit, as the Go client's applier
//! goroutine does. A process that never connects starts no thread; an idle
//! connection wakes only for its keepalive ping.
//!
//! Why not the server's shape (a LocalSet the caller drives): this library
//! is meant to be hosted by an FFI (iOS, Android, app-hook), whose callers
//! are foreign threads that can't drive a Rust runtime, and the WebSocket
//! carrier is `!Send`, so it needs a thread of its own either way.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use osewa_wire as wire;

mod channel;
mod conn;
mod dial;
mod link;
mod rt;
mod session;
mod trace;

pub mod hosts;
pub mod install;
pub mod login;

#[cfg(test)]
mod tests;

pub use channel::{Exec, ExecOptions, ExecStderr, ExecStdin, ExecStdout, Forward, Output};
pub use conn::{Conn, CreateOptions};
pub use link::Link;
pub use rt::{block_on, sleep, timeout};
pub use session::{AttachOptions, InputMode, Renderer, RendererError, Session};

/// The libghostty commit whose snapshots this build decodes, as in
/// `osewa_ghostty::COMMIT` and scripts/libghostty.sh. Kept here so the
/// transport needn't link libghostty; a test checks they agree.
pub const GHOSTTY_COMMIT: &str = "83fb95624b0439ee660c2679b43b38c8b63769cd";
/// The snapshot format version, as `osewa_ghostty::SNAPSHOT_VERSION`.
pub const SNAPSHOT_VERSION: i32 = 1;

/// Hears each WELCOME's carriers, to remember them (hosts.json keeps the
/// QUIC port).
pub type OnCarriers = Arc<dyn Fn(&[wire::Carrier]) + Send + Sync>;

/// Describes one connection.
#[derive(Clone, Default)]
pub struct Config {
    /// wss://host:port/osewa
    pub url: String,
    /// How to connect: "ws" the WebSocket; "quic" QUIC only, at
    /// `quic_port`, else the URL's port; empty (the default) races QUIC and
    /// the WebSocket when `quic_port` is known, else the WebSocket.
    /// OSEWA_CARRIER sets it when empty.
    pub carrier: String,
    /// The UDP port the host advertised QUIC on (WELCOME carriers), as
    /// remembered from an earlier connection; 0 for none.
    pub quic_port: u16,
    pub on_carriers: Option<OnCarriers>,
    /// The pinned mutual TLS config, `osewa_auth::client_config`. Required:
    /// without a pin, keystrokes would go to whoever presents a
    /// certificate.
    pub tls: Option<Arc<rustls::ClientConfig>>,
    /// Identifies this device and app instance to holders, which key input
    /// seq and ownership by it.
    pub client_id: String,
    pub name: String,
    /// How long a pending TCP connect waits before a second attempt races
    /// it. None is the default, 250 ms; zero never races.
    pub race_after: Option<Duration>,

    pub(crate) knobs: Knobs,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("url", &self.url)
            .field("carrier", &self.carrier)
            .field("quic_port", &self.quic_port)
            .field("client_id", &self.client_id)
            .field("name", &self.name)
            .field("race_after", &self.race_after)
            .finish_non_exhaustive()
    }
}

/// What tests change: Go's package variables.
#[derive(Clone, Default)]
pub(crate) struct Knobs {
    /// The keepalive interval (default 5 s).
    pub ping: Option<Duration>,
    /// Bounds bytes received but not yet applied (default 16 MiB).
    pub apply_max: Option<usize>,
    /// Replaces the TCP connect.
    pub dial: Option<dial::TcpDial>,
    /// Runs between taking a response off pending and running its hook.
    pub response_taken: Option<ResponseTaken>,
    /// Traces into this directory, whatever OSEWA_TRACE says (tests run in
    /// parallel in one process).
    pub trace_dir: Option<std::path::PathBuf>,
}

pub(crate) type ResponseTaken = Arc<dyn Fn(&Conn, &wire::Response) + Send + Sync>;

/// Why a call failed. Errors are values: cheap to clone, so one
/// connection's error can be every waiter's.
#[derive(Debug, Clone)]
pub enum Error {
    /// A RESPONSE error from the server.
    Response { code: String, message: String },
    /// The connection is closed or lost, with why when known.
    Closed(Option<String>),
    /// The server's snapshots can't be decoded by this build: snapshot
    /// format 1 is only stable within one libghostty commit. Retrying
    /// can't help.
    Incompatible { server_commit: String, client_commit: String, server_snapshot: i32, client_snapshot: i32 },
    /// The session can no longer be resumed.
    Ended(Option<String>),
    /// The Link was closed.
    LinkClosed,
    Io(Arc<io::Error>),
    /// Several errors, each attempt's (Go's errors.Join).
    Join(Vec<Error>),
    Other(String),
}

impl Error {
    pub(crate) fn closed(why: impl fmt::Display) -> Error {
        Error::Closed(Some(why.to_string()))
    }

    /// Reports whether this is a RESPONSE error with this code.
    pub fn is_code(&self, code: &str) -> bool {
        matches!(self, Error::Response { code: c, .. } if c == code)
    }

    /// The RESPONSE error code, if this is one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Response { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Error::Closed(_))
    }

    pub fn is_incompatible(&self) -> bool {
        self.any(&|e| matches!(e, Error::Incompatible { .. }))
    }

    pub fn is_ended(&self) -> bool {
        matches!(self, Error::Ended(_))
    }

    /// The kind of an I/O error behind this one, looking into joined
    /// errors: the first that has one.
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Error::Io(e) => Some(e.kind()),
            Error::Join(v) => v.iter().find_map(Error::io_kind),
            _ => None,
        }
    }

    /// Reports whether this error, or one joined in it, matches.
    pub fn any(&self, f: &dyn Fn(&Error) -> bool) -> bool {
        f(self) || matches!(self, Error::Join(v) if v.iter().any(|e| e.any(f)))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Response { code, message } if message.is_empty() => write!(f, "osewa: {code}"),
            Error::Response { code, message } => write!(f, "osewa: {code}: {message}"),
            Error::Closed(None) => f.write_str("osewa: connection closed"),
            Error::Closed(Some(why)) => write!(f, "osewa: connection closed: {why}"),
            Error::Incompatible { server_commit, client_commit, server_snapshot, client_snapshot } => write!(
                f,
                "osewa: incompatible libghostty: server {server_commit} (snapshot v{server_snapshot}), client {client_commit} (snapshot v{client_snapshot})"
            ),
            Error::Ended(None) => f.write_str("osewa: session ended"),
            Error::Ended(Some(why)) => write!(f, "osewa: session ended: {why}"),
            Error::LinkClosed => f.write_str("osewa: link closed"),
            Error::Io(e) => e.fmt(f),
            Error::Join(v) => {
                for (i, e) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    e.fmt(f)?;
                }
                Ok(())
            }
            Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(Arc::new(e))
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => io::Error::new(e.kind(), e.to_string()),
            Error::Closed(_) => io::Error::new(io::ErrorKind::ConnectionAborted, e),
            e => io::Error::other(e),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
