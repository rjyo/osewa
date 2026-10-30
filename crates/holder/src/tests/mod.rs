//! Holder tests: a real holder on its own thread, driven over its socket by
//! a client that restores snapshots into a libghostty terminal and applies
//! output like a real one. Ported from the Go holder's tests, one file per
//! Go file.

// Shared by every ported test file; not each uses all of it.
#[allow(dead_code)]
mod helpers;
mod catchup;
mod flow;
mod holder;
mod main;
mod unit;
