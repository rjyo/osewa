//! Manual credit and key validation.

use super::*;

fn mirror_event() -> wire::EventMsg {
    wire::EventMsg { title: "t".into(), ..wire::EventMsg::kind(wire::EVENT_TITLE) }
}

/// With manual credit nothing is credited until the caller says so, and
/// on_counted adds up to exactly what the server counted.
#[test]
fn manual_credit() {
    let f = Fake::new();
    let id = f.create(40, 5);
    for _ in 0..50 {
        f.output(&id, "history\r\n");
    }
    let counted = shared::<(usize, usize)>();
    let c2 = counted.clone();
    let s = attach_raw(
        &f.dial("a"),
        &id,
        AttachOptions {
            cols: 40,
            rows: 5,
            manual_credit: true,
            on_counted: Some(Box::new(move |n| {
                let mut c = c2.lock().unwrap();
                c.0 += n;
                c.1 += 1;
            })),
            ..Default::default()
        },
    );
    for _ in 0..20 {
        f.output(&id, &"o".repeat(3000));
    }
    f.emit(&id, mirror_event());
    f.screen_snapshot(&id); // a later snapshot counts its screen too
    let total = || counted.lock().unwrap().0;
    eventually("all counted", || {
        let (sent, _) = f.credited(&id);
        sent > 0 && total() == sent
    });
    let (_, cr) = f.credited(&id);
    assert_eq!(cr, 0, "credited before the caller consumed anything");
    s.credit(1000);
    eventually("credit", || f.credited(&id).1 == 1000);
    // Never more than was received on this attach.
    s.credit(1 << 30);
    eventually("credit capped", || f.credited(&id).1 == total());
    s.credit(1);
    s.credit(0);

    // A resume starts over: what the old stream owed is not credited to the
    // new one.
    f.drop_all();
    eventually("lost", || !s.connected());
    s.credit(10); // disconnected: nothing to do
    run(s.resume(&f.dial("a"))).unwrap();
    s.credit(500);
    f.output(&id, "after");
    eventually("counted after", || f.credited(&id).0 > 0);
    let (_, cr) = f.credited(&id);
    assert_eq!(cr, 0, "new stream credited for the old one's bytes");
    s.close();
}

/// Without manual credit, on_counted never runs and credit flows by itself.
#[test]
fn auto_credit_ignores_on_counted() {
    let f = Fake::new();
    let id = f.create(40, 5);
    let called = shared::<bool>();
    let c2 = called.clone();
    let s = attach(&f.dial("a"), &id, AttachOptions { on_counted: Some(Box::new(move |_| *c2.lock().unwrap() = true)), ..Default::default() });
    f.output(&id, &"a".repeat(20000));
    f.output(&id, "end");
    s.wait_text("end");
    eventually("credit", || {
        let (sent, cr) = f.credited(&id);
        sent > 0 && cr == sent
    });
    assert!(!*called.lock().unwrap(), "on_counted ran without manual credit");
}

#[test]
fn key_validation() {
    let f = Fake::new();
    let id = f.create(40, 5);
    let s = attach(&f.dial("a"), &id, own());
    assert!(s.key("KeyA", &["hyper"], "", "").is_err());
    assert!(s.key("KeyA", &[], "", "hold").is_err());
    assert!(s.key("", &[], "a", "").is_err());
    s.key("KeyA", &["ctrl"], "", "release").unwrap();
    run(s.flush()).unwrap();
    f.with(&id, |fs| {
        let i = fs.applied.last().unwrap();
        assert!(i.key == "KeyA" && i.action == "release" && i.mods[0] == "ctrl", "applied {i:?}");
    });
}

/// The dialer keeps little unsent data in the kernel.
#[test]
fn not_sent_lowat() {
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let c = std::net::TcpStream::connect(ln.local_addr().unwrap()).unwrap();
    crate::dial::set_not_sent_lowat(&c);
    let mut v: libc::c_int = 0;
    let mut n = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    use std::os::fd::AsRawFd;
    let r = unsafe {
        libc::getsockopt(c.as_raw_fd(), libc::IPPROTO_TCP, crate::dial::TCP_NOTSENT_LOWAT, (&mut v as *mut libc::c_int).cast(), &mut n)
    };
    assert!(r == 0 && v == crate::dial::NOT_SENT_LOWAT, "TCP_NOTSENT_LOWAT = {v}, {r}");
}
