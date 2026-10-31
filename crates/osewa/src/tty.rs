//! The local terminal: raw mode and size.

use std::io;
use std::os::fd::RawFd;

/// Puts the terminal on fd in raw mode, like cfmakeraw, and returns what
/// restores it.
pub(crate) fn make_raw(fd: RawFd) -> io::Result<Box<dyn FnOnce() + Send>> {
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut raw = old;
    raw.c_iflag &= !(libc::IGNBRK | libc::BRKINT | libc::PARMRK | libc::ISTRIP | libc::INLCR | libc::IGNCR | libc::ICRNL | libc::IXON);
    raw.c_oflag &= !libc::OPOST;
    raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    raw.c_cflag &= !(libc::CSIZE | libc::PARENB);
    raw.c_cflag |= libc::CS8;
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Box::new(move || {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    }))
}

pub(crate) fn is_terminal(fd: RawFd) -> bool {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(fd, &mut t) == 0 }
}

/// fd's size in cells, or 80x24 when fd isn't a terminal.
pub(crate) fn term_size(fd: RawFd) -> (i64, i64) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return (80, 24);
    }
    (ws.ws_col as i64, ws.ws_row as i64)
}
