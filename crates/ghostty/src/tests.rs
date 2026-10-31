use super::*;
use std::cell::RefCell;
use std::rc::Rc;

fn new_term(cols: usize, rows: usize, cb: Callbacks) -> Terminal {
    Terminal::new(cols, rows, Options { continuation: true, callbacks: cb, ..Default::default() }).unwrap()
}

// Everything the tests compare between two terminals.
fn state(term: &Terminal) -> String {
    let vt = term.format(Format::VtPalette, false, false).unwrap();
    let (cols, rows) = term.size();
    let (x, y) = term.cursor();
    format!(
        "{cols}x{rows} cursor={x},{y} alt={} title={:?}\n{:?}",
        term.alt_screen(),
        term.title(),
        merge_links(&String::from_utf8_lossy(&vt))
    )
}

// Drops an OSC 8 close that is followed, with only line breaks and cursor
// moves in between, by a reopen of the same URI. libghostty keeps hyperlink
// ids per page, so a restored terminal whose pages split at a different row
// formats one link run as two. Every cell still carries the same URI.
fn merge_links(vt: &str) -> String {
    let link = regex::Regex::new("\x1b]8;;([^\x1b\x07]*)\x1b\\\\").unwrap();
    let moves = regex::Regex::new("^(?:\r|\n|\x1b\\[[0-9;]*[ABCDGH])*").unwrap();
    let mut out = String::new();
    let mut open = String::new();
    let mut rest = vt;
    loop {
        let Some(c) = link.captures(rest) else {
            out.push_str(rest);
            return out;
        };
        let m = c.get(0).unwrap();
        out.push_str(&rest[..m.start()]);
        let uri = c.get(1).unwrap().as_str().to_string();
        let after = &rest[m.end()..];
        if uri.is_empty() && !open.is_empty() {
            let gap = moves.find(after).map_or("", |g| g.as_str());
            if let Some(n) = link.captures(&after[gap.len()..]) {
                if n.get(0).unwrap().start() == 0 && n.get(1).unwrap().as_str() == open {
                    out.push_str(gap);
                    rest = &after[gap.len() + n.get(0).unwrap().end()..];
                    continue;
                }
            }
        }
        out.push_str(m.as_str());
        open = uri;
        rest = after;
    }
}

#[test]
fn write_and_text() {
    let mut term = new_term(20, 5, Callbacks::default());
    term.write(b"hello\r\nworld");
    assert_eq!(term.text(), "hello\nworld");
    assert_eq!(term.cursor(), (5, 1));
}

#[test]
fn bad_size() {
    assert!(Terminal::new(0, 10, Options::default()).is_err());
    let mut term = new_term(10, 10, Callbacks::default());
    assert!(term.resize(10, 0).is_err());
    term.resize(30, 7).unwrap();
    assert_eq!(term.size(), (30, 7));
}

// A snapshot taken at any byte of a stream, restored and continued with the
// rest of the stream, must equal a terminal that saw every byte.
#[test]
fn snapshot_continuation_at_every_byte() {
    let stream: &[u8] = b"plain \x1b[1;31mred bold\x1b[0m\r\n\
\x1b]2;my title\x1b\\\
\x1b[?1049h\x1b[2Jalt screen \xe4\xbd\xa0\xe5\xa5\xbd \xf0\x9f\x98\x80\r\n\
\x1b[?25l\x1b[5;3Hplaced\x1b[?1049l\
\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ tail\r\n\
\x1b[?2004h\x1b[?1h\x1b[3g\x1b[1;2r\x1b[r";
    let mut reference = new_term(40, 8, Callbacks::default());
    reference.write(stream);
    let want = state(&reference);
    for cut in 0..=stream.len() {
        let mut src = new_term(40, 8, Callbacks::default());
        src.write(&stream[..cut]);
        let snap = src.snapshot().unwrap();
        let mut restored = restore(&snap, Callbacks::default()).unwrap();
        restored.write(&stream[cut..]);
        assert_eq!(state(&restored), want, "cut {cut}");
    }
}

#[test]
fn snapshot_history_incremental() {
    let mut src = Terminal::new(120, 4, Options { continuation: true, scrollback_bytes: 10 << 20, ..Default::default() }).unwrap();
    for i in 0..3000 {
        src.write(format!("history line {i}\r\n").as_bytes());
    }
    let snap = src.snapshot().unwrap();
    let mut dec = Decoder::new().unwrap();
    dec.feed(&snap);
    let mut term = dec.ready(Callbacks::default()).unwrap();
    assert!(term.text().contains("history line 2999"), "screen at READY lacks the last line");
    // Live output arrives before history finishes.
    term.write(b"live\r\n");
    dec.finish().unwrap();
    // Every history row came back, plus the line "live" pushed up.
    let want = src.scrollback_rows() + 1;
    assert_eq!(term.scrollback_rows(), want);
    assert!(want >= 2990);
    assert!(term.text().contains("live"));
}

#[test]
fn restore_rejects_garbage() {
    assert!(restore(b"not a snapshot at all", Callbacks::default()).is_err());
    let mut src = new_term(10, 3, Callbacks::default());
    src.write(b"x");
    let mut snap = src.snapshot().unwrap();
    let mid = snap.len() / 2;
    snap[mid] ^= 0xff;
    assert!(restore(&snap, Callbacks::default()).is_err(), "want CRC error for corrupted snapshot");
}

#[test]
fn query_replies() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let r = replies.clone();
    let mut term =
        new_term(80, 24, Callbacks { write_pty: Some(Box::new(move |b| r.borrow_mut().extend_from_slice(b))), ..Default::default() });
    term.write(b"\x1b[3;5H\x1b[6n");
    assert_eq!(replies.borrow().as_slice(), b"\x1b[3;5R");
    replies.borrow_mut().clear();
    term.write(b"\x1b[?7$p");
    assert!(replies.borrow().starts_with(b"\x1b[?7;"), "DECRQM reply = {:?}", replies.borrow());
}

#[test]
fn no_replies_without_callback() {
    let mut term = new_term(80, 24, Callbacks::default());
    term.write(b"\x1b[6n\x1b[c"); // must not crash or block
}

#[test]
fn effects() {
    #[derive(Default)]
    struct Seen {
        bells: usize,
        title: String,
        clip: Vec<u8>,
        status: Vec<Status>,
    }
    let seen = Rc::new(RefCell::new(Seen::default()));
    let (a, b, c, d) = (seen.clone(), seen.clone(), seen.clone(), seen.clone());
    let mut term = new_term(
        80,
        24,
        Callbacks {
            bell: Some(Box::new(move || a.borrow_mut().bells += 1)),
            title: Some(Box::new(move |t| b.borrow_mut().title = t)),
            clipboard: Some(Box::new(move |x| c.borrow_mut().clip = x.to_vec())),
            status: Some(Box::new(move |s| d.borrow_mut().status.push(s))),
            ..Default::default()
        },
    );
    term.write(b"\x07\x1b]2;work\x1b\\\x1b]52;c;aGVsbG8=\x1b\\\x1b]7501;state=blocked:kind=permission:app=claude-code:msg=QXBwbHk/\x07");
    let s = seen.borrow();
    assert_eq!(s.bells, 1);
    assert_eq!(s.title, "work");
    assert_eq!(s.clip, b"hello");
    let want = Status {
        state: "blocked".into(),
        kind: "permission".into(),
        progress: -1,
        app: "claude-code".into(),
        message: "Apply?".into(),
        ..Default::default()
    };
    assert_eq!(s.status, vec![want]);
}

fn key(code: &str, mods: &[&str], text: &str) -> Key {
    Key { code: code.into(), mods: parse_mods(mods).unwrap(), text: text.into(), action: KeyAction::Press }
}

#[test]
fn encode_key_follows_modes() {
    let mut term = new_term(80, 24, Callbacks::default());
    assert_eq!(term.encode_key(&key("ArrowUp", &[], "")).unwrap(), b"\x1b[A");
    term.write(b"\x1b[?1h"); // DECCKM application cursor keys
    assert_eq!(term.encode_key(&key("ArrowUp", &[], "")).unwrap(), b"\x1bOA");
    assert_eq!(term.encode_key(&key("KeyA", &[], "a")).unwrap(), b"a");
    assert_eq!(term.encode_key(&key("KeyC", &["ctrl"], "c")).unwrap(), b"\x03");
    assert_eq!(term.encode_key(&key("Enter", &[], "")).unwrap(), b"\r");
    term.write(b"\x1b[>1u"); // kitty keyboard: disambiguate
    assert_eq!(term.encode_key(&key("Escape", &[], "")).unwrap(), b"\x1b[27u");
    assert!(term.encode_key(&key("NoSuchKey", &[], "")).is_err());
    assert!(known_key("F12") && !known_key("f12"));
}

#[test]
fn parse_mods_names() {
    let both = parse_mods(&["ctrl", "shift"]).unwrap();
    let ctrl = parse_mods(&["ctrl"]).unwrap();
    let shift = parse_mods(&["shift"]).unwrap();
    assert_eq!(both.0, ctrl.0 | shift.0);
    assert_ne!(ctrl.0, 0);
    assert!(parse_mods(&["hyper"]).is_err());
}

#[test]
fn encode_paste_and_focus() {
    let mut term = new_term(80, 24, Callbacks::default());
    assert_eq!(term.encode_paste("a\nb").unwrap(), b"a\rb");
    term.write(b"\x1b[?2004h");
    assert_eq!(term.encode_paste("x\x1by").unwrap(), b"\x1b[200~x y\x1b[201~");
    // Longer than the first buffer guess.
    let long = "z".repeat(5000);
    assert_eq!(term.encode_paste(&long).unwrap().len(), 5000 + 12);
    assert!(term.encode_focus(true).unwrap().is_empty());
    term.write(b"\x1b[?1004h");
    assert_eq!(term.encode_focus(true).unwrap(), b"\x1b[I");
    assert_eq!(term.encode_focus(false).unwrap(), b"\x1b[O");
}

#[test]
fn merge_links_cases() {
    let open = |u: &str| format!("\x1b]8;;{u}\x1b\\");
    let cases = [
        (format!("{}x{}\r\n{}y{}", open("a"), open(""), open("a"), open("")), format!("{}x\r\ny{}", open("a"), open(""))),
        (
            format!("{}x{}\r\n{}y{}", open("a"), open(""), open("b"), open("")),
            format!("{}x{}\r\n{}y{}", open("a"), open(""), open("b"), open("")),
        ),
        (format!("{}x{}z{}y", open("a"), open(""), open("a")), format!("{}x{}z{}y", open("a"), open(""), open("a"))),
        (format!("{}x{}\r\n\x1b[1C{}y", open("a"), open(""), open("a")), format!("{}x\r\n\x1b[1Cy", open("a"))),
    ];
    for (input, want) in cases {
        assert_eq!(merge_links(&input), want, "{input:?}");
    }
}

#[test]
fn format_palette() {
    let mut term = new_term(10, 2, Callbacks::default());
    term.write(b"x");
    let vt = String::from_utf8_lossy(&term.format(Format::Vt, false, false).unwrap()).into_owned();
    let full = String::from_utf8_lossy(&term.format(Format::VtPalette, false, false).unwrap()).into_owned();
    assert!(!vt.contains("\x1b]4;") && full.contains("\x1b]4;255;"));
}

#[test]
fn snapshot_screen() {
    let mut src = Terminal::new(80, 10, Options { continuation: true, scrollback_bytes: 10 << 20, ..Default::default() }).unwrap();
    for i in 0..5000 {
        src.write(format!("line {i}\r\n").as_bytes());
    }
    src.write(b"\x1b[1;3"); // unfinished, so the continuation matters
    let full = src.snapshot().unwrap();
    let screen = src.snapshot_screen().unwrap();
    let end = ready_end(&full);
    assert_eq!(screen, full[..end], "screen snapshot is not the full one cut at READY");
    assert!(screen.len() < full.len() / 4, "history not skipped");
    let mut dec = Decoder::new().unwrap();
    dec.feed(&screen);
    let mut term = dec.ready(Callbacks::default()).unwrap();
    term.write(b"2mX"); // completes ESC[1;32m
    let text = term.text();
    assert!(text.contains("line 4999") && text.contains('X'), "{text}");
    assert_eq!(ready_end(b"short"), 5);
}

// The screen rows as styled VT, plus cursor and size: what a screen snapshot
// must preserve. Untrimmed VT output keeps blank rows, so its last rows
// segments are the screen whatever history precedes it.
fn visible(term: &Terminal) -> String {
    let vt = String::from_utf8_lossy(&term.format(Format::Vt, false, false).unwrap()).into_owned();
    // The preamble (modes, tab stops) ends with the cursor-home that starts
    // the rows; compare it separately so it can't hide in the first row.
    let (pre, body) = match vt.find("\x1b[H") {
        Some(i) => (&vt[..i], &vt[i + 3..]),
        None => ("", vt.as_str()),
    };
    let segs: Vec<&str> = body.split("\r\n").collect();
    let (cols, rows) = term.size();
    let segs = if segs.len() > rows { &segs[segs.len() - rows..] } else { &segs[..] };
    let (x, y) = term.cursor();
    format!("{cols}x{rows} {x},{y} alt={} pre={pre:?}\n{:?}", term.alt_screen(), segs.join("\n"))
}

fn restore_screen(snap: &[u8]) -> Terminal {
    let mut dec = Decoder::new().unwrap();
    dec.feed(snap);
    dec.ready(Callbacks::default()).unwrap()
}

#[test]
fn snapshot_screen_compact() {
    let mut src = Terminal::new(80, 24, Options { continuation: true, scrollback_bytes: 10 << 20, ..Default::default() }).unwrap();
    for i in 0..3000 {
        src.write(format!("\x1b[3{}m{i:05} request served in 12ms from cache, upstream healthy\x1b[0m\r\n", i % 8).as_bytes());
    }
    src.write(b"\x1b[?2004h\x1b[5;7Hcursor here");
    assert!(src.at_ground());
    let plain = src.snapshot_screen().unwrap();
    let compact = src.snapshot_screen_compact().unwrap();
    // Whether history shares the screen's page depends on page layout, so
    // the size win varies; it must never grow, and it is large here.
    assert!(compact.len() <= plain.len() && (plain.len() <= 16 << 10 || compact.len() * 4 <= plain.len()));
    let (mut a, mut b) = (restore_screen(&plain), restore_screen(&compact));
    assert_eq!(visible(&b), visible(&a));
    assert!(b.mode(2004, false), "bracketed paste mode lost");
    // Both continue identically.
    let more = b"\r\nnext\x1b[1mbold\r\n";
    a.write(more);
    b.write(more);
    assert_eq!(visible(&b), visible(&a));
    // Mid-sequence it falls back to the plain screen snapshot.
    src.write(b"\x1b[3");
    assert!(!src.at_ground());
    assert_eq!(src.snapshot_screen_compact().unwrap(), src.snapshot_screen().unwrap());
}

#[test]
fn set_callbacks_on_restored() {
    let mut src = new_term(10, 2, Callbacks::default());
    src.write(b"x");
    let mut t = restore(&src.snapshot().unwrap(), Callbacks::default()).unwrap();
    let bells = Rc::new(RefCell::new(0));
    let b = bells.clone();
    t.set_callbacks(Callbacks { bell: Some(Box::new(move || *b.borrow_mut() += 1)), ..Default::default() });
    t.write(b"\x07");
    assert_eq!(*bells.borrow(), 1);
}

// The pinned commit here and in the build script must agree.
#[test]
fn commit_matches_script() {
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/libghostty.sh")).unwrap();
    assert!(script.contains(&format!("\nGHOSTTY_COMMIT={COMMIT}\n")));
}

// active_text is the full plain format's active rows, wide characters,
// blanks, blank rows and soft-wrapped rows included, whatever the
// scrollback holds.
#[test]
fn active_text_is_the_active_rows() {
    let mut t = new_term(12, 4, Callbacks::default());
    let active = |t: &Terminal| -> Vec<String> {
        let full = String::from_utf8(t.format(Format::Plain, false, false).unwrap()).unwrap();
        let lines: Vec<String> = full.split('\n').map(String::from).collect();
        let (_, rows) = t.size();
        let mut want: Vec<String> = lines[t.scrollback_rows()..].to_vec();
        want.resize(rows, String::new());
        want
    };
    let got = |t: &Terminal| -> Vec<String> {
        let s = String::from_utf8(t.active_text().unwrap()).unwrap();
        let mut rows: Vec<String> = s.split('\n').map(String::from).collect();
        rows.resize(t.size().1, String::new());
        rows
    };
    for chunk in [&b"$ "[..], b"ls  ", b"\r\nold\r\n\r\n", b"wide \xe4\xbd\xa0\xe5\xa5\xbd!", b"\r\nmore\r\nlines\r\nthan\r\nrows\r\n$ x  ", b"\r\nsoft-wrapped past twelve \xe4\xbd\xa0 columns", b"\r\n$ "] {
        t.write(chunk);
        assert_eq!(got(&t), active(&t), "after {:?}", String::from_utf8_lossy(chunk));
    }
    assert!(t.scrollback_rows() > 0, "no scrollback to leave out");
}

fn image_term(cols: usize, rows: usize, cb: Callbacks) -> Terminal {
    let mut t = Terminal::new(cols, rows, Options { continuation: true, images: true, callbacks: cb, ..Default::default() }).unwrap();
    t.resize_cells(cols, rows, 10, 20).unwrap();
    t
}

// A 2x2 RGB image, transmitted and placed at the cursor as image 7.
const RGB_2X2: &[u8] = b"\x1b_Ga=T,f=24,s=2,v=2,i=7,q=2;AAAA////AAAA////\x1b\\";

fn png_2x1() -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, 2, 1);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    let mut w = e.write_header().unwrap();
    w.write_image_data(&[255, 0, 0, 0, 255, 0]).unwrap();
    drop(w);
    out
}

#[test]
fn notifications() {
    let got = Rc::new(RefCell::new(Vec::new()));
    let g = got.clone();
    let mut t = new_term(20, 5, Callbacks { notify: Some(Box::new(move |title, body| g.borrow_mut().push((title, body)))), ..Default::default() });
    t.write(b"\x1b]9;build done\x07\x1b]777;notify;CI;all green\x1b\\");
    assert_eq!(*got.borrow(), [("".to_string(), "build done".to_string()), ("CI".to_string(), "all green".to_string())]);
}

/// DSR 996 is answered from the callback, and not at all until it knows.
#[test]
fn color_scheme_reports() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let scheme = Rc::new(std::cell::Cell::new(None));
    let (r, s) = (replies.clone(), scheme.clone());
    let mut t = new_term(
        20,
        5,
        Callbacks {
            write_pty: Some(Box::new(move |b| r.borrow_mut().extend_from_slice(b))),
            color_scheme: Some(Box::new(move || s.get())),
            ..Default::default()
        },
    );
    t.write(b"\x1b[?996n");
    assert_eq!(String::from_utf8_lossy(&replies.borrow()), "", "answered without knowing");
    for (dark, want) in [(true, "\x1b[?997;1n"), (false, "\x1b[?997;2n")] {
        scheme.set(Some(dark));
        replies.borrow_mut().clear();
        t.write(b"\x1b[?996n");
        assert_eq!(String::from_utf8_lossy(&replies.borrow()), want);
    }
}

/// Default colors answer queries; what programs set goes over them, keeps
/// through new defaults, and through a snapshot.
#[test]
fn colors() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let r = replies.clone();
    let mut t = new_term(20, 5, Callbacks { write_pty: Some(Box::new(move |b| r.borrow_mut().extend_from_slice(b))), ..Default::default() });
    assert_eq!((t.colors().foreground, t.colors().background), (None, None));
    t.write(b"\x1b]11;?\x1b\\");
    assert_eq!(String::from_utf8_lossy(&replies.borrow()), "", "answered with no background");
    let mut d = t.default_colors();
    d.background = Some([0x10, 0x20, 0x30]);
    d.foreground = Some([0xee, 0xee, 0xee]);
    d.palette[1] = [0xcc, 0, 0];
    t.set_default_colors(&d).unwrap();
    t.write(b"\x1b]11;?\x1b\\\x1b]4;1;?\x1b\\");
    assert_eq!(String::from_utf8_lossy(&replies.borrow()), "\x1b]11;rgb:1010/2020/3030\x1b\\\x1b]4;1;rgb:cccc/0000/0000\x1b\\");
    t.write(b"\x1b]10;#ff0000\x1b\\\x1b]4;2;#00ff00\x1b\\");
    assert_eq!((t.colors().foreground, t.default_colors().foreground), (Some([0xff, 0, 0]), Some([0xee, 0xee, 0xee])));
    d.palette[2] = [1, 2, 3];
    t.set_default_colors(&d).unwrap();
    assert_eq!((t.colors().palette[2], t.default_colors().palette[2]), ([0, 0xff, 0], [1, 2, 3]), "a program's color was replaced");
    let back = restore(&t.snapshot().unwrap(), Callbacks::default()).unwrap();
    assert_eq!(back.colors(), t.colors(), "colors lost in a snapshot");
}

#[test]
fn pointer_shapes() {
    let mut t = new_term(20, 5, Callbacks::default());
    assert_eq!(t.pointer_shape(), "text");
    for (osc, want) in [("pointer", "pointer"), ("zoom-out", "zoom-out"), ("ew-resize", "ew-resize"), ("", "text")] {
        t.write(format!("\x1b]22;{osc}\x1b\\").as_bytes());
        assert_eq!(t.pointer_shape(), want, "OSC 22 {osc:?}");
    }
    t.write(b"\x1b]22;wait\x1b\\");
    let snap = t.snapshot().unwrap();
    assert_eq!(restore(&snap, Callbacks::default()).unwrap().pointer_shape(), "wait", "lost in a snapshot");
}

#[test]
fn images_off_by_default() {
    let mut t = new_term(20, 5, Callbacks::default());
    t.write(RGB_2X2);
    assert!(t.placements().is_empty() && t.image(7).is_none(), "an image was kept without images on");
}

#[test]
fn images_kept_and_placed() {
    let mut t = image_term(20, 5, Callbacks::default());
    assert_eq!(t.cell_size(), (10, 20));
    t.write(b"ab");
    t.write(RGB_2X2);
    let img = t.image(7).expect("image 7");
    assert_eq!((img.width, img.height, img.format), (2, 2, PixelFormat::Rgb));
    assert_eq!(img.pixels.map(|p| p.len()), Some(12));
    let p = t.placements();
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!((p[0].image, p[0].col, p[0].row, p[0].grid, p[0].visible), (7, 2, 0, (1, 1), true));
    // A placement moves the cursor past the image unless C=1.
    assert_eq!(t.cursor(), (3, 0));
    // Deleting placements alone keeps the image; clearing the screen
    // deletes both.
    t.write(b"\x1b_Ga=d,d=a,q=2\x1b\\");
    assert!(t.placements().is_empty());
    assert!(t.image(7).is_some(), "d=a deleted the image");
    t.write(b"\x1b_Ga=p,i=7,q=2\x1b\\");
    assert_eq!(t.placements().len(), 1);
    t.write(b"\x1b[2J");
    assert!(t.placements().is_empty() && t.image(7).is_none());
}

#[test]
fn images_decode_png() {
    let mut t = image_term(20, 5, Callbacks::default());
    t.write(format!("\x1b_Ga=T,f=100,i=9,q=2;{}\x1b\\", b64(&png_2x1())).as_bytes());
    let img = t.image(9).expect("png image");
    assert_eq!((img.width, img.height, img.format), (2, 1, PixelFormat::Rgba));
    assert_eq!(img.pixels, Some(&[255, 0, 0, 255, 0, 255, 0, 255][..]));
    t.write(b"\x1b_Ga=T,f=100,i=10,q=2;bm90IGEgcG5n\x1b\\");
    assert!(t.image(10).is_none(), "garbage decoded as a PNG");
}

/// A few bytes claiming 30000x30000 pixels are refused before 3.6 GB is
/// allocated for them.
#[test]
fn images_refuse_huge_png() {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    let mut png = png_2x1();
    // IHDR's width and height follow the signature, length and type.
    png[16..24].copy_from_slice(&[0, 0, 0x75, 0x30, 0, 0, 0x75, 0x30]);
    let sum = crc(&png[12..29]);
    png[29..33].copy_from_slice(&sum.to_be_bytes());
    let mut t = image_term(20, 5, Callbacks::default());
    t.write(format!("\x1b_Ga=T,f=100,i=12,q=2;{}\x1b\\", b64(&png)).as_bytes());
    assert!(t.image(12).is_none(), "a huge PNG was decoded");
}

/// A program can't make the terminal read a file: images come in the data.
#[test]
fn images_never_read_files() {
    let path = std::env::temp_dir().join(format!("osewa-image-{}.png", std::process::id()));
    std::fs::write(&path, png_2x1()).unwrap();
    let mut t = image_term(20, 5, Callbacks::default());
    for medium in ["f", "t"] {
        t.write(format!("\x1b_Ga=T,f=100,t={medium},i=11,q=2;{}\x1b\\", b64(path.to_str().unwrap().as_bytes())).as_bytes());
        assert!(t.image(11).is_none(), "read an image from a file (t={medium})");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn images_answer_queries() {
    let replies = Rc::new(RefCell::new(Vec::new()));
    let r = replies.clone();
    let mut t = image_term(20, 5, Callbacks { write_pty: Some(Box::new(move |b| r.borrow_mut().extend_from_slice(b))), ..Default::default() });
    t.write(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    assert_eq!(String::from_utf8_lossy(&replies.borrow()), "\x1b_Gi=31;OK\x1b\\");
}

#[test]
fn images_in_snapshots() {
    let mut t = image_term(20, 5, Callbacks::default());
    t.write(RGB_2X2);
    let snap = t.snapshot().unwrap();
    let full = restore(&snap, Callbacks::default()).unwrap();
    assert_eq!(full.cell_size(), (10, 20), "the cell size didn't survive a snapshot");
    assert_eq!(full.image(7).and_then(|i| i.pixels).map(|p| p.len()), Some(12));
    assert_eq!(full.placements(), t.placements());
    // Generations are the terminal's own, never another's.
    assert_ne!(full.image(7).unwrap().generation, t.image(7).unwrap().generation);
    // Through READY only, the image and its placement come without pixels.
    let screen = restore_screen(&t.snapshot_screen().unwrap());
    assert!(screen.image(7).expect("image in a screen snapshot").pixels.is_none());
    assert_eq!(screen.placements(), t.placements());
}

fn b64(b: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, &x)| n | (x as u32) << (16 - 8 * i));
        for i in 0..4 {
            s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}
