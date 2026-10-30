use std::ffi::CStr;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Opens a PTY pair: the master (non-blocking, close-on-exec) and the
/// slave.
pub(crate) fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    unsafe {
        let m = cvt(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY))?;
        let master = OwnedFd::from_raw_fd(m);
        cvt(libc::grantpt(m))?;
        cvt(libc::unlockpt(m))?;
        // ptsname's static buffer is safe here: the holder opens one PTY,
        // before it starts any other thread.
        let name = libc::ptsname(m);
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let path = CStr::from_ptr(name).to_owned();
        let s = cvt(libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC))?;
        let slave = OwnedFd::from_raw_fd(s);
        let flags = cvt(libc::fcntl(m, libc::F_GETFL))?;
        cvt(libc::fcntl(m, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        cvt(libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC))?;
        set_size(m, cols, rows)?;
        Ok((master, slave))
    }
}

/// Gives the PTY a size; the kernel sends the child SIGWINCH.
pub(crate) fn set_size(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) }).map(|_| ())
}

/// The PTY's line discipline as far as echo prediction cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct InputMode {
    pub echo: bool,
    pub canonical: bool,
}

/// Reads the termios of the PTY. On Linux and macOS the master reports the
/// slave's settings, so the child's stty changes show here.
pub(crate) fn read_mode(fd: RawFd) -> io::Result<InputMode> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::tcgetattr(fd, &mut t) })?;
    Ok(InputMode { echo: t.c_lflag & libc::ECHO != 0, canonical: t.c_lflag & libc::ICANON != 0 })
}

/// Builds the child's command: its own session with the PTY as its
/// controlling terminal and stdio.
pub(crate) fn command(
    argv: &[String],
    cwd: &str,
    env: &[String],
    slave: &OwnedFd,
) -> io::Result<tokio::process::Command> {
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if !cwd.is_empty() {
        cmd.current_dir(Path::new(cwd));
    }
    cmd.env("TERM", "xterm-256color");
    for kv in env {
        if let Some((k, v)) = kv.split_once('=') {
            cmd.env(k, v);
        }
    }
    cmd.stdin(slave.try_clone()?);
    cmd.stdout(slave.try_clone()?);
    cmd.stderr(slave.try_clone()?);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(tokio::process::Command::from(cmd))
}

/// The holder's exit code convention: 128 + the signal for a signaled
/// child.
pub(crate) fn exit_code(st: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = st.signal() {
        return 128 + sig as i64;
    }
    st.code().unwrap_or(-1) as i64
}
