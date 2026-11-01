//! Reading osewa_open_options: addresses, appearance, events.

use crate::options::{parse_appearance, with_port, DEFAULT_PORT};
use crate::session::{event_json, fatal};
use osewa_wire as wire;

#[test]
fn addresses() {
    for (addr, want) in [
        ("box", format!("box:{DEFAULT_PORT}")),
        ("box:9000", "box:9000".into()),
        ("10.0.0.1", format!("10.0.0.1:{DEFAULT_PORT}")),
        ("[::1]:7000", "[::1]:7000".into()),
        ("[fe80::1]", format!("[fe80::1]:{DEFAULT_PORT}")),
        ("fe80::1", format!("[fe80::1]:{DEFAULT_PORT}")),
    ] {
        assert_eq!(with_port(addr).as_deref(), Ok(want.as_str()), "{addr}");
    }
    for bad in ["box:", ":7322", "box:99999", "[::1]x", "a b", "box/path", "user@box"] {
        assert!(with_port(bad).is_err(), "{bad} accepted");
    }
}

#[test]
fn appearance() {
    let a = parse_appearance(r##"{"color_scheme":"light","fg":"#000000","bg":"#FFfe01","palette":["#010203","#a0b0c0"]}"##).unwrap();
    assert_eq!(a.dark, Some(false));
    assert_eq!(a.colors.foreground, Some([0, 0, 0]));
    assert_eq!(a.colors.background, Some([0xff, 0xfe, 0x01]));
    assert_eq!(a.colors.palette[0], Some([1, 2, 3]));
    assert_eq!(a.colors.palette[1], Some([0xa0, 0xb0, 0xc0]));
    assert_eq!(a.colors.palette[2], None);
    assert_eq!(parse_appearance("{}").unwrap(), Default::default());
    for bad in [r#"{"fg":"red"}"#, r##"{"bg":"#12345"}"##, r##"{"fg":"#gg0000"}"##, r#"{"colour":"x"}"#, "7"] {
        assert!(parse_appearance(bad).is_err(), "{bad} accepted");
    }
    let many = format!(r#"{{"palette":[{}]}}"#, vec![r##""#000000""##; 257].join(","));
    assert!(parse_appearance(&many).is_err(), "257 palette colors");
}

/// Events reach the app as the holder sent them, kind as "type".
#[test]
fn events() {
    let e = wire::EventMsg { code: Some(7), ..wire::EventMsg::kind(wire::EVENT_EXIT) };
    assert_eq!(serde_json::from_str::<serde_json::Value>(&event_json(&e)).unwrap(), serde_json::json!({"type":"exit","code":7}));
    let e = wire::EventMsg { title: "vim".into(), ..wire::EventMsg::kind(wire::EVENT_TITLE) };
    assert_eq!(serde_json::from_str::<serde_json::Value>(&event_json(&e)).unwrap(), serde_json::json!({"type":"title","title":"vim"}));
    let e = wire::EventMsg::kind(wire::EVENT_BELL);
    assert_eq!(event_json(&e), r#"{"type":"bell"}"#);
}

/// Only what retrying can't fix closes the session.
#[test]
fn fatal_errors() {
    let incompatible = osewa_client::Error::Protocol("server speaks 1, client 0".into());
    assert!(fatal(&incompatible).is_some_and(|w| w.starts_with("incompatible: ")));
    assert_eq!(fatal(&osewa_client::Error::Closed(Some("reset".into()))), None);
    assert_eq!(fatal(&osewa_client::Error::Io(std::sync::Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)))), None);
}
