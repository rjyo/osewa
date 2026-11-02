use super::*;
use std::io::Cursor;

#[test]
fn frame_round_trip() {
    let f = Frame::new(TYPE_OUTPUT, 7, b"abc".to_vec());
    let got = Frame::unmarshal(&f.marshal()).unwrap();
    assert_eq!(got, f);
}

#[test]
fn unmarshal_errors() {
    assert_eq!(Frame::unmarshal(&[1, 2]), Err(Error::Short));
    assert_eq!(Frame::unmarshal(&[0x7f, 0, 0, 0, 0]), Err(Error::UnknownType(0x7f)));
    assert_eq!(Frame::unmarshal(&vec![0; MAX_FRAME + 1]), Err(Error::TooLarge));
}

#[test]
fn length_prefixed() {
    let frames = [
        Frame::new(TYPE_HELLO, 0, b"{}".to_vec()),
        output(3, 99, b"out"),
        Frame::new(TYPE_SNAPSHOT_READY, 3, vec![]),
    ];
    let mut buf = Vec::new();
    for f in &frames {
        write(&mut buf, f).unwrap();
    }
    let mut parsed = Vec::new();
    assert_eq!(parse_frames(&buf[..buf.len() - 1], &mut parsed).unwrap(), buf.len() - 9);
    assert_eq!(parsed, frames[..2]);
    let mut r = Cursor::new(buf);
    for want in &frames {
        assert_eq!(&read(&mut r).unwrap(), want);
    }
    assert_eq!(read(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn read_rejects_bad_lengths() {
    let err = read(&mut Cursor::new(vec![2, 0, 0, 0, 1, 2])).unwrap_err();
    assert_eq!(err.to_string(), "wire: frame too short");
    let err = read(&mut Cursor::new(vec![0xff, 0xff, 0xff, 0x7f])).unwrap_err();
    assert_eq!(err.to_string(), "wire: frame too large");
    let err = read(&mut Cursor::new(vec![9, 0, 0, 0, 1])).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    let big = Frame::new(TYPE_OUTPUT, 0, vec![0; MAX_FRAME]);
    assert!(write(&mut io::sink(), &big).is_err());
    assert!(parse_frames(&[0xff, 0xff, 0xff, 0x7f], &mut Vec::new()).is_err());
}

#[test]
fn payload_helpers() {
    let o = output(1, 1234, b"x");
    assert_eq!(split_end(&o.payload).unwrap(), (1234, &b"x"[..]));
    let e = event(1, 5, &marshal_event(&EventMsg::kind(EVENT_BELL)));
    assert_eq!(split_end(&e.payload).unwrap(), (5, &br#"{"kind":"bell"}"#[..]));
    assert!(split_end(&[1]).is_err());
    assert_eq!(parse_ack(&ack(1, 3, 77).payload).unwrap(), (3, 77));
    assert!(parse_ack(&[]).is_err());
    assert_eq!(parse_u64(&input_ack(1, 42).payload).unwrap(), 42);
    assert!(parse_u64(&[]).is_err());
    assert_eq!(parse_credit(&credit(1, 4096).payload).unwrap(), 4096);
    assert!(parse_credit(&[]).is_err());
    assert_eq!(record_size(3), 8);
}

#[test]
fn channel_frames() {
    let f = channel_data(7, CHANNEL_STDERR, b"oops");
    let g = Frame::unmarshal(&f.marshal()).unwrap();
    assert_eq!((g.ty, g.stream), (TYPE_CHANNEL_DATA, 7));
    assert_eq!(split_channel(&g.payload).unwrap(), (CHANNEL_STDERR, &b"oops"[..]));
    assert_eq!(split_channel(&channel_eof(7, CHANNEL_STDOUT).payload).unwrap(), (CHANNEL_STDOUT, &b""[..]));
    assert_eq!(split_channel(&[]), Err(Error::Short));
    let gr = grant(3, 1234);
    assert_eq!((gr.ty, parse_credit(&gr.payload).unwrap()), (TYPE_GRANT, 1234));
    for t in [TYPE_DATA, TYPE_EOF, TYPE_GRANT, TYPE_CHANNEL_DATA, TYPE_CHANNEL_EOF] {
        assert!(known(t), "0x{t:02x}");
    }
    assert!(has_feature(&[FEATURE_EXEC.into(), FEATURE_FORWARD.into()], FEATURE_FORWARD));
    assert!(!has_feature(&[], FEATURE_EXEC));
}

#[test]
fn json_request() {
    let req = Request { id: 1, method: METHOD_ATTACH.into(), session: "s".into(), position: Some(10), ..Default::default() };
    let f = json(TYPE_REQUEST, 0, &req);
    assert_eq!(f.payload, br#"{"id":1,"method":"session.attach","session":"s","position":10}"#);
    let r: Request = f.decode().unwrap();
    assert_eq!(r, req);
    assert!(Frame::new(TYPE_REQUEST, 0, b"{".to_vec()).decode::<Request>().is_err());
}

// v0 JSON: unset optional fields absent, unset lists as null, bytes as
// base64. These are byte-exact so every v0 peer agrees on every message.
#[test]
fn v0_json_bytes() {
    let r: Request = serde_json::from_str(r#"{"id":3,"method":"session.create","argv":null,"env":null,"extra":1}"#).unwrap();
    assert_eq!((r.id, r.argv.len()), (3, 0));
    let w: Welcome = serde_json::from_str(r#"{"version":0,"server":"h","snapshot_version":1,"ghostty":"c","carriers":null}"#).unwrap();
    assert!(w.carriers.is_empty());
    assert_eq!(
        serde_json::to_string(&Welcome { server: "h".into(), snapshot_version: 1, ..Default::default() }).unwrap(),
        r#"{"version":0,"server":"h","snapshot_version":1,"ghostty":"","carriers":[]}"#
    );

    let clip = EventMsg { kind: EVENT_CLIPBOARD.into(), data: b"hi!".to_vec(), ..Default::default() };
    let b = marshal_event(&clip);
    assert_eq!(b, br#"{"kind":"clipboard","data":"aGkh"}"#);
    assert_eq!(serde_json::from_slice::<EventMsg>(&b).unwrap(), clip);
    let ev: EventMsg = serde_json::from_str(r#"{"kind":"clipboard","data":null}"#).unwrap();
    assert!(ev.data.is_empty());

    let mode = EventMsg { kind: EVENT_INPUT_MODE.into(), echo: Some(false), canonical: Some(true), ..Default::default() };
    assert_eq!(marshal_event(&mode), br#"{"kind":"input_mode","echo":false,"canonical":true}"#);
    let exit = EventMsg { kind: EVENT_EXIT.into(), code: Some(0), ..Default::default() };
    assert_eq!(marshal_event(&exit), br#"{"kind":"exit","code":0}"#);

    let info = SessionInfo { id: "a".into(), cols: 80, rows: 24, ..Default::default() };
    assert_eq!(
        serde_json::to_string(&info).unwrap(),
        r#"{"id":"a","title":"","cols":80,"rows":24,"created":0,"exited":false,"exit_code":0,"attached":0,"idle_seconds":0}"#
    );
    let resp = Response { id: 2, epoch: "e".into(), head: Some(0), ..Default::default() };
    assert_eq!(serde_json::to_string(&resp).unwrap(), r#"{"id":2,"epoch":"e","head":0}"#);
    let begin = SnapshotBegin { generation: 1, epoch: "e".into(), position: 5, cols: 80, rows: 24, history: Some(false) };
    assert_eq!(
        serde_json::to_string(&begin).unwrap(),
        r#"{"generation":1,"epoch":"e","position":5,"cols":80,"rows":24,"history":false}"#
    );
    let end = StreamEnd { reason: END_EXITED.into(), exit_code: Some(3) };
    assert_eq!(serde_json::to_string(&end).unwrap(), r#"{"reason":"exited","exit_code":3}"#);
    let input: Input = serde_json::from_str(r#"{"seq":1,"type":"key","key":"KeyA","mods":["ctrl"],"text":"a"}"#).unwrap();
    assert_eq!((input.ty.as_str(), input.mods.len()), ("key", 1));
}

#[test]
fn errors_display() {
    assert_eq!(Error::UnknownType(0x7f).to_string(), "wire: unknown frame type 0x7f");
    let e: io::Error = Error::Short.into();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
}

// What v0 JSON accepts and writes, beyond the field shapes.
#[test]
fn lenient_decoding_and_html_escaping() {
    let f = Frame::new(TYPE_INPUT, 1, br#"{"seq":5,"Type":"key","key":"KeyA","text":null,"focused":null,"seq":6}"#.to_vec());
    let i: Input = f.decode().unwrap();
    assert_eq!((i.seq, i.ty.as_str(), i.text.as_str(), i.focused), (6, "key", "", false));
    let f = Frame::new(TYPE_INPUT, 1, b"{\"seq\":1,\"type\":\"text\",\"text\":\"a\xffb\"}".to_vec());
    assert_eq!(f.decode::<Input>().unwrap().text, "a\u{fffd}b");
    let ev = EventMsg { kind: EVENT_TITLE.into(), title: "a<b>&c\u{2028}".into(), ..Default::default() };
    assert_eq!(marshal_event(&ev), br#"{"kind":"title","title":"a\u003cb\u003e\u0026c\u2028"}"#);
    assert_eq!(serde_json::from_slice::<EventMsg>(&marshal_event(&ev)).unwrap(), ev);
}

#[test]
fn quoting() {
    assert_eq!(quote("nope"), "\"nope\"");
    assert_eq!(quote("a\x1b\"\\\n\u{85}é"), r#""a\x1b\"\\\n\u0085é""#);
}

#[test]
fn build_id_and_older_session_info() {
    let b = Build { version: 0, ghostty: "83fb95624b0439ee660c2679b43b38c8b63769cd".into(), snapshot_version: 1 };
    assert_eq!(b.id(), "p0-s1");
    // Another libghostty commit with the same snapshots interoperates.
    assert!(b.compatible(&Build { ghostty: "0".repeat(40), ..b.clone() }));
    assert!(!b.compatible(&Build { snapshot_version: 2, ..b.clone() }));
    assert!(!b.compatible(&Build { version: 1, ..b.clone() }));
    // Holders before builds were reported: no build, nothing incompatible.
    let s: SessionInfo = serde_json::from_str(r#"{"id":"a","cols":80,"rows":24}"#).unwrap();
    assert!(s.build.is_none() && s.incompatible.is_empty());
    let s = SessionInfo { build: Some(b.clone()), ..Default::default() };
    let back: SessionInfo = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back.build, Some(b));
}
