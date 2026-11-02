//! The single osewa binary: the server, the per-session holder, enrollment,
//! and the client commands.

use std::process::ExitCode;

mod attach;
mod autoupdate;
mod channels;
mod cli;
mod commands;
mod daemon;
mod service;
mod tty;
mod ui;
mod update;

use osewa_vt::{status, vtfilter};

#[cfg(test)]
mod tests;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(cli::run(&args, cli::Env::process()) as u8)
}
