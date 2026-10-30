//! A session's terminal on the client side: a libghostty terminal kept from
//! the stream, as the holder's terminal looked at each applied position,
//! and mosh-style local echo predicted over it. It plugs into the transport
//! as an [`osewa_client::Renderer`]:
//!
//! ```ignore
//! let m = Mirror::new(Options { on_ready: Some(Box::new(draw)), ..Default::default() });
//! let s = conn.attach(id, AttachOptions { renderer: Some(Box::new(m.clone())), ..Default::default() }).await?;
//! m.wait_text("$ ").await?;
//! ```
//!
//! This crate links libghostty-vt. `osewa-client` itself never does, so
//! programs that only relay streams (app-hook) build without it.

mod mirror;
mod predict;

pub use mirror::{key, Mirror, Options};
pub use predict::{
    parse_predict_mode, CellState, Clock, InputMode, PredictMode, Prediction, Predictor,
    PredictorConfig,
};

/// A libghostty terminal or decoder that may move between threads.
///
/// osewa_ghostty's types are `!Send` because they hold raw pointers and
/// callback boxes that need not be `Send`. Every value wrapped here is made
/// with `Callbacks::default()`, so it holds no closures; libghostty-vt keeps
/// no thread affinity (the Go client uses it from any goroutine); and each
/// is only ever reached through a Mutex, so access is serialized.
pub(crate) struct SendCell<T>(pub T);

unsafe impl<T> Send for SendCell<T> {}
