//! Draws an attached osewa session for a terminal on this side: the
//! mirror's VT stream re-rendered (a full repaint after each snapshot,
//! then the remote output, filtered), local echo predictions over it, and
//! notices. The `osewa` CLI draws on the user's terminal with it; the FFI
//! hands the same bytes to an app's own terminal.

pub mod render;
pub mod status;
pub mod ui;
pub mod vtfilter;

pub use ui::{owner_name, split_utf8, ticker, viewer_notice, Attached, Report, Ui};

#[cfg(test)]
mod tests;
