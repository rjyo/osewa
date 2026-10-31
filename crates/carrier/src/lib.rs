//! Carriers move osewa frames between a client and the server, one frame
//! per message: a WebSocket over TCP (docs/PROTOCOL.md) or QUIC
//! (docs/QUIC.md), each byte-compatible with every protocol v0 peer.

pub mod quic;
pub mod ws;
