//! Echo prediction (PROTOCOL.md "Echo prediction").

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use osewa_ghostty::{self as ghostty, Terminal};

use crate::SendCell;

/// Chooses when a Predictor's overlay is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PredictMode {
    /// Shows predictions once the round trip reaches `min_rtt`.
    #[default]
    Auto,
    Always,
    Never,
}

/// Parses "auto", "always" or "never".
pub fn parse_predict_mode(s: &str) -> Result<PredictMode, String> {
    match s {
        "auto" => Ok(PredictMode::Auto),
        "always" => Ok(PredictMode::Always),
        "never" => Ok(PredictMode::Never),
        _ => Err(format!(
            "osewa: predict mode {}: want auto, always or never",
            osewa_wire::go_quote(s)
        )),
    }
}

/// A prediction's state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CellState {
    /// Predicted and not yet in the mirror; draw it marked.
    #[default]
    Tentative,
    /// Matches the mirror and needs no drawing.
    Confirmed,
}

/// One predicted cell in the active area, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prediction {
    pub x: i32,
    pub y: i32,
    /// ' ' for an erased cell.
    pub rune: char,
    pub wide: bool,
    pub state: CellState,
}

impl Default for Prediction {
    fn default() -> Prediction {
        Prediction {
            x: 0,
            y: 0,
            rune: '\0',
            wide: false,
            state: CellState::Tentative,
        }
    }
}

/// A clock, for tests.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Tunes a Predictor. Zero values take the defaults.
#[derive(Clone, Default)]
pub struct PredictorConfig {
    pub mode: PredictMode,
    /// The round trip from which auto mode shows predictions; below it
    /// echo is fast enough. Default 30 ms.
    pub min_rtt: Duration,
    /// The least time a prediction waits for its echo before it is
    /// dropped; the wait is at least 3 round trips. Default 1 s.
    pub timeout: Duration,
    /// Consecutive misses that hide predictions until one is confirmed.
    /// Default 3.
    pub max_misses: u32,
    pub now: Option<Clock>,
}

impl fmt::Debug for PredictorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PredictorConfig")
            .field("mode", &self.mode)
            .field("min_rtt", &self.min_rtt)
            .field("timeout", &self.timeout)
            .field("max_misses", &self.max_misses)
            .finish_non_exhaustive()
    }
}

/// The PTY's line discipline as the holder last reported it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputMode {
    /// An input_mode event has arrived.
    pub known: bool,
    pub echo: bool,
    pub canonical: bool,
}

impl From<osewa_client::InputMode> for InputMode {
    fn from(m: osewa_client::InputMode) -> InputMode {
        InputMode {
            known: m.known,
            echo: m.echo,
            canonical: m.canonical,
        }
    }
}

#[derive(Clone)]
struct Pred {
    p: Prediction,
    epoch: u64,
    /// The mirror's cell when predicted.
    before: char,
    at: Instant,
    /// Its epoch was confirmed, so it may have been drawn.
    shown: bool,
    /// Input after this one (Enter, a control key) may move or scroll it,
    /// so a mismatch is not the prediction's fault.
    barrier: bool,
}

/// Mosh-style local echo (PROTOCOL.md "Echo prediction"). It guesses what
/// typed input will show, as an overlay over the mirror, and checks each
/// guess against the mirror as output arrives. It only reads the terminals
/// it is given. Call it where the mirror is locked, such as `Mirror::view`
/// or the Options callbacks.
///
/// Guesses come in epochs, as in mosh. Enter, control keys and misses start
/// a new epoch, and nothing of an epoch is drawn until the mirror confirms
/// one of its guesses. Input the program doesn't echo, such as a password,
/// is therefore never drawn.
pub struct Predictor {
    cfg: PredictorConfig,
    now: Clock,
    mode: InputMode,
    srtt: Duration,
    preds: Vec<Pred>,

    /// The predicted cursor, while have_cursor.
    cx: i32,
    cy: i32,
    have_cursor: bool,

    epoch: u64,
    /// The current epoch has a confirmed guess.
    confirmed: bool,

    /// Stops predicting after input whose effect we can't guess (Enter,
    /// control keys) until the mirror shows its result.
    blocked: bool,
    blocked_at: Instant,
    block_x: i32,
    block_y: i32,

    misses: u32,
    /// An overlay was handed out since the last repaint.
    drawn: bool,
    repaint: bool,

    /// An escape sequence cut off at the end of an Input, so its tail isn't
    /// taken for typed text.
    esc: Vec<u8>,
}

impl fmt::Debug for Predictor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Predictor")
            .field("cfg", &self.cfg)
            .field("pending", &self.preds.len())
            .finish_non_exhaustive()
    }
}

impl Predictor {
    /// A Predictor with cfg's defaults filled in.
    pub fn new(mut cfg: PredictorConfig) -> Predictor {
        if cfg.min_rtt.is_zero() {
            cfg.min_rtt = Duration::from_millis(30);
        }
        if cfg.timeout.is_zero() {
            cfg.timeout = Duration::from_secs(1);
        }
        if cfg.max_misses == 0 {
            cfg.max_misses = 3;
        }
        let now = cfg.now.clone().unwrap_or_else(|| Arc::new(Instant::now));
        let t = now();
        Predictor {
            cfg,
            now,
            mode: InputMode::default(),
            srtt: Duration::ZERO,
            preds: Vec::new(),
            cx: 0,
            cy: 0,
            have_cursor: false,
            epoch: 0,
            confirmed: false,
            blocked: false,
            blocked_at: t,
            block_x: 0,
            block_y: 0,
            misses: 0,
            drawn: false,
            repaint: false,
            esc: Vec::new(),
        }
    }

    /// Records the PTY's latest input_mode.
    pub fn input_mode(&mut self, m: InputMode) {
        self.mode = m;
    }

    /// Adds a round trip sample.
    pub fn rtt(&mut self, d: Duration) {
        if self.srtt.is_zero() {
            self.srtt = d;
        } else {
            let s = self.srtt.as_nanos() as i128;
            let n = s + (d.as_nanos() as i128 - s) / 8;
            self.srtt = Duration::from_nanos(n.max(0) as u64);
        }
    }

    /// Forgets every prediction. The caller repaints from the mirror.
    pub fn reset(&mut self) {
        self.preds.clear();
        self.have_cursor = false;
        self.blocked = false;
        self.drawn = false;
        self.repaint = false;
        self.new_epoch();
    }

    /// Whether any prediction awaits its echo, so a caller knows to keep
    /// calling `check` for timeouts while no output arrives.
    pub fn pending(&self) -> bool {
        !self.preds.is_empty()
    }

    /// Whether input is predictable now. Line editors (bash, zsh, fish)
    /// turn off both ECHO and ICANON and echo themselves; a password prompt
    /// turns off only ECHO.
    fn active(&self, t: &Terminal) -> bool {
        // Origin mode makes cursor rows relative.
        self.cfg.mode != PredictMode::Never
            && self.mode.known
            && (self.mode.echo || !self.mode.canonical)
            && !t.alt_screen()
            && !t.mode(6, false)
    }

    fn shown(&self) -> bool {
        if self.misses >= self.cfg.max_misses {
            return false;
        }
        match self.cfg.mode {
            PredictMode::Always => true,
            PredictMode::Auto => self.srtt >= self.cfg.min_rtt,
            PredictMode::Never => false,
        }
    }

    fn timeout(&self) -> Duration {
        self.cfg.timeout.max(3 * self.srtt)
    }

    /// When `check` would next time a prediction out with no output
    /// arriving, so a caller can sleep until then instead of polling: None
    /// while nothing is pending.
    pub fn deadline(&self) -> Option<Instant> {
        let timeout = self.timeout();
        self.preds
            .iter()
            .filter(|pr| pr.p.state == CellState::Tentative)
            .map(|pr| pr.at + timeout)
            .min()
    }

    pub(crate) fn new_epoch(&mut self) {
        self.epoch += 1;
        self.confirmed = false;
    }

    /// Predicts the effect of bytes the user is about to send. t is the
    /// mirror.
    pub fn input(&mut self, t: &Terminal, b: &[u8]) {
        let mut buf = std::mem::take(&mut self.esc);
        buf.extend_from_slice(b);
        let (n, rest) = split_escape(&buf);
        self.esc = rest;
        let mut b = &buf[..n];
        if !self.active(t) {
            self.drop_all(false);
            return;
        }
        let scr = Screen::new(t);
        while !b.is_empty() && !self.blocked {
            let c = b[0];
            if c == 0x1b {
                let mut n = 0;
                if !self.mode.canonical {
                    // In canonical mode the tty echoes an arrow as ^[[D.
                    n = self.arrow(&scr, t, b);
                }
                if n == 0 {
                    self.block(t);
                    return;
                }
                b = &b[n..];
            } else if c == 0x7f || c == 0x08 {
                self.backspace(t, &scr);
                b = &b[1..];
            } else if c < 0x20 {
                self.block(t);
            } else {
                let Some((r, n)) = decode_rune(b) else {
                    self.block(t);
                    return;
                };
                self.char(t, &scr, r);
                b = &b[n..];
            }
        }
    }

    fn cursor_at(&self, t: &Terminal) -> (i32, i32) {
        if self.have_cursor {
            return (self.cx, self.cy);
        }
        let (x, y) = t.cursor();
        (x as i32, y as i32)
    }

    /// Stops predicting until the mirror settles, and starts a new epoch:
    /// what came before may now move, and what comes after must earn its
    /// display again.
    fn block(&mut self, t: &Terminal) {
        self.blocked = true;
        self.blocked_at = (self.now)();
        let (x, y) = t.cursor();
        (self.block_x, self.block_y) = (x as i32, y as i32);
        self.have_cursor = false;
        for p in &mut self.preds {
            p.barrier = true;
        }
        self.new_epoch();
    }

    /// Whether nothing but blanks (in the mirror or predicted) lies at or
    /// right of x on row y. Shells insert mid-line, which we don't model,
    /// so we only predict at the end of the line.
    fn at_end(&self, scr: &Screen, x: i32, y: i32) -> bool {
        let row = scr.row(y);
        for i in x.max(0)..row.len() as i32 {
            let mut c = row[i as usize];
            if let Some(pr) = self.at(i, y) {
                c = pr.p.rune;
            }
            if c != ' ' && c != '\0' {
                return false;
            }
        }
        let from = x.max(row.len() as i32);
        !self
            .preds
            .iter()
            .any(|pr| pr.p.y == y && pr.p.x >= from && pr.p.rune != ' ')
    }

    /// The latest prediction for a cell.
    fn at(&self, x: i32, y: i32) -> Option<&Pred> {
        self.preds
            .iter()
            .rev()
            .find(|pr| pr.p.y == y && pr.p.x == x)
    }

    fn char(&mut self, t: &Terminal, scr: &Screen, r: char) {
        let mut tmp = [0u8; 4];
        let w = cluster_width(r.encode_utf8(&mut tmp)) as i32;
        let (x, y) = self.cursor_at(t);
        let (cols, _) = t.size();
        // Stay off the last column, where wrapping starts.
        if w == 0 || x + w >= cols as i32 || !self.at_end(scr, x, y) {
            self.block(t);
            return;
        }
        self.add(
            scr,
            Prediction {
                x,
                y,
                rune: r,
                wide: w == 2,
                state: CellState::Tentative,
            },
        );
        (self.cx, self.cy, self.have_cursor) = (x + w, y, true);
    }

    fn backspace(&mut self, t: &Terminal, scr: &Screen) {
        let (x, y) = self.cursor_at(t);
        if x == 0 || !self.at_end(scr, x, y) {
            self.block(t);
            return;
        }
        let mut nx = x - 1;
        if self.wide_at(scr, nx - 1, y) {
            nx -= 1;
        }
        self.add(
            scr,
            Prediction {
                x: nx,
                y,
                rune: ' ',
                ..Default::default()
            },
        );
        (self.cx, self.cy, self.have_cursor) = (nx, y, true);
    }

    /// Whether a wide character starts at x on row y, as predicted or in
    /// the mirror.
    fn wide_at(&self, scr: &Screen, x: i32, y: i32) -> bool {
        if x < 0 {
            return false;
        }
        if let Some(pr) = self.at(x, y) {
            return pr.p.wide;
        }
        let row = scr.row(y);
        let x = x as usize;
        x + 1 < row.len() && row[x] != '\0' && row[x + 1] == '\0'
    }

    /// Handles left and right keys and returns how many bytes it used, or 0
    /// for any other escape.
    fn arrow(&mut self, scr: &Screen, t: &Terminal, b: &[u8]) -> usize {
        if b.len() < 3 || (b[1] != b'[' && b[1] != b'O') || (b[2] != b'C' && b[2] != b'D') {
            return 0;
        }
        let (mut x, y) = self.cursor_at(t);
        if b[2] == b'D' && x > 0 {
            x -= 1;
            if self.wide_at(scr, x - 1, y) {
                x -= 1;
            }
        } else if b[2] == b'C' && !self.at_end(scr, x, y) {
            x += 1;
            if self.wide_at(scr, x - 1, y) {
                x += 1;
            }
        }
        (self.cx, self.cy, self.have_cursor) = (x, y, true);
        3
    }

    fn add(&mut self, scr: &Screen, c: Prediction) {
        // A new guess for a cell replaces the old one, which can't be
        // checked on its own any more.
        self.preds.retain(|pr| pr.p.y != c.y || pr.p.x != c.x);
        self.preds.push(Pred {
            p: c,
            epoch: self.epoch,
            before: scr.cell(c.x, c.y),
            at: (self.now)(),
            shown: self.confirmed,
            barrier: false,
        });
    }

    /// Compares predictions with the mirror: after every applied frame, and
    /// periodically (while `pending`) so a prediction whose echo never
    /// comes times out.
    pub fn check(&mut self, t: &Terminal) {
        if !self.active(t) {
            self.drop_all(false);
            return;
        }
        let now = (self.now)();
        if !self.preds.is_empty() {
            let scr = Screen::new(t);
            let timeout = self.timeout();
            let mut tentative = 0;
            for i in 0..self.preds.len() {
                let pr = &self.preds[i];
                if pr.p.state == CellState::Confirmed {
                    continue;
                }
                let c = scr.cell(pr.p.x, pr.p.y);
                if c == pr.p.rune {
                    let (rune, before, epoch) = (pr.p.rune, pr.before, pr.epoch);
                    self.preds[i].p.state = CellState::Confirmed;
                    // A guess that matches what was already there proves
                    // nothing about the echo.
                    if rune != before {
                        // Hidden after repeated misses until a confirmation.
                        // Earlier ones don't wipe the count, or misses could
                        // never add up: a guess is only drawn after one.
                        if self.misses >= self.cfg.max_misses {
                            self.misses = 0;
                        }
                        if epoch == self.epoch {
                            self.confirmed = true;
                        }
                    }
                } else if c == pr.before && now.saturating_duration_since(pr.at) < timeout {
                    tentative += 1;
                } else {
                    // Later guesses built on this one; drop them all. Only
                    // a guess that could have been drawn counts as a miss.
                    let miss = pr.shown && !pr.barrier;
                    self.drop_all(miss);
                    self.block(t);
                    return;
                }
            }
            if tentative == 0 {
                self.preds.clear();
            }
            self.mark_shown();
        }
        if self.preds.is_empty() {
            self.have_cursor = false;
        }
        if self.blocked && self.preds.is_empty() {
            let (x, y) = t.cursor();
            if x as i32 != self.block_x
                || y as i32 != self.block_y
                || now.saturating_duration_since(self.blocked_at) >= self.timeout()
            {
                self.blocked = false;
            }
        }
    }

    /// Flags the guesses of the current epoch once it is confirmed.
    fn mark_shown(&mut self) {
        if !self.confirmed {
            return;
        }
        let epoch = self.epoch;
        for p in &mut self.preds {
            if p.epoch == epoch {
                p.shown = true;
            }
        }
    }

    fn drop_all(&mut self, miss: bool) {
        self.have_cursor = false;
        if self.preds.is_empty() {
            return;
        }
        if miss {
            self.misses += 1;
        }
        self.preds.clear();
        if self.drawn {
            self.repaint = true;
            self.drawn = false;
        }
    }

    /// The predictions to draw: those of confirmed epochs, or none while
    /// predictions are hidden (round trip too short, too many misses).
    pub fn overlay(&mut self) -> Vec<Prediction> {
        if !self.shown() {
            return Vec::new();
        }
        self.mark_shown();
        let out: Vec<Prediction> = self.preds.iter().filter(|p| p.shown).map(|p| p.p).collect();
        if !out.is_empty() {
            self.drawn = true;
        }
        out
    }

    /// Where the cursor goes while an overlay is shown: after the last
    /// predicted character.
    pub fn cursor(&self) -> Option<(i32, i32)> {
        if !self.have_cursor || !self.shown() || !self.confirmed {
            return None;
        }
        self.preds
            .iter()
            .any(|p| p.shown && p.p.state == CellState::Tentative)
            .then_some((self.cx, self.cy))
    }

    /// Reports, once, that predictions shown earlier were dropped: the
    /// cells they covered must be redrawn from the mirror.
    pub fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.repaint)
    }
}

/// Holds back an escape sequence cut off at the end of b: returns how many
/// bytes are complete, and the rest.
fn split_escape(b: &[u8]) -> (usize, Vec<u8>) {
    let Some(i) = b.iter().rposition(|&c| c == 0x1b) else {
        return (b.len(), Vec::new());
    };
    if b.len() - i > 32 {
        return (b.len(), Vec::new());
    }
    let tail = &b[i..];
    let mut cut = tail.len() == 1 || (tail.len() == 2 && tail[1] == b'O');
    if tail.len() >= 2 && tail[1] == b'[' {
        cut = !tail[2..].iter().any(|&c| (0x40..=0x7e).contains(&c));
    }
    if !cut {
        return (b.len(), Vec::new());
    }
    (i, tail.to_vec())
}

/// Decodes the first UTF-8 character of b, or None if it isn't one (an
/// invalid byte, which counts as one byte wide).
fn decode_rune(b: &[u8]) -> Option<(char, usize)> {
    let n = match b[0] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let s = std::str::from_utf8(b.get(..n)?).ok()?;
    Some((s.chars().next()?, n))
}

/// The mirror's active area as cells: the first char of each character per
/// column, '\0' for the right half of a wide character.
struct Screen {
    rows: Vec<Vec<char>>,
}

impl Screen {
    fn new(t: &Terminal) -> Screen {
        // Every keystroke and echo builds one, so it reads the active area
        // only: a long scrollback would cost its length each time.
        let b = t.active_text().unwrap_or_default();
        let s = String::from_utf8_lossy(&b);
        let (_, n) = t.size();
        let lines = s.split('\n');
        let mut rows = vec![Vec::new(); n];
        for (y, line) in lines.take(n).enumerate() {
            let mut row = Vec::new();
            for cl in clusters(line) {
                let r = cl.chars().next().unwrap_or('\0');
                let w = cluster_width(cl);
                if w > 0 {
                    row.push(r);
                    row.extend(std::iter::repeat_n('\0', w - 1));
                }
            }
            rows[y] = row;
        }
        Screen { rows }
    }

    fn row(&self, y: i32) -> &[char] {
        if y < 0 {
            return &[];
        }
        self.rows.get(y as usize).map(Vec::as_slice).unwrap_or(&[])
    }

    fn cell(&self, x: i32, y: i32) -> char {
        let row = self.row(y);
        if x >= 0 && (x as usize) < row.len() {
            return row[x as usize];
        }
        ' '
    }
}

/// Splits a row of text into characters as cells hold them: a char with
/// the zero-width chars after it, and chars joined by ZWJ.
pub(crate) fn clusters(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut prev = '\0';
    for (i, r) in s.char_indices() {
        let join = start.is_some()
            && (prev == '\u{200d}'
                || (r as u32 > 0x7f && cluster_width(r.encode_utf8(&mut [0; 4])) == 0));
        if !join {
            if let Some(st) = start {
                out.push(&s[st..i]);
            }
            start = Some(i);
        }
        prev = r;
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

struct Widths {
    t: SendCell<Terminal>,
    m: HashMap<String, usize>,
}

/// Caches how many cells libghostty gives a character. Measuring with the
/// pinned libghostty itself keeps cells in step with the mirror; a table of
/// our own disagreed on emoji like ⚡ and shifted whole rows.
static WIDTHS: Mutex<Option<Widths>> = Mutex::new(None);

pub(crate) fn cluster_width(s: &str) -> usize {
    if s.len() == 1 && (0x20..0x7f).contains(&s.as_bytes()[0]) {
        return 1;
    }
    let mut g = WIDTHS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(w) = g.as_ref().and_then(|w| w.m.get(s)) {
        return *w;
    }
    if g.is_none() {
        let Ok(t) = Terminal::new(16, 1, ghostty::Options::default()) else {
            return 1;
        };
        *g = Some(Widths {
            t: SendCell(t),
            m: HashMap::new(),
        });
    }
    let w = g.as_mut().expect("set above");
    if w.m.len() > 4096 {
        w.m.clear();
    }
    w.t.0.write(format!("\x1b[H\x1b[2K{s}").as_bytes());
    let (x, _) = w.t.0.cursor();
    w.m.insert(s.to_string(), x);
    x
}

#[cfg(test)]
mod tests;
