//! Sessions: attach, snapshots, output, acks, input and resume (Go's
//! client/session.go).

use std::time::Duration;

use osewa_wire as wire;

/// A Renderer's failure: the stream can't be rendered, so the connection
/// ends with it.
pub type RendererError = Box<dyn std::error::Error + Send + Sync>;

/// Keeps a terminal from a session's stream; `osewa_mirror::Mirror` is one.
/// Its methods run in stream order on the connection's applier thread,
/// with the session locked, before the [`AttachOptions`] callbacks for the
/// same frame. An error ends the connection: the stream can't be rendered.
///
/// `snapshot_begin` before the previous snapshot's `snapshot_ready` means
/// that snapshot was abandoned (PROTOCOL.md "Lagging clients and
/// catch-up"): drop its partial decode and keep the current terminal until
/// the new READY. `end` runs once, when the session ends for this client.
pub trait Renderer: Send {
    fn snapshot_begin(&mut self, b: &wire::SnapshotBegin) -> Result<(), RendererError>;
    fn snapshot_data(&mut self, data: &[u8], history: bool) -> Result<(), RendererError>;
    fn snapshot_ready(&mut self) -> Result<(), RendererError>;
    fn snapshot_end(&mut self) -> Result<(), RendererError>;
    fn output(&mut self, data: &[u8]) -> Result<(), RendererError>;
    fn event(&mut self, e: &wire::EventMsg, live: bool) -> Result<(), RendererError>;
    fn end(&mut self);
}

type Cb<T> = Option<Box<T>>;

/// Configures an attach.
#[derive(Default)]
pub struct AttachOptions {
    pub cols: i64,
    pub rows: i64,
    /// Claims ownership on attach. Resume never claims it: another client
    /// may have taken over meanwhile, and the holder keeps our ownership
    /// through its grace period anyway.
    pub own: bool,
    /// If set, gets the stream first (see [`Renderer`]).
    pub renderer: Option<Box<dyn Renderer>>,

    /// The raw stream, for an app that feeds it into its own libghostty.
    /// All run with the session locked: they must not call back into the
    /// Session. `on_snapshot_data` gets the bytes through READY with
    /// history false (before `on_ready`), then the history bytes;
    /// `on_snapshot_end` follows the last of them. A begin with history
    /// false (a catch-up screen) brings no history. An `on_snapshot_begin`
    /// before `on_ready` abandons the snapshot in progress.
    pub on_snapshot_begin: Cb<dyn FnMut(&wire::SnapshotBegin) + Send>,
    pub on_snapshot_data: Cb<dyn FnMut(&[u8], bool) + Send>,
    pub on_ready: Cb<dyn FnMut() + Send>,
    pub on_snapshot_end: Cb<dyn FnMut() + Send>,
    /// Runs with each OUTPUT's bytes.
    pub on_output: Cb<dyn FnMut(&[u8]) + Send>,
    /// Runs with every applied event; live is false for events from a
    /// resume catch-up, whose effects must not fire.
    pub on_event: Cb<dyn FnMut(&wire::EventMsg, bool) + Send>,
    /// Runs after each frame that moved the position.
    pub on_applied: Cb<dyn FnMut() + Send>,
    /// Gets each round trip sample, timed from an input's send to its
    /// INPUT_ACK.
    pub on_rtt: Cb<dyn FnMut(Duration) + Send>,

    /// Returns no CREDIT for this stream until the caller reports bytes
    /// consumed with [`Session::credit`], for a consumer that relays the
    /// stream elsewhere and must not buffer it without bound. The callbacks
    /// must then not block: the holder's catch-up (screen snapshots instead
    /// of a queue) takes care of a consumer that falls behind. `on_counted`
    /// runs after the callbacks of each frame that counts toward credit,
    /// with its counted size: OUTPUT and EVENT payloads including their
    /// 8-byte end, SNAPSHOT_HISTORY, and SNAPSHOT_DATA of every snapshot but
    /// an attach's first (PROTOCOL.md "Flow control").
    pub manual_credit: bool,
    pub on_counted: Cb<dyn FnMut(usize) + Send>,
}

/// The PTY's line discipline as the holder last reported it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputMode {
    /// An input_mode event has arrived.
    pub known: bool,
    pub echo: bool,
    pub canonical: bool,
}

/// One attached session.
#[derive(Clone)]
pub struct Session {}
