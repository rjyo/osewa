//! Frames as binary WebSocket messages (RFC 6455), with permessage-deflate
//! (RFC 7692) from server to client, negotiated and framed exactly as
//! coder/websocket v1.8.15 does in osewa's Go server and client.

mod conn;
mod deflate;
mod frame;
mod handshake;
#[cfg(test)]
mod tests;

pub use conn::*;
pub use handshake::{accept, dial, Acceptor};
