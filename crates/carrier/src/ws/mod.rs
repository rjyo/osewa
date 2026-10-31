//! Frames as binary WebSocket messages (RFC 6455), with permessage-deflate
//! (RFC 7692) from server to client, negotiated and framed exactly as
//! docs/PROTOCOL.md has v0 servers and clients do it.

mod conn;
mod deflate;
mod frame;
mod handshake;
#[cfg(test)]
mod tests;

pub use conn::*;
pub use deflate::Level;
pub use handshake::{accept, dial, Acceptor};
