//! Links through the C API: forwards to a TCP server on the host, sessions
//! sharing the link's connection, and both across a server restart.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use super::*;

/// An echo server on the host's loopback: each connection gets back what
/// it sends, until it half-closes.
fn echo_server() -> u16 {
    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ln.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in ln.incoming() {
            let Ok(mut c) = c else { return };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = c.read(&mut buf) {
                    if n == 0 || c.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = c.shutdown(std::net::Shutdown::Write);
            });
        }
    });
    port
}

fn open_link(host: &Host, r: &Arc<Rec>) -> *mut OsewaLink {
    let mut s = Strings::default();
    let opts = LinkOptions {
        addr: s.add(&host.addr),
        server_key: s.add(&host.server_key),
        quic_port: 0,
        device_key: host.device.as_ptr(),
        device_key_len: host.device.len(),
        client_id: s.add("phone-test"),
        device_name: s.add("phone"),
    };
    let mut err: *mut c_char = std::ptr::null_mut();
    let l = unsafe { osewa_link_open(&opts, r.callbacks(), &mut err) };
    assert!(!l.is_null() && err.is_null(), "osewa_link_open failed");
    l
}

fn forward(l: *mut OsewaLink, port: u16) -> u16 {
    let host = CString::new("127.0.0.1").unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    let p = unsafe { osewa_link_forward(l, 0, host.as_ptr(), port, &mut err) };
    assert!(p != 0 && err.is_null(), "osewa_link_forward failed");
    p
}

/// One round trip through local port p: what was sent comes back.
#[track_caller]
fn round_trip(p: u16, msg: &[u8]) {
    let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
    c.set_read_timeout(Some(WAIT)).unwrap();
    c.write_all(msg).unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = Vec::new();
    c.read_to_end(&mut got).unwrap();
    assert_eq!(got, msg);
}

#[test]
fn forward_echoes_and_survives_restart() {
    let host = Host::new();
    let r = Rec::new(80, 24);
    let l = open_link(&host, &r);
    assert_eq!(r.wait_state(CONNECTED), None);
    eventually(
        "carrier",
        || !r.stats.lock().unwrap().is_empty(),
        String::new,
    );
    assert_eq!(r.stats.lock().unwrap()[0], (0, "ws".to_string()));

    let p = forward(l, echo_server());
    round_trip(p, b"hello");
    // Big enough to need the server's grants along the way.
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    round_trip(p, &big);

    // The connection drops and comes back; new connections take the new one.
    host.restart();
    eventually(
        "reconnect",
        || r.states.lock().unwrap().iter().any(|s| s.0 == RECONNECTING),
        || format!("{:?}", r.states.lock().unwrap()),
    );
    r.wait_state(CONNECTED);
    round_trip(p, b"again");

    // Unforwarded, the port stops answering.
    unsafe { osewa_link_unforward(l, p) };
    assert!(
        TcpStream::connect(("127.0.0.1", p)).is_err(),
        "still listening"
    );

    unsafe { osewa_link_close(l) };
    let calls = r.calls();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(r.calls(), calls, "callbacks after close");
}

#[test]
fn sessions_share_the_link() {
    let host = Host::new();
    let lr = Rec::new(80, 24);
    let l = open_link(&host, &lr);
    lr.wait_state(CONNECTED);

    // Connection fields are ignored on a link: none given.
    let open_on = |r: &Arc<Rec>| {
        let mut s = Strings::default();
        let mut o = c_opts(&host.open_opts(), &mut s);
        (
            o.addr,
            o.server_key,
            o.client_id,
            o.device_key,
            o.device_key_len,
        ) = (
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        let mut err: *mut c_char = std::ptr::null_mut();
        let h = unsafe { osewa_open_on(l, &o, r.callbacks(), &mut err) };
        assert!(!h.is_null() && err.is_null(), "osewa_open_on failed");
        h
    };
    let (r1, r2) = (Rec::new(80, 24), Rec::new(80, 24));
    let (h1, h2) = (open_on(&r1), open_on(&r2));
    r1.wait_state(CONNECTED);
    r2.wait_state(CONNECTED);
    send(h1, "echo one-$((1+1))\r");
    send(h2, "echo two-$((2+1))\r");
    r1.wait_screen("one-2");
    r2.wait_screen("two-3");

    // Both resume over the link's next connection.
    host.restart();
    r1.wait_state(CONNECTED);
    send(h1, "echo back-$((3+4))\r");
    r1.wait_screen("back-7");

    // Closing a session leaves the link and the other session up.
    unsafe { osewa_close(h1) };
    send(h2, "echo still-$((5+5))\r");
    r2.wait_screen("still-10");
    assert_eq!(lr.last_state().map(|s| s.0), Some(CONNECTED));

    unsafe { osewa_close(h2) };
    unsafe { osewa_link_close(l) };
}

#[test]
fn link_bad_options_fail_open() {
    let r = Rec::new(80, 24);
    let mut s = Strings::default();
    let opts = LinkOptions {
        addr: s.add("example.com:1"),
        server_key: std::ptr::null(),
        quic_port: 0,
        device_key: std::ptr::null(),
        device_key_len: 0,
        client_id: s.add("x"),
        device_name: std::ptr::null(),
    };
    let mut err: *mut c_char = std::ptr::null_mut();
    let l = unsafe { osewa_link_open(&opts, r.callbacks(), &mut err) };
    assert!(l.is_null() && !err.is_null());
    let e = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    unsafe { osewa_string_free(err) };
    assert!(e.contains("server_key"), "{e}");
}
