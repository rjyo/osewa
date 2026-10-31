//! Holder tests: a real holder on its own thread, driven over its socket by
//! a client that restores snapshots into a libghostty terminal and applies
//! output like a real one.

mod catchup;
mod flow;
// Shared by every test file; not each uses all of it.
#[allow(dead_code)]
mod helpers;
mod holder;
mod main;
mod review;
mod review3;
mod round3;
mod status;
mod trace;
mod unit;
