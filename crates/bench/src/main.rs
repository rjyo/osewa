//! The osewa-bench binary; see the library.

use std::fs::File;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Unbuffered, as Go's os.Stdout: gen's stamps must leave when written.
    let mut stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let mut stderr = ManuallyDrop::new(unsafe { File::from_raw_fd(2) });
    let code = osewa_bench::run(&args, &mut *stdout as &mut dyn Write, &mut *stderr as &mut dyn Write);
    ExitCode::from(code as u8)
}
