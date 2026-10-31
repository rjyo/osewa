//! Drawing the mirror, predictions over it, and status notices.

use std::sync::Arc;

use osewa_ghostty::{Format, Options, Terminal};
use osewa_mirror::{CellState, PredictMode, Prediction, Predictor, PredictorConfig};
use osewa_wire as wire;

use super::*;
use crate::render::*;
use crate::ui::Ui;
use crate::vtfilter::QueryFilter;

fn term(cols: usize, rows: usize) -> Terminal {
    Terminal::new(cols, rows, Options::default()).unwrap()
}

/// A terminal's active area and cursor, for comparing what the user sees
/// with the mirror.
fn screen_of(t: &Terminal) -> String {
    let b = t.format(Format::Plain, false, false).unwrap();
    let s = String::from_utf8_lossy(&b).into_owned();
    let lines: Vec<&str> = s.split('\n').collect();
    let lines: Vec<&str> = lines[t.scrollback_rows().min(lines.len())..].iter().map(|l| l.trim_end_matches(' ')).collect();
    let (x, y) = t.cursor();
    format!("{}\ncursor {x},{y} alt={}", lines.join("\n").trim_end_matches('\n'), t.alt_screen())
}

fn screens() -> Vec<(&'static str, String)> {
    vec![
        ("empty", String::new()),
        ("prompt", "$ ls\r\nfile\r\n$ ".into()),
        ("styles", "\x1b[1;31m你好\x1b[0m plain \x1b[42mgreen\r\nacross\x1b[0m\r\n\x1b[4munder".into()),
        ("sparse", "\x1b[3;7Hmid\x1b[1;2Htop".into()),
        ("scroll", "line\r\n".repeat(30) + "last"),
        ("wrap", "0123456789012345678901234567890\r\n".into()),
        ("alt", "primary\r\n\x1b[?1049h\x1b[2;3Halt text".into()),
    ]
}

#[test]
fn screen_redraw_reproduces_the_mirror() {
    for (name, input) in screens() {
        let mut mirror = term(12, 5);
        mirror.write(input.as_bytes());
        let mut local = term(12, 5);
        local.write(b"junk\r\n\x1b[7mmore junk everywhere\x1b[0m");
        if input.contains("1049h") {
            local.write(b"\x1b[?1049h");
        }
        local.write(&screen_redraw(&mirror));
        assert_eq!(screen_of(&local), screen_of(&mirror), "{name}");
    }
}

#[test]
fn attach_redraw_carries_it_all() {
    for (name, input) in screens() {
        let mut mirror = term(12, 5);
        mirror.write(input.as_bytes());
        let out = attach_redraw(&mirror);
        assert!(!out.windows(4).any(|w| w == b"\x1b]4;"), "{name}: palette sent to the local terminal");
        let mut local = term(12, 5);
        local.write(&out);
        assert_eq!(screen_of(&local), screen_of(&mirror), "{name}");
        if mirror.scrollback_rows() > 0 {
            assert!(local.scrollback_rows() >= mirror.scrollback_rows(), "{name}: scrollback not carried over");
        }
    }
}

#[test]
fn snapshot_redraw_resets_modes() {
    let mut mirror = term(12, 5);
    mirror.write(("x\r\n".repeat(20) + "\x1b[?2004hnow").as_bytes());
    let mut local = term(12, 5);
    local.write(b"\x1b[?1049h\x1b[?1000hstale");
    local.write(&snapshot_redraw(&mirror));
    assert!(!local.alt_screen() && !local.mode(1000, false) && local.mode(2004, false), "modes not brought in line with the mirror");
    assert!(screen_of(&local) == screen_of(&mirror) && local.scrollback_rows() == 0, "got {:?}, scrollback {}", screen_of(&local), local.scrollback_rows());
}

#[test]
fn vt_parts_rejects_other_shapes() {
    assert!(vt_parts(b"no home").is_none());
    assert!(vt_parts(b"\x1b[Hno cursor").is_none());
    assert!(attach_redraw(&term(4, 2)).starts_with(REDRAW));
}

#[test]
fn notice_and_overlay_draw() {
    let mut mt = term(10, 3);
    mt.write(b"$ ");
    let mut local = term(10, 3);
    local.write(b"$ ");
    local.write(&notice_draw(&mt, "a very long notice", b"\x1b[0m"));
    let s = screen_of(&local);
    assert!(s.contains("a very lo\n") && s.ends_with("cursor 2,0 alt=false"), "notice: {s:?}");
    let p = |x, rune, wide, state| Prediction { x, y: 0, rune, wide, state };
    assert!(overlay_draw(&[p(2, 'q', false, CellState::Confirmed)], b"").is_empty(), "drew a confirmed cell");
    local.write(&overlay_draw(&[p(2, 'q', false, CellState::Tentative), p(3, '你', true, CellState::Tentative)], b"\x1b[0m"));
    assert!(screen_of(&local).starts_with("$ q你"), "overlay: {:?}", screen_of(&local));
}

/// Drives a Ui the way a session would, and replays everything it writes
/// into a terminal standing in for the user's.
struct Harness {
    mirror: Terminal,
    local: Terminal,
    out: Buf,
    u: Ui,
    now: Arc<Mutex<Instant>>,
}

impl Harness {
    fn new() -> Harness {
        let out = Buf::default();
        let now = Arc::new(Mutex::new(Instant::now()));
        let n = now.clone();
        let pred = Predictor::new(PredictorConfig {
            mode: PredictMode::Always,
            now: Some(Arc::new(move || *n.lock().unwrap())),
            ..Default::default()
        });
        let mut h = Harness { mirror: term(20, 4), local: term(20, 4), out: out.clone(), u: Ui::new(Box::new(out), false, pred), now };
        h.u.ready(&h.mirror);
        h.u.event(&wire::EventMsg { echo: Some(true), canonical: Some(true), ..wire::EventMsg::kind(wire::EVENT_INPUT_MODE) }, true);
        h.sync();
        h
    }

    /// Applies remote output to the mirror, as the session does.
    fn output(&mut self, s: &str) {
        self.mirror.write(s.as_bytes());
        self.u.output(s.as_bytes());
        self.u.applied(&self.mirror);
        self.sync();
    }

    /// Predicts input, as the UI's send does without the network.
    fn typed(&mut self, s: &str) {
        self.u.pred.input(&self.mirror, s.as_bytes());
        self.u.paint(&self.mirror);
        self.sync();
    }

    fn tick(&mut self) {
        self.u.tick(&self.mirror);
        self.sync();
    }

    fn advance(&self, d: Duration) {
        *self.now.lock().unwrap() += d;
    }

    fn sync(&mut self) {
        let b = std::mem::take(&mut *self.out.0.lock().unwrap());
        self.local.write(&b);
    }

    #[track_caller]
    fn matches(&self) {
        assert_eq!(screen_of(&self.local), screen_of(&self.mirror), "local screen differs from the mirror");
    }

    /// Types "ab" and has the mirror echo "a", so "b" is on screen as a
    /// prediction of a confirmed epoch.
    #[track_caller]
    fn drawn(&mut self) {
        self.output("$ ");
        self.typed("ab");
        self.output("a");
        assert!(screen_of(&self.local).starts_with("$ ab"), "prediction not drawn: {:?}", screen_of(&self.local));
    }

    /// What the user sees, styles and cursor included.
    fn styled(&self) -> (String, String) {
        (String::from_utf8_lossy(&screen_redraw(&self.local)).into(), String::from_utf8_lossy(&screen_redraw(&self.mirror)).into())
    }
}

#[test]
fn ui_predictions_confirmed() {
    let mut h = Harness::new();
    h.output("$ ");
    h.typed("ls");
    assert_eq!(screen_of(&h.local), screen_of(&h.mirror), "drew before the epoch was confirmed");
    h.output("l");
    assert!(screen_of(&h.local).starts_with("$ ls"), "prediction not drawn: {:?}", screen_of(&h.local));
    h.output("s");
    h.matches();
}

#[test]
fn ui_miss_is_repainted() {
    let mut h = Harness::new();
    h.output("$ ");
    h.typed("abc");
    h.output("Z"); // the program shows something else
    h.matches();
}

#[test]
fn ui_miss_on_alt_screen_repaints_after() {
    let mut h = Harness::new();
    h.output("$ ");
    h.typed("vi");
    h.output("\x1b[?1049h\x1b[Hediting"); // the guess is dropped on the alt screen
    h.matches();
    h.output("\x1b[?1049l");
    h.matches();
}

#[test]
fn ui_later_snapshots_redraw_only_the_screen() {
    let mut h = Harness::new();
    h.output(&("old\r\n".repeat(10) + "$ "));
    h.mirror.write(b"\x1b[2Jfresh");
    h.u.ready(&h.mirror);
    // Neither resend the history nor clear the user's own scrollback.
    let out = h.out.string();
    assert!(!out.contains("old") && !out.contains("\x1b[3J"), "a resnapshot redrew the scrollback: {out:?}");
    h.sync();
    h.matches();
}

#[test]
fn owner_name() {
    for (input, want) in [("", "nobody"), ("laptop-0a1b2c", "laptop"), ("my-mac-ff00", "my-mac"), ("plain", "plain")] {
        assert_eq!(crate::commands::owner_name(input), want, "{input:?}");
    }
}

/// A catch-up screen snapshot replaces the mirror; predictions carry over
/// and are checked against it.
#[test]
fn ui_predictions_survive_screen_snapshot() {
    let mut h = Harness::new();
    h.output("$ ");
    h.typed("ab");
    let mut next = term(20, 4);
    next.write(b"$ a"); // the echo of a made it, b not yet
    h.mirror = next;
    h.u.ready(&h.mirror);
    h.u.applied(&h.mirror);
    h.sync();
    assert!(screen_of(&h.local).starts_with("$ ab"), "prediction lost on a new mirror: {:?}", screen_of(&h.local));
    h.output("b");
    h.matches();
}

// The rendering findings of the prediction review.

/// #4: a prediction whose echo never comes is taken down by the tick, with
/// no output arriving.
#[test]
fn ui_tick_times_out_predictions() {
    let mut h = Harness::new();
    h.drawn();
    h.advance(Duration::from_secs(2));
    h.tick();
    h.matches();
}

/// #5: a frame ending inside a UTF-8 character must not have an overlay or
/// cursor move written into the middle of it.
#[test]
fn ui_overlay_does_not_split_utf8() {
    let mut f = QueryFilter::default();
    assert_eq!(f.filter(b"ab\xe4\xb8"), b"ab", "partial rune passed");
    assert_eq!(f.filter(b"\xadcd"), "中cd".as_bytes(), "rune not completed");
    assert_eq!(f.filter(b"\xe4\x1b[1m"), "\u{fffd}\x1b[1m".as_bytes(), "broken rune not replaced");

    let mut h = Harness::new();
    h.drawn();
    // Raw bytes, as the mirror and the UI get them.
    for chunk in [&b"\r\n\xe4\xb8"[..], b"\xadcd"] {
        h.mirror.write(chunk);
        h.u.output(chunk);
        h.u.applied(&h.mirror);
        h.sync();
    }
    h.advance(Duration::from_secs(2));
    h.tick();
    h.matches();
}

/// #6: a repaint must not scroll inside the remote's scroll region.
#[test]
fn screen_redraw_under_scroll_region() {
    let setup = "\x1b[6;1Hstatus line\x1b[1;5r\x1b[H".to_string() + &"output line\r\n".repeat(8);
    let (mut mirror, mut local) = (term(20, 6), term(20, 6));
    mirror.write(setup.as_bytes());
    local.write(setup.as_bytes());
    local.write(&screen_redraw(&mirror));
    mirror.write(b"after\r\n");
    local.write(b"after\r\n");
    assert_eq!(screen_of(&local), screen_of(&mirror));
    // Origin mode too: rows are relative to the region.
    mirror.write(b"\x1b[?6h\x1b[2;1Hin region");
    local.write(b"\x1b[?6h\x1b[2;1Hin region");
    local.write(&screen_redraw(&mirror));
    mirror.write(b"\x1b[1;1Hhome");
    local.write(b"\x1b[1;1Hhome");
    assert_eq!(screen_of(&local), screen_of(&mirror), "origin mode");
}

/// #8: drawing an overlay keeps the remote's style, and keeps a pending
/// wrap at the last column.
#[test]
fn ui_overlay_keeps_style() {
    let mut h = Harness::new();
    h.output("\x1b[4;31m"); // the remote underlines (and colors) the input
    h.drawn();
    h.output("bc");
    let (l, m) = h.styled();
    assert_eq!(l, m, "style lost");
}

#[test]
fn ui_overlay_keeps_pending_wrap() {
    let mut h = Harness::new();
    h.drawn();
    h.output(&("\r\n".to_string() + &"w".repeat(20))); // the cursor waits to wrap
    h.output("Z");
    h.advance(Duration::from_secs(2));
    h.tick();
    h.matches();
}

/// #10: the cursor shows after the predicted text, and real output still
/// lands where the mirror's cursor is.
#[test]
fn ui_cursor_at_prediction() {
    let mut h = Harness::new();
    h.drawn();
    assert_eq!(h.local.cursor(), (4, 0), "cursor, want after the prediction");
    h.output("b");
    h.matches();
    h.typed("cd");
    h.output("c");
    h.output("\r\nmore");
    h.advance(Duration::from_secs(2));
    h.tick();
    h.matches();
}

/// Counts the writes a Ui makes: each costs a syscall, and a wakeup of the
/// terminal that reads them.
#[derive(Clone, Default)]
struct Writes(Arc<std::sync::Mutex<Vec<Vec<u8>>>>);

impl std::io::Write for Writes {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().push(b.to_vec());
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An echo with a prediction over it, and a keystroke's overlay and cursor
/// move, each go out in one write.
#[test]
fn ui_writes_once_per_frame() {
    let w = Writes::default();
    let now = Instant::now();
    let pred = Predictor::new(PredictorConfig { mode: PredictMode::Always, now: Some(Arc::new(move || now)), ..Default::default() });
    let mut u = Ui::new(Box::new(w.clone()), false, pred);
    let mut mirror = term(20, 4);
    u.ready(&mirror);
    u.event(&wire::EventMsg { echo: Some(true), canonical: Some(true), ..wire::EventMsg::kind(wire::EVENT_INPUT_MODE) }, true);
    let frame = |u: &mut Ui, mirror: &mut Terminal, s: &str| {
        w.0.lock().unwrap().clear();
        mirror.write(s.as_bytes());
        u.output(s.as_bytes());
        u.applied(mirror);
        w.0.lock().unwrap().len()
    };
    assert_eq!(frame(&mut u, &mut mirror, "$ "), 1, "writes for a frame");
    w.0.lock().unwrap().clear();
    u.pred.input(&mirror, b"ab");
    u.paint(&mirror);
    frame(&mut u, &mut mirror, "a"); // confirms the epoch: b is drawn from now on
    w.0.lock().unwrap().clear();
    u.pred.input(&mirror, b"c");
    u.paint(&mirror);
    let typed = w.0.lock().unwrap().clone();
    assert!(typed.len() == 1 && String::from_utf8_lossy(&typed[0]).contains("\x1b[4mc"), "writes for a keystroke: {typed:?}");
    assert_eq!(frame(&mut u, &mut mirror, "b"), 1, "writes for an echo under predictions");
}

/// A live blocked or done status shows
/// a notice, unless quiet, typing, or from a catch-up.
#[test]
fn status_notice() {
    let blocked = wire::EventMsg { state: "blocked".into(), app: "claude-code".into(), msg: "Apply?".into(), ..wire::EventMsg::kind(wire::EVENT_STATUS) };
    let done = wire::EventMsg { state: "done".into(), ..wire::EventMsg::kind(wire::EVENT_STATUS) };
    let working = wire::EventMsg { state: "working".into(), progress: Some(5), ..wire::EventMsg::kind(wire::EVENT_STATUS) };
    for (name, e, live, quiet, typing, want) in [
        ("blocked", &blocked, true, false, false, true),
        ("done", &done, true, false, false, true),
        ("working", &working, true, false, false, false),
        ("catch-up", &blocked, false, false, false, false),
        ("quiet", &blocked, true, true, false, false),
        ("typing", &blocked, true, false, true, false),
    ] {
        let mut h = Harness::new();
        h.output("$ ");
        h.u.quiet = quiet;
        if typing {
            h.u.typed_at = Some(Instant::now());
        }
        h.u.event(e, live);
        h.u.applied(&h.mirror);
        h.sync();
        // The harness is 20 columns: the notice is cut to fit.
        let s = screen_of(&h.local);
        let got = s.contains(" claude-code: block") || s.contains(" program: done");
        assert_eq!(got, want, "{name}: notice:\n{s}");
    }
}
