//! Socket tuning for client connections (docs/PROTOCOL.md, In flight).

use std::os::fd::RawFd;

/// Keeps at most this much unsent data in a client socket's kernel buffer.
/// Bytes there can't be passed by a keystroke's echo, so the less sits
/// there, the more the send queue's priority decides what goes next.
pub(crate) const NOT_SENT_LOWAT: libc::c_int = 16 << 10;

/// <netinet/tcp.h>'s TCP_NOTSENT_LOWAT; 0 where the platform has none.
#[cfg(target_os = "macos")]
pub(crate) const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;
#[cfg(target_os = "linux")]
pub(crate) const TCP_NOTSENT_LOWAT: libc::c_int = 25;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) const TCP_NOTSENT_LOWAT: libc::c_int = 0;

/// Sets TCP_NOTSENT_LOWAT, where the platform has it, and the congestion
/// control, on an accepted connection.
pub(crate) fn tune(fd: RawFd) {
    if TCP_NOTSENT_LOWAT == 0 {
        return;
    }
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            (&NOT_SENT_LOWAT as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    set_congestion(fd);
}

/// Asks for BBR, which keeps a long or lossy path full where the usual
/// default, CUBIC, backs off at every loss: a host far away carries more.
/// The kernel refuses it unless BBR is built and listed in
/// net.ipv4.tcp_allowed_congestion_control (or this runs as root); the
/// default stays then.
#[cfg(target_os = "linux")]
fn set_congestion(fd: RawFd) {
    let bbr = b"bbr";
    unsafe {
        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_CONGESTION, bbr.as_ptr().cast(), bbr.len() as libc::socklen_t);
    }
}

/// There is no per-socket choice here: the system's congestion control
/// stays.
#[cfg(not(target_os = "linux"))]
fn set_congestion(_: RawFd) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn not_sent_lowat() {
        if TCP_NOTSENT_LOWAT == 0 {
            return; // no TCP_NOTSENT_LOWAT here
        }
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _c = std::net::TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (conn, _) = ln.accept().unwrap();
        tune(conn.as_raw_fd());
        let mut v: libc::c_int = 0;
        let mut n = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(conn.as_raw_fd(), libc::IPPROTO_TCP, TCP_NOTSENT_LOWAT, (&mut v as *mut libc::c_int).cast(), &mut n)
        };
        assert_eq!((r, v), (0, NOT_SENT_LOWAT));
    }
}
