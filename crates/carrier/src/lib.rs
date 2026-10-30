//! Carriers move osewa frames between a client and the server, one frame
//! per message: a WebSocket over TCP (docs/PROTOCOL.md), byte-compatible
//! with the Go peers' coder/websocket.

pub mod ws;
