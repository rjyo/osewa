//! The single osewa binary: the server, the per-session holder, enrollment,
//! and the client commands. Same command line as the Go osewa.

use std::process::ExitCode;

mod attach;
mod channels;
mod cli;
mod commands;
mod daemon;
mod service;
mod status;
// Parts of these serve only the attach UI, ported separately.
#[allow(dead_code)]
mod tty;
mod update;
#[allow(dead_code)]
mod vtfilter;

#[cfg(test)]
mod tests;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(cli::run(&args, cli::Env::process()) as u8)
}
