//! Ported from client/mirror/predict_test.go and predict_review_test.go.
//! Go's fuzz targets run here over their seed corpus plus deterministic
//! pseudo-random cases.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use osewa_ghostty::{self as ghostty, Terminal};

use super::*;

#[derive(Clone)]
struct TestClock(Arc<Mutex<Instant>>);

impl TestClock {
    fn new() -> TestClock {
        TestClock(Arc::new(Mutex::new(Instant::now())))
    }
    fn add(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
    fn clock(&self) -> Clock {
        let c = self.0.clone();
        Arc::new(move || *c.lock().unwrap())
    }
}

fn term(cols: usize, rows: usize) -> Terminal {
    Terminal::new(cols, rows, ghostty::Options::default()).unwrap()
}

/// A predictor that shows predictions, over a 20x5 mirror showing a shell
/// prompt.
fn predictor_at() -> (Predictor, Terminal, TestClock) {
    let mut t = term(20, 5);
    t.write(b"$ ");
    let clk = TestClock::new();
    let mut p = Predictor::new(PredictorConfig {
        now: Some(clk.clock()),
        ..Default::default()
    });
    p.input_mode(InputMode {
        known: true,
        echo: true,
        canonical: true,
    });
    p.rtt(Duration::from_millis(100));
    // As if an earlier keystroke's echo had confirmed this epoch; the epoch
    // rule has tests of its own.
    p.confirmed = true;
    (p, t, clk)
}

fn overlay(p: &mut Predictor) -> String {
    p.overlay()
        .iter()
        .filter(|c| c.state == CellState::Tentative)
        .map(|c| c.rune)
        .collect()
}

fn pred(x: i32, y: i32, rune: char) -> Prediction {
    Prediction {
        x,
        y,
        rune,
        ..Default::default()
    }
}

/// Predictions, drawn or not.
fn pending_at(p: &Predictor) -> Vec<Prediction> {
    p.preds.iter().map(|pr| pr.p).collect()
}

#[test]
fn predict_typing_confirmed() {
    let (mut p, mut t, _) = predictor_at();
    p.input(&t, b"ls");
    let ov = p.overlay();
    assert!(
        ov.len() == 2 && ov[0] == pred(2, 0, 'l') && ov[1].x == 3,
        "overlay = {ov:?}"
    );
    t.write(b"l");
    p.check(&t);
    assert_eq!(overlay(&mut p), "s", "after echo of l");
    let states = p.overlay();
    assert_eq!(
        states[0].state,
        CellState::Confirmed,
        "l not confirmed: {states:?}"
    );
    t.write(b"s");
    p.check(&t);
    assert!(
        p.overlay().is_empty() && !p.take_repaint(),
        "confirmed predictions linger or ask for a repaint"
    );
    // Typing continues from the mirror's cursor.
    p.input(&t, b" ");
    let ov = p.overlay();
    assert!(ov.len() == 1 && ov[0].x == 4, "overlay = {ov:?}");
}

#[test]
fn predict_hidden_below_min_rtt() {
    let t = term(20, 5);
    let mut p = Predictor::new(PredictorConfig::default());
    p.input_mode(InputMode {
        known: true,
        echo: true,
        ..Default::default()
    });
    p.confirmed = true;
    p.input(&t, b"a");
    assert!(p.overlay().is_empty(), "shown without an RTT sample");
    p.rtt(Duration::from_millis(10));
    assert!(p.overlay().is_empty(), "shown at 10 ms");
    for _ in 0..40 {
        p.rtt(Duration::from_millis(80));
    }
    assert_eq!(overlay(&mut p), "a", "not shown at 80 ms");

    let mut always = Predictor::new(PredictorConfig {
        mode: PredictMode::Always,
        ..Default::default()
    });
    always.input_mode(InputMode {
        known: true,
        echo: true,
        ..Default::default()
    });
    always.confirmed = true;
    always.input(&t, b"b");
    assert_eq!(overlay(&mut always), "b", "always mode hides predictions");
    let mut never = Predictor::new(PredictorConfig {
        mode: PredictMode::Never,
        ..Default::default()
    });
    never.input_mode(InputMode {
        known: true,
        echo: true,
        ..Default::default()
    });
    never.input(&t, b"b");
    assert!(never.overlay().is_empty(), "never mode predicts");
}

#[test]
fn predict_off_without_echo() {
    let (mut p, t, _) = predictor_at();
    p.input_mode(InputMode {
        known: true,
        echo: false,
        canonical: true,
    }); // password prompt
    p.input(&t, b"secret");
    assert!(p.overlay().is_empty(), "predicted with echo off");
    let mut unknown = Predictor::new(PredictorConfig {
        mode: PredictMode::Always,
        ..Default::default()
    });
    unknown.input(&t, b"x");
    assert!(
        unknown.overlay().is_empty(),
        "predicted before any input_mode"
    );
}

#[test]
fn predict_off_on_alt_screen() {
    let (mut p, mut t, _) = predictor_at();
    p.input(&t, b"ab");
    assert_eq!(overlay(&mut p), "ab");
    t.write(b"\x1b[?1049h");
    p.check(&t);
    assert!(
        p.overlay().is_empty() && p.take_repaint(),
        "predictions kept on the alternate screen"
    );
    p.input(&t, b"c");
    assert!(p.overlay().is_empty(), "predicted on the alternate screen");
}

#[test]
fn predict_stops_at_enter() {
    let (mut p, mut t, clk) = predictor_at();
    p.input(&t, b"a\rb");
    assert_eq!(overlay(&mut p), "a");
    p.input(&t, b"c");
    assert_eq!(overlay(&mut p), "a", "predicted past Enter");
    // The echo arrives and the prompt moves down: predicting resumes there.
    t.write(b"a\r\n$ ");
    p.check(&t);
    p.input(&t, b"d");
    let ov = pending_at(&p);
    assert!(
        ov.len() == 1 && ov[0] == pred(2, 1, 'd'),
        "predicted {ov:?}"
    );
    // Other control keys block too, until the cursor moves or time passes.
    let (mut p2, t2, _) = predictor_at();
    p2.input(&t2, b"\x03x");
    assert!(p2.overlay().is_empty(), "predicted after ^C");
    clk.add(Duration::from_secs(2));
}

#[test]
fn predict_unblocks_after_timeout() {
    let (mut p, t, clk) = predictor_at();
    p.input(&t, b"\t");
    p.input(&t, b"x");
    assert!(p.overlay().is_empty(), "predicted after tab");
    clk.add(Duration::from_secs(2));
    p.check(&t);
    p.input(&t, b"x");
    assert_eq!(p.preds.len(), 1, "still blocked after the timeout");
}

#[test]
fn predict_wide() {
    let (mut p, mut t, _) = predictor_at();
    p.input(&t, "你a".as_bytes());
    let ov = p.overlay();
    assert!(
        ov.len() == 2 && ov[0].wide && ov[0].x == 2 && ov[1].x == 4,
        "overlay = {ov:?}"
    );
    t.write("你a".as_bytes());
    p.check(&t);
    assert!(
        overlay(&mut p).is_empty() && !p.take_repaint(),
        "wide echo not confirmed"
    );
    // Backspace over a wide character erases both cells' worth.
    p.input(&t, b"\x7f\x7f");
    let ov = p.overlay();
    assert!(
        ov.len() == 2 && ov[1].x == 2 && ov[1].rune == ' ',
        "overlay = {ov:?}"
    );
}

#[test]
fn predict_backspace() {
    let (mut p, mut t, _) = predictor_at();
    t.write(b"abc");
    p.input(&t, b"\x7f");
    let ov = p.overlay();
    assert!(
        ov.len() == 1 && ov[0] == pred(4, 0, ' '),
        "overlay = {ov:?}"
    );
    t.write(b"\x08 \x08");
    p.check(&t);
    assert!(
        p.overlay().is_empty() && !p.take_repaint(),
        "erase not confirmed"
    );
    // Erasing walks left to column 0, then stops: like mosh, a guess over
    // the prompt is a miss the mirror corrects.
    p.input(&t, b"\x7f\x7f\x7f\x7f\x7fq");
    let ov = p.overlay();
    assert!(
        ov.len() == 4 && ov[3] == pred(0, 0, ' '),
        "overlay = {ov:?}"
    );
}

#[test]
fn predict_arrows() {
    let line_editor = InputMode {
        known: true,
        ..Default::default()
    }; // arrows only move outside canonical mode
    let (mut p, mut t, _) = predictor_at();
    p.input_mode(line_editor);
    t.write(b"abc");
    // Left then type: mid-line insertion is not predicted.
    p.input(&t, b"\x1b[Dx");
    assert!(p.overlay().is_empty(), "predicted a mid-line insert");
    let (mut p2, mut t2, _) = predictor_at();
    p2.input_mode(line_editor);
    t2.write(b"abc\x08\x08");
    p2.input(&t2, b"\x1bOC\x1b[C"); // right twice reaches the end
    p2.input(&t2, b"\x1b[Cz"); // right at the end stays put
    let ov = p2.overlay();
    assert!(ov.len() == 1 && ov[0].x == 5, "overlay = {ov:?}");
    let (mut p3, t3, _) = predictor_at();
    p3.input_mode(line_editor);
    p3.input(&t3, b"\x1b[A"); // up: history, unpredictable
    p3.input(&t3, b"q");
    assert!(p3.overlay().is_empty(), "predicted after up arrow");
}

#[test]
fn predict_line_end() {
    let (mut p, t, _) = predictor_at();
    p.input(&t, b"0123456789012345678"); // 2 + 19 = past column 19
    assert_eq!(
        overlay(&mut p).chars().count(),
        17,
        "predicted cells up to the last column"
    );
}

#[test]
fn predict_miss_and_backoff() {
    let (mut p, mut t, _) = predictor_at();
    for i in 0..3 {
        // Each round: an echo confirms the epoch, the next guess shows, and
        // the program prints something else there.
        p.input(&t, b"ok");
        t.write(b"o");
        p.check(&t);
        assert_eq!(overlay(&mut p), "k", "round {i}: hidden too early");
        t.write(b"X");
        p.check(&t);
        assert!(
            p.overlay().is_empty() && p.take_repaint(),
            "round {i}: miss kept or not repainted"
        );
        assert_eq!(p.misses, i + 1, "round {i}: misses");
        t.write(b"\x1b[2J\x1b[H$ "); // a fresh prompt, no scrolling
        p.check(&t);
    }
    // Three misses: still predicting, but hidden until a confirmation.
    p.input(&t, b"bc");
    t.write(b"b");
    p.check(&t);
    assert_eq!(p.misses, 0, "a confirmation didn't reset the backoff");
    assert_eq!(overlay(&mut p), "c", "not shown again after a confirmation");
}

#[test]
fn predict_timeout() {
    let (mut p, t, clk) = predictor_at();
    p.input(&t, b"a");
    let _ = p.overlay();
    clk.add(Duration::from_millis(500));
    p.check(&t);
    assert_eq!(overlay(&mut p), "a", "dropped before the timeout");
    clk.add(Duration::from_millis(600));
    p.check(&t);
    assert!(
        p.overlay().is_empty() && p.take_repaint(),
        "kept past the timeout"
    );
}

/// The deadline is exactly when check drops an unechoed guess, so a caller
/// can sleep until it instead of polling.
#[test]
fn predict_deadline() {
    let (mut p, mut t, clk) = predictor_at();
    assert_eq!(p.deadline(), None, "a deadline with nothing pending");
    let start = (p.now)();
    p.input(&t, b"ab");
    clk.add(Duration::from_millis(400));
    p.input(&t, b"c");
    assert_eq!(p.deadline(), Some(start + Duration::from_secs(1)), "not the oldest guess's timeout");
    t.write(b"ab");
    p.check(&t);
    assert_eq!(
        p.deadline(),
        Some(start + Duration::from_millis(1400)),
        "confirmed guesses still count"
    );
    clk.add(Duration::from_millis(999));
    p.check(&t);
    assert_eq!(overlay(&mut p), "c", "dropped before its deadline");
    clk.add(Duration::from_millis(1));
    p.check(&t);
    assert!(p.overlay().is_empty() && p.deadline().is_none(), "kept at its deadline");
}

#[test]
fn predict_barrier_miss_is_free() {
    let (mut p, mut t, _) = predictor_at();
    for _ in 0..5 {
        p.input(&t, b"a\r");
        t.write(b"\x1b[HZZZ"); // the screen moved under the guess
        p.check(&t);
        t.write(b"\x1b[2J\x1b[H$ ");
        p.check(&t);
    }
    assert_eq!(p.misses, 0, "misses behind Enter counted toward backoff");
}

#[test]
fn predict_reset() {
    let (mut p, t, _) = predictor_at();
    p.input(&t, b"a\r");
    p.overlay();
    p.reset();
    assert!(
        p.overlay().is_empty() && !p.take_repaint(),
        "reset kept state"
    );
    p.input(&t, b"b");
    assert!(
        p.preds.len() == 1 && p.overlay().is_empty(),
        "after reset: blocked, or drawn before a confirmation"
    );
}

#[test]
fn parse_modes() {
    for (s, want) in [
        ("auto", PredictMode::Auto),
        ("always", PredictMode::Always),
        ("never", PredictMode::Never),
    ] {
        assert_eq!(parse_predict_mode(s), Ok(want), "{s}");
    }
    assert!(
        parse_predict_mode("sometimes").is_err(),
        "bad mode accepted"
    );
}

#[test]
fn cluster_widths() {
    for (s, w) in [
        ("a", 1),
        ("é", 1),
        ("你", 2),
        ("한", 2),
        ("😀", 2),
        ("Ｗ", 2),
        ("e\u{301}", 1),
        ("⚡", 2),
    ] {
        assert_eq!(cluster_width(s), w, "{s:?}");
    }
    let got = clusters("a\u{301}b你👨\u{200d}👩!");
    assert_eq!(got.join("|"), "a\u{301}|b|你|👨\u{200d}👩|!");
}

/// Deterministic pseudo-random bytes for the fuzz targets' extra cases.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }
    fn bytes(&mut self, alphabet: &[u8], max: usize) -> Vec<u8> {
        let n = self.next() as usize % max;
        (0..n)
            .map(|_| alphabet[self.next() as usize % alphabet.len()])
            .collect()
    }
}

/// Bytes a fuzzer would favor: text, controls, escapes, UTF-8 pieces.
const ALPHABET: &[u8] =
    b"abc xyz\r\n\x08\x7f\x1b[DCOA?1049hl;5\xe4\xbd\xa0\xf0\x9f\x98\x80\x03\t2J";

fn fuzz_cases(seeds: &[(&[u8], &[u8], u8)], n: u64) -> Vec<(Vec<u8>, Vec<u8>, u8)> {
    let mut cases: Vec<_> = seeds
        .iter()
        .map(|(a, b, s)| (a.to_vec(), b.to_vec(), *s))
        .collect();
    let mut r = Lcg(0x05e3a);
    for _ in 0..n {
        cases.push((r.bytes(ALPHABET, 40), r.bytes(ALPHABET, 60), r.next()));
    }
    cases
}

fn chunk(b: &[u8], i: usize, n: usize) -> &[u8] {
    &b[b.len() * i / n..b.len() * (i + 1) / n]
}

/// Predictions never change the mirror, and the overlay stays on screen,
/// whatever the input, output and timing (Go's FuzzPredictor).
#[test]
fn fuzz_predictor() {
    let seeds: &[(&[u8], &[u8], u8)] = &[
        (b"ls -la\r", b"ls -la\r\n$ ", 3),
        (
            "你好\x7f\x1b[Dx".as_bytes(),
            "你好\x08\x08  \x08\x08".as_bytes(),
            9,
        ),
        (b"\x1b[?1049h", b"\x1b[?1049hfoo\x1b[?1049l", 0),
    ];
    for (input, out, steps) in fuzz_cases(seeds, 300) {
        let mut mirror = term(12, 4);
        let mut reference = term(12, 4);
        let clk = TestClock::new();
        let mut p = Predictor::new(PredictorConfig {
            mode: PredictMode::Always,
            now: Some(clk.clock()),
            ..Default::default()
        });
        p.input_mode(InputMode {
            known: true,
            echo: true,
            ..Default::default()
        });
        let n = (steps % 8) as usize + 1;
        for i in 0..n {
            let (a, b) = (chunk(&input, i, n), chunk(&out, i, n));
            p.input(&mirror, a);
            mirror.write(b);
            reference.write(b);
            clk.add(Duration::from_millis(300) * i as u32);
            p.check(&mirror);
            let (cols, rows) = mirror.size();
            for c in p.overlay() {
                assert!(
                    !(c.x < 0
                        || c.y < 0
                        || c.x >= cols as i32
                        || c.y >= rows as i32
                        || (c.wide && c.x + 1 >= cols as i32)),
                    "overlay cell off screen: {c:?} for {input:?} {out:?} {steps}"
                );
            }
            p.take_repaint();
        }
        let got = mirror.format(ghostty::Format::Vt, false, false).unwrap();
        let want = reference.format(ghostty::Format::Vt, false, false).unwrap();
        assert_eq!(
            got, want,
            "predictor changed the mirror for {input:?} {out:?} {steps}"
        );
    }
}

/// Predictions survive a mirror replaced by a screen snapshot: they are
/// checked against the new mirror, confirmed where it shows the echo.
#[test]
fn predictor_rebases_on_new_mirror() {
    let (mut p, old, _) = predictor_at();
    p.input(&old, b"ab");
    let mut next = term(20, 5);
    next.write(b"$ a");
    p.check(&next);
    assert_eq!(overlay(&mut p), "b", "overlay on the new mirror");
    next.write(b"b");
    p.check(&next);
    assert!(
        p.overlay().is_empty() && !p.take_repaint(),
        "not confirmed on the new mirror"
    );
}

// From predict_review_test.go: the findings in .cache/reviews/predict.md.

/// #1: after Enter a new epoch draws nothing until the mirror confirms one
/// of its predictions, so a password typed at a prompt that doesn't echo is
/// never shown, even before input_mode says echo is off.
#[test]
fn predict_epoch_hides_unechoed_input() {
    let (mut p, mut t, clk) = predictor_at();
    p.input(&t, b"sudo ls");
    t.write(b"sudo ls");
    p.check(&t);
    p.input(&t, b"\r");
    t.write(b"\r\n[sudo] pw: ");
    p.check(&t);
    p.input(&t, b"hunter2");
    let ov = p.overlay();
    assert!(ov.is_empty(), "drew an unconfirmed epoch: {ov:?}");
    // It times out unseen, and doesn't count toward backoff.
    clk.add(Duration::from_secs(2));
    p.check(&t);
    assert!(
        p.misses == 0 && !p.take_repaint(),
        "misses {} after an epoch that was never drawn",
        p.misses
    );
}

#[test]
fn predict_epoch_draws_after_first_confirmation() {
    let (mut p, mut t, _) = predictor_at();
    p.new_epoch();
    p.input(&t, b"ab");
    assert!(p.overlay().is_empty(), "drew before any confirmation");
    t.write(b"a");
    p.check(&t);
    assert_eq!(overlay(&mut p), "b");
    // A miss starts a new epoch: hidden again until a confirmation.
    t.write(b"Z");
    p.check(&t);
    p.input(&t, b"c");
    assert!(p.overlay().is_empty(), "drew right after a miss");
}

/// #2: line editors turn ECHO and ICANON off and echo themselves; password
/// prompts turn only ECHO off.
#[test]
fn predict_active_rule() {
    for (echo, canonical, want) in [
        (true, true, true),
        (true, false, true),
        (false, false, true),
        (false, true, false),
    ] {
        let (mut p, mut t, _) = predictor_at();
        p.input_mode(InputMode {
            known: true,
            echo,
            canonical,
        });
        p.input(&t, b"a");
        t.write(b"a");
        p.check(&t);
        p.input(&t, b"b");
        assert_eq!(
            overlay(&mut p) == "b",
            want,
            "echo={echo} canonical={canonical}"
        );
    }
}

/// #3: an arrow press must not leave a predicted cursor behind once the
/// mirror has moved on.
#[test]
fn predict_cursor_not_stale_after_arrow() {
    let (mut p, mut t, _) = predictor_at();
    p.input_mode(InputMode {
        known: true,
        canonical: false,
        ..Default::default()
    });
    p.input(&t, b"ls");
    t.write(b"ls");
    p.check(&t);
    p.input(&t, b"\x1b[D");
    p.check(&t);
    p.input(&t, b"\r");
    t.write(b"\r\nfile\r\n$ ");
    p.check(&t);
    p.input(&t, b"x");
    let got = pending_at(&p);
    assert!(
        got.len() == 1 && got[0].x == 2 && got[0].y == 2,
        "predicted at {got:?}, want the mirror's cursor (2,2)"
    );
}

/// #7: widths come from libghostty, so an emoji in the prompt doesn't shift
/// the cells after it.
#[test]
fn predict_emoji_prompt() {
    let (mut p, mut t, _) = predictor_at();
    t.write("\r⚡ ✅ ⭐ 🫠 ❯ ".as_bytes());
    for r in ['⚡', '✅', '⭐', '🫠'] {
        assert_eq!(
            cluster_width(r.encode_utf8(&mut [0; 4])),
            2,
            "{r:?}: libghostty says 2"
        );
    }
    p.input(&t, b"a");
    t.write(b"a");
    p.check(&t);
    assert!(
        p.misses == 0 && p.preds.is_empty(),
        "echo after an emoji prompt not confirmed: misses {}, pending {:?}",
        p.misses,
        pending_at(&p)
    );
    p.input(&t, b"b");
    assert_eq!(overlay(&mut p), "b", "not predicting after an emoji prompt");
}

/// #9: a space over a blank looks confirmed before any echo; it must not
/// reset the miss backoff or confirm an epoch.
#[test]
fn predict_space_is_no_confirmation() {
    let (mut p, t, _) = predictor_at();
    p.new_epoch();
    p.misses = 2;
    p.input(&t, b" ");
    p.check(&t);
    assert_eq!(p.misses, 2, "a blank over a blank reset misses");
    p.input(&t, b"q");
    assert!(p.overlay().is_empty(), "a blank confirmed the epoch");
}

/// #11: in canonical mode the tty echoes an arrow as ^[[D; it's not a move.
#[test]
fn predict_arrows_only_outside_canonical() {
    let (mut p, t, _) = predictor_at(); // echo, canonical; "$ " with blanks after
    p.input(&t, b"\x1b[Dx");
    assert!(
        p.preds.is_empty(),
        "predicted after an arrow in canonical mode: {:?}",
        pending_at(&p)
    );
}

/// Overlay never shows a key the mirror hasn't echoed: with no echo at
/// all, whatever is typed and printed, nothing is ever drawn (Go's
/// FuzzPredictNeverDrawsUnechoed).
#[test]
fn fuzz_predict_never_draws_unechoed() {
    let seeds: &[(&[u8], &[u8], u8)] = &[
        (b"abc\rdef\x1b[Dgh", b"xyz\r\n$ ", 5),
        (b"hunter2\r", b"\r\n[sudo] password: ", 2),
    ];
    for (typed, printed, steps) in fuzz_cases(seeds, 300) {
        let mut t = term(16, 4);
        t.write(b"$ ");
        let clk = TestClock::new();
        let mut p = Predictor::new(PredictorConfig {
            mode: PredictMode::Always,
            now: Some(clk.clock()),
            ..Default::default()
        });
        p.input_mode(InputMode {
            known: true,
            echo: true,
            ..Default::default()
        });
        // Typed: letters a-h, Enter and whole arrow sequences. Printed:
        // anything but a letter a-h, so no typed key is ever echoed.
        let mut input = Vec::new();
        for &b in &typed {
            match b % 10 {
                8 => input.push(b'\r'),
                9 => input.extend_from_slice(b"\x1b[D"),
                _ => input.push(b'a' + b % 8),
            }
        }
        let out: Vec<u8> = printed
            .iter()
            .map(|&b| {
                if (b'a'..=b'h').contains(&b) {
                    b + (b'z' - b'h')
                } else {
                    b
                }
            })
            .collect();
        let n = (steps % 6) as usize + 1;
        for i in 0..n {
            p.input(&t, chunk(&input, i, n));
            t.write(chunk(&out, i, n));
            clk.add(Duration::from_millis(400) * i as u32);
            p.check(&t);
            for c in p.overlay() {
                assert!(
                    !(c.state == CellState::Tentative && ('a'..='h').contains(&c.rune)),
                    "drew {:?}, never echoed ({typed:?} {printed:?} {steps})",
                    c.rune
                );
            }
        }
    }
}

/// An arrow split across reads is still an arrow, not the text "[D".
#[test]
fn predict_split_escape() {
    let (mut p, t, _) = predictor_at();
    p.input(&t, b"\x1b");
    p.input(&t, b"[Dx");
    assert!(
        p.preds.is_empty(),
        "predicted the tail of an escape: {:?}",
        pending_at(&p)
    );
    // Also when the start came with Enter, and for longer sequences.
    let (mut p2, t2, clk) = predictor_at();
    p2.input(&t2, b"\r\x1b[1");
    clk.add(Duration::from_secs(2));
    p2.check(&t2);
    p2.input(&t2, b";5Dz");
    assert!(
        p2.preds.is_empty(),
        "predicted the tail of an escape: {:?}",
        pending_at(&p2)
    );
}
