//! Connections (Go's client/client.go).

use osewa_wire as wire;

use crate::{Config, Result};

/// Describes a new session.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub cols: i64,
    pub rows: i64,
    /// Empty runs the user's login shell.
    pub argv: Vec<String>,
    /// Empty is the home directory.
    pub cwd: String,
    /// KEY=VALUE, added to the server's environment.
    pub env: Vec<String>,
}

/// One connection to a server.
#[derive(Clone)]
pub struct Conn {}

impl Conn {
    /// Connects and sends HELLO; it doesn't wait for WELCOME.
    pub async fn dial(_cfg: Config) -> Result<Conn> {
        unimplemented!()
    }
    /// Waits for the server's WELCOME.
    pub async fn welcome(&self) -> Result<wire::Welcome> {
        unimplemented!()
    }
    /// Closes the connection.
    pub async fn close(&self) {
        unimplemented!()
    }
    /// Closes without the closing handshake.
    pub fn close_now(&self) {
        unimplemented!()
    }
}
