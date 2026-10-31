//! osewa-bench, the e2e benchmarks' helper. It runs inside the test
//! containers:
//!
//! ```text
//! osewa-bench gen   -kind w1|w2 -rate B/s       timestamped output
//! osewa-bench drive -mode type|watch -- cmd...  run a client in a PTY
//! osewa-bench clock                              time for each stdin line
//! osewa-bench resume -addr -session -key ...     cold resume timings
//! osewa-bench serve -listen 127.0.0.1:8080       HTTP/WebSocket target for forwards
//! osewa-bench bytes -n N [-check]                transfer pattern on stdout, or check stdin
//! osewa-bench vs [-runs N] [-quic] [-mosh] host   osewa vs ssh vs mosh on a real host
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
//! transfer pattern's bytes are fixed: the e2e harness drives this helper
//! and checks patterns with its own code, and a benchmark is only
//! comparable on the same bytes.

use std::fmt;
use std::io::{self, BufRead, Write};

pub mod drive;
pub mod gen;
pub mod pattern;
pub mod resume;
pub mod serve;
pub mod stamp;
pub mod vs;
pub mod w4;
pub mod workload;

#[cfg(test)]
mod tests;

/// A command's failure, printed as "osewa-bench: <it>".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl Error {
    pub(crate) fn io(e: io::Error) -> Error {
        Error(go_io_error(&e))
    }
}

pub const USAGE: &str = "usage: osewa-bench gen|drive|clock|resume|serve|bytes|vs [flags]";

/// Runs one command; the exit code is what the harness expects: 2 for no
/// command, 1 for any failure.
pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let Some((cmd, rest)) = args.split_first() else {
        let _ = writeln!(stderr, "{USAGE}");
        return 2;
    };
    let r = match cmd.as_str() {
        "gen" => gen::gen_main(rest, stdout),
        "drive" => drive::drive_main(rest, stdout),
        "clock" => clock_main(io::stdin().lock(), stdout),
        "resume" => resume::resume_main(rest, stdout),
        "serve" => serve::serve_main(rest),
        "bytes" => serve::bytes_main(rest, io::stdin().lock(), stdout),
        "vs" => vs::vs_main(rest, stdout),
        c => Err(Error(format!("unknown command {}", osewa_wire::go_quote(c)))),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(stderr, "osewa-bench: {e}");
            1
        }
    }
}

/// Answers every input line with this host's time in unix nanoseconds. Over
/// `docker exec -i` it lets the harness measure the offset between its
/// clock and the container's NTP-style: the reply's time lies within the
/// round trip of the line.
pub fn clock_main(r: impl BufRead, w: &mut dyn Write) -> Result<(), Error> {
    for line in r.split(b'\n') {
        line.map_err(Error::io)?;
        let ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        writeln!(w, "{ns}").map_err(Error::io)?;
    }
    Ok(())
}

/// Parses flags (osewa_flag's syntax): a bad flag prints its error and the
/// usage to stderr and fails.
pub(crate) fn parse(fs: &osewa_flag::FlagSet, args: &[String]) -> Result<osewa_flag::Parsed, Error> {
    fs.parse(args).map_err(|e| {
        if e.0 == "help requested" {
            eprint!("{}", fs.usage());
            return Error("flag: help requested".into());
        }
        eprintln!("{e}");
        eprint!("{}", fs.usage());
        Error(e.0)
    })
}

/// An int flag; a bad one is rejected with the usage, as a parse error is.
pub(crate) fn int(p: &osewa_flag::Parsed, name: &str) -> Result<i64, Error> {
    p.int(name).map_err(|e| {
        eprintln!("{e}");
        Error(e.0)
    })
}

/// A duration flag, as int.
pub(crate) fn duration(p: &osewa_flag::Parsed, name: &str) -> Result<std::time::Duration, Error> {
    p.duration(name).map_err(|_| {
        let e = format!("invalid value {} for flag -{name}: parse error", osewa_wire::go_quote(&p.str(name)));
        eprintln!("{e}");
        Error(e)
    })
}

/// A JSON array of numbers as the harness parses and compares them: the
/// shortest representation, no ".0" on whole numbers, and null for none.
pub(crate) fn json_f64s(v: &[f64]) -> String {
    if v.is_empty() {
        return "null".into();
    }
    let parts: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("[{}]", parts.join(","))
}

/// An I/O error as the C library words it ("no such file or
/// directory"), without Rust's " (os error N)": the harness matches the
/// text.
pub(crate) fn go_io_error(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => {
            let mut t = s[..i].to_string();
            if let Some(c) = t.get(..1) {
                t.replace_range(..1, &c.to_lowercase());
            }
            t
        }
        None => s,
    }
}

/// A failed start, worded as the harness expects ("exec: ...: executable
/// file not found in $PATH").
pub(crate) fn go_exec_error(name: &str, e: &io::Error) -> String {
    if e.kind() == io::ErrorKind::NotFound && !name.contains('/') {
        return format!("exec: {}: executable file not found in $PATH", osewa_wire::go_quote(name));
    }
    format!("fork/exec {name}: {}", go_io_error(e))
}
