//! osewa-bench, the e2e benchmarks' helper (Go's bench/cmd/osewa-bench and
//! the bench package it uses). It runs inside the test containers:
//!
//! ```text
//! osewa-bench gen   -kind w1|w2 -rate B/s       timestamped output
//! osewa-bench drive -mode type|watch -- cmd...  run a client in a PTY
//! osewa-bench clock                              time for each stdin line
//! osewa-bench resume -addr -session -key ...     cold resume timings
//! osewa-bench serve -listen 127.0.0.1:8080       HTTP/WebSocket target for forwards
//! osewa-bench bytes -n N [-check]                transfer pattern on stdout, or check stdin
//! ```
//!
//! gen writes workload lines with a stamp line "@T<unix ms>@" before every
//! block, so whoever renders the screen can tell how old it is. drive runs
//! a terminal client in a PTY, renders its output with libghostty like a
//! real terminal, and measures what a user sees. Containers on one host
//! share the kernel clock, so stamps written in one container compare with
//! the clock in another.
//!
//! The command line, the output formats, the workloads' bytes and the
//! transfer pattern's bytes are the Go helper's exactly: the Go e2e harness
//! drives this one, and checks patterns with its own code.

pub mod pattern;
pub mod stamp;
pub mod w4;
pub mod workload;
