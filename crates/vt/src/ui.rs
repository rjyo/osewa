//! The attach UI: one attached session drawn on a
//! local terminal, with predictions over it and notices.

use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use osewa_client::Session;
use osewa_ghostty::Terminal;
use osewa_mirror::{InputMode, Mirror, Predictor};
use osewa_wire as wire;
use tokio::sync::Notify;

use crate::render::*;
use crate::status::status_text;
use crate::vtfilter::{printable, QueryFilter};

/// What the local terminal said about itself, answering its appearance
/// queries and clipboard reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// An OSC 52 reply: what the clipboard holds.
    Clipboard(Vec<u8>),
    /// Dark when true (CSI ? 997 ; 1|2 n).
    Scheme(bool),
    /// OSC 10, 11 and 12 replies.
    Foreground([u8; 3]),
    Background([u8; 3]),
    Cursor([u8; 3]),
    /// OSC 4 replies.
    Palette(u8, [u8; 3]),
}

/// A client id as people know it: the device name, without the per-process
/// suffix, made printable since it comes from the remote.
pub fn owner_name(id: &str) -> String {
    if id.is_empty() {
        return "nobody".into();
    }
    let id = match id.rfind('-') {
        Some(i) if i > 0 => &id[..i],
        _ => id,
    };
    printable(id)
}

/// Holds back an incomplete trailing character, since a read can end in the
/// middle of one and text input travels as a JSON string.
pub fn split_utf8(b: &[u8]) -> (&[u8], &[u8]) {
    let lo = b.len().saturating_sub(4);
    for i in (lo..b.len()).rev() {
        // A rune start: not a continuation byte.
        if b[i] & 0xc0 != 0x80 {
            let need = match b[i] {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1, // invalid: never completes
            };
            if b.len() - i < need {
                return (&b[..i], &b[i..]);
            }
            break;
        }
    }
    (b, &[])
}

/// Checks predictions for timeouts while attached. It sleeps until the
/// oldest prediction's deadline, or until typing makes one, never on a
/// period: a laptop or phone attached all day should not wake for nothing.
/// It runs on any executor (the CLI's tokio runtime, or a thread's
/// [`osewa_client::block_on`]).
pub async fn ticker(a: Attached, done: impl std::future::Future<Output = ()>) {
    tokio::pin!(done);
    loop {
        match a.next_tick() {
            Some(at) => tokio::select! {
                _ = osewa_client::sleep(at.saturating_duration_since(Instant::now())) => a.tick(),
                _ = a.wake.notified() => {}
                _ = &mut done => return,
            },
            None => tokio::select! {
                _ = a.wake.notified() => {}
                _ = &mut done => return,
            },
        }
    }
}

/// How long a notice stays on the bottom row, and how often one can show.
pub const NOTICE_FOR: Duration = Duration::from_secs(3);
pub const NOTICE_EVERY: Duration = Duration::from_secs(1);
/// How long after a keystroke status notices wait.
const TYPING_QUIET: Duration = Duration::from_secs(2);

/// Takes ownership when typed by a viewer. A viewer's keys are dropped
/// anyway, so it needs no escape; the owner's Ctrl-T reaches the program.
/// It is this one byte because a viewer's terminal stays out of the
/// program's keyboard modes (Ui::legacy_viewer).
const TAKE_KEY: char = '\x14';

/// How the local terminal encodes keys.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KeyModes {
    /// Kitty keyboard flags.
    kitty: u8,
    /// Each screen has its own flags, so a switch needs them set again.
    /// Always false without flags, when there is nothing to set.
    alt: bool,
    /// xterm modifyOtherKeys mode 2.
    other: bool,
}

/// What the UI draws with, made to repaint later (a notice's timer).
struct Later {
    m: Mirror,
    ui: Weak<Mutex<Ui>>,
}

/// One attached session's drawing state. Everything that writes runs with
/// the mirror locked (the mirror's callbacks, or inside Mirror::view), and
/// then this, so writes never interleave.
pub struct Ui {
    out: Box<dyn Write + Send>,
    /// What one frame or keystroke draws, written at once: a write per
    /// piece costs a syscall each, and the user's terminal a wakeup each.
    buf: Vec<u8>,
    pub pred: Predictor,
    pub filter: QueryFilter,
    mode: InputMode,
    /// A snapshot was drawn, so later ones repaint only the screen.
    drawn: bool,
    /// Dropped predictions wait for the primary screen to come back before
    /// they can be painted over.
    repaint_later: bool,
    pub notice_at: Option<Instant>,
    notice_gen: u64,
    later: Option<Later>,
    /// Turns off status notices. status_note is one waiting for the screen
    /// (events come without the terminal); typed_at is the last keystroke,
    /// since a notice must not land on what the user is typing.
    pub quiet: bool,
    status_note: String,
    pub typed_at: Option<Instant>,
    /// The local cursor sits after predicted text while an overlay shows
    /// (displaced); real output must then first go back to the mirror's
    /// cursor as it was before that output: real_x, real_y.
    displaced: bool,
    real_x: usize,
    real_y: usize,
    /// The ticker has a prediction deadline to sleep until; without one it
    /// waits to be woken (Attached::wake), so an idle attach never wakes.
    ticking: bool,
    /// The ticker's wakeups, which the tests bound.
    pub ticker_wakes: u64,
    /// The images the local terminal has, while graphics pass
    /// (filter.graphics).
    sent: SentImages,
    /// The pointer shape the local terminal was last given (OSC 22).
    pointer: &'static str,
    /// The local terminal may have colors a program set.
    colored: bool,
    /// Programs may read the local clipboard (OSC 52): the local terminal
    /// is asked, and it answers. Otherwise reads are answered empty.
    clipboard_read: bool,
    /// The session, to answer clipboard reads.
    session: Arc<Mutex<Option<Session>>>,
    /// This client's id, and whether it owns the session as last reported.
    id: String,
    owned: bool,
    /// Owner events so far, to tell a read of the owner from a newer event.
    pub(crate) owner_events: u64,
    /// Keeps a viewer's terminal out of the program's keyboard modes: its
    /// keys are for us, not the program, and a program's modes would encode
    /// them past recognition (Ctrl-T as CSI u; with Kitty's "report all
    /// keys", the ~t escape too).
    legacy_viewer: bool,
    /// The keyboard modes the mirror has.
    keys: KeyModes,
    /// The ones the local terminal has; None when a redraw may have changed
    /// them.
    sent_keys: Option<KeyModes>,
}

impl Ui {
    pub fn new(out: Box<dyn Write + Send>, quiet: bool, pred: Predictor) -> Ui {
        Ui {
            out,
            buf: Vec::new(),
            pred,
            filter: QueryFilter::default(),
            mode: InputMode::default(),
            drawn: false,
            repaint_later: false,
            notice_at: None,
            notice_gen: 0,
            later: None,
            quiet,
            status_note: String::new(),
            typed_at: None,
            displaced: false,
            real_x: 0,
            real_y: 0,
            ticking: false,
            ticker_wakes: 0,
            sent: SentImages::new(),
            pointer: "text",
            colored: false,
            clipboard_read: false,
            session: Arc::default(),
            id: String::new(),
            owned: false,
            owner_events: 0,
            legacy_viewer: false,
            keys: KeyModes::default(),
            sent_keys: Some(KeyModes::default()),
        }
    }

    /// Passes the remote's Kitty graphics to the local terminal, and redraws
    /// its images along with the screen.
    pub fn with_graphics(mut self) -> Ui {
        self.filter.graphics = true;
        self
    }

    /// Lets programs read the local clipboard, through the local terminal,
    /// which may ask first.
    pub fn with_clipboard_read(mut self) -> Ui {
        self.clipboard_read = true;
        self
    }

    /// Reads keys for this client alone while it only views. Not for an app
    /// whose first key takes over: that key goes to the program, encoded in
    /// the program's modes.
    pub fn with_legacy_viewer(mut self) -> Ui {
        self.legacy_viewer = true;
        self
    }

    fn write(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// Writes out what was drawn since the last flush.
    pub fn flush(&mut self) {
        if !self.buf.is_empty() {
            let _ = self.out.write_all(&self.buf);
            self.buf.clear();
        }
    }

    /// Draws a new snapshot. The first one brings the scrollback along;
    /// later ones (after lag or a lost resume) replace only the screen, so
    /// the local scrollback doesn't fill with copies.
    pub fn ready(&mut self, t: &Terminal) {
        // Output after a snapshot continues the remote stream, not whatever
        // partial sequence we held from before it. Predictions stay: the
        // applied callback right after this checks them against the new
        // mirror. Under catch-up, screen snapshots arrive often and the
        // echo of what was typed shows up in one of them.
        self.repaint_later = false;
        self.displaced = false;
        if !self.drawn {
            self.drawn = true;
            self.write(&attach_redraw(t));
        } else {
            self.write(&snapshot_redraw(t));
        }
        self.filter.reset(&tail_style(t));
        (self.real_x, self.real_y) = t.cursor();
        self.sync_pointer(t);
        self.redrawn_keys(t);
        let (colors, colored) = colors_redraw(t, self.colored || std::mem::take(&mut self.filter.passed_colors));
        self.colored = colored;
        self.write(&colors);
        if self.filter.graphics {
            let draw = images_draw(t, &mut self.sent);
            self.write(&draw);
        }
        self.flush();
    }

    /// Repaints everything, scrollback included, for a local terminal that
    /// was replaced. Unlike ready, the remote stream goes on from where it
    /// is: the filter keeps what it holds.
    pub fn redraw(&mut self, t: &Terminal) {
        self.write(&attach_redraw(t));
        self.displaced = false;
        (self.real_x, self.real_y) = t.cursor();
        self.pointer = "";
        self.sync_pointer(t);
        self.redrawn_keys(t);
        let (colors, colored) = colors_redraw(t, self.colored);
        self.colored = colored;
        self.write(&colors);
        if self.filter.graphics {
            self.sent.clear();
            let draw = images_draw(t, &mut self.sent);
            self.write(&draw);
        }
        self.paint_buffered(t);
        self.flush();
    }

    /// After a redraw, which can switch the local screen under the modes
    /// and skip output that changed them.
    fn redrawn_keys(&mut self, t: &Terminal) {
        if self.sent_keys != Some(KeyModes::default()) {
            self.sent_keys = None;
        }
        self.filter.saw_other_keys = true;
        self.read_keys(t);
    }

    /// Takes the keyboard modes from the mirror. They aren't passed along
    /// with the output: which ones the local terminal gets depends on who
    /// owns the session, and setting the mirror's current flags keeps it
    /// right however the program pushed and popped them.
    fn read_keys(&mut self, t: &Terminal) {
        self.keys.kitty = t.kitty_flags();
        self.keys.alt = self.keys.kitty != 0 && t.alt_screen();
        if std::mem::take(&mut self.filter.saw_other_keys) {
            self.keys.other = t.modify_other_keys();
        }
        self.sync_keys();
    }

    /// Takes an owner read from the session after `events` owner events,
    /// unless a later event already said who it is.
    pub(crate) fn read_owner(&mut self, owner: &str, events: u64) {
        if self.owner_events == events {
            self.set_owner(owner);
        }
    }

    /// Takes who owns the session. No output need follow, so the modes
    /// can't wait for the next flush.
    fn set_owner(&mut self, owner: &str) {
        self.owned = owner == self.id;
        self.sync_keys();
        self.flush();
    }

    /// Gives the local terminal the keyboard modes it should have now.
    fn sync_keys(&mut self) {
        let want = if self.legacy_viewer && !self.owned { KeyModes::default() } else { self.keys };
        let had = self.sent_keys;
        if had == Some(want) {
            return;
        }
        if had.is_none_or(|h| (h.kitty, h.alt) != (want.kitty, want.alt)) {
            self.write(format!("\x1b[={};1u", want.kitty).as_bytes());
        }
        if had.is_none_or(|h| h.other != want.other) {
            self.write(if want.other { b"\x1b[>4;2m" } else { b"\x1b[>4m" });
        }
        self.sent_keys = Some(want);
    }

    /// Gives the local terminal the pointer shape the program asked for.
    fn sync_pointer(&mut self, t: &Terminal) {
        let shape = t.pointer_shape();
        if shape != self.pointer {
            self.pointer = shape;
            self.write(&pointer_set(shape));
        }
    }

    /// The snapshot's images have their pixels now.
    pub fn snapshot_end(&mut self, t: &Terminal) {
        if self.filter.graphics && !t.placements().is_empty() {
            self.displaced = false;
            let draw = images_draw(t, &mut self.sent);
            self.write(&draw);
            self.flush();
        }
    }

    /// Writes remote output, after putting the cursor back if an overlay
    /// had moved it. It goes out with what applied draws next.
    pub fn output(&mut self, b: &[u8]) {
        if self.displaced {
            let cup = format!("\x1b[{};{}H", self.real_y + 1, self.real_x + 1);
            self.write(cup.as_bytes());
            self.displaced = false;
        }
        let f = self.filter.filter(b);
        self.write(&f);
    }

    pub fn applied(&mut self, t: &Terminal) {
        let (saw, ids) = self.filter.take_graphics();
        if self.filter.graphics {
            for id in ids {
                if let Some(hash) = t.image(id).as_ref().and_then(pixels_hash) {
                    self.sent.insert(id, hash);
                }
            }
        } else if saw {
            // The mirror placed images this terminal never saw, moving the
            // cursor past them: the output after went elsewhere here.
            self.write(&screen_redraw(t));
            self.displaced = false;
        }
        self.sync_pointer(t);
        self.read_keys(t);
        self.colored |= std::mem::take(&mut self.filter.passed_colors);
        self.pred.check(t);
        self.paint_buffered(t);
        if !self.status_note.is_empty() {
            let note = std::mem::take(&mut self.status_note);
            self.notice(Some(t), &note);
        }
        self.flush();
    }

    /// Checks predictions with no output arriving, so one whose echo never
    /// comes is taken down at its timeout. Call it with the mirror locked.
    pub fn tick(&mut self, t: &Terminal) {
        if self.pred.pending() {
            self.pred.check(t);
            self.paint(t);
        }
    }

    /// When the ticker should next tick, or None to wait for a wake; the
    /// ticker counts as asleep from then on.
    pub fn next_tick(&mut self) -> Option<Instant> {
        self.ticker_wakes += 1;
        let at = self.pred.deadline();
        self.ticking = at.is_some();
        at
    }

    /// Whether the ticker must be woken for predictions just made: it is
    /// asleep without a deadline. Later ones only push the deadline out.
    pub fn arm_ticker(&mut self) -> bool {
        if self.ticking || !self.pred.pending() {
            return false;
        }
        self.ticking = true;
        true
    }

    pub fn event(&mut self, e: &wire::EventMsg, live: bool) {
        if e.kind == wire::EVENT_STATUS {
            // Only live ones: a status from a catch-up is old news.
            let quiet_typing = self.typed_at.is_none_or(|at| at.elapsed() >= TYPING_QUIET);
            if live && !self.quiet && (e.state == "blocked" || e.state == "done") && quiet_typing {
                let who = if e.app.is_empty() { "program".to_string() } else { printable(&e.app) };
                self.status_note = format!(" {who}: {} ", status_text(&e.state, "", &e.msg, e.progress));
            }
            return;
        }
        if e.kind == wire::EVENT_CLIPBOARD {
            // The holder sends these only to the owner; one replayed from a
            // catch-up would overwrite whatever was copied since.
            if live {
                self.write(&clipboard_set(&e.data));
            }
            return;
        }
        if e.kind == wire::EVENT_CLIPBOARD_READ {
            // Only live ones are still waiting; the holder sends them only to
            // the owner.
            if !live {
                return;
            }
            if self.clipboard_read {
                let sel = if matches!(e.selection.as_str(), "c" | "p" | "s") { e.selection.as_str() } else { "c" };
                self.write(format!("\x1b]52;{sel};?\x1b\\").as_bytes());
            } else if let Some(s) = self.session.lock().unwrap().clone() {
                // Not blocking the mirror on a round trip.
                std::thread::spawn(move || osewa_client::block_on(osewa_client::timeout(Duration::from_secs(5), s.clipboard(b""))));
            }
            return;
        }
        if e.kind == wire::EVENT_OWNER {
            // Catch-up ones too: they end at the current owner.
            self.owner_events += 1;
            self.set_owner(&e.owner);
            return;
        }
        if e.kind == wire::EVENT_NOTIFY {
            if live {
                self.write(&notify_set(&e.title, &e.msg));
            }
            return;
        }
        if e.kind != wire::EVENT_INPUT_MODE {
            return;
        }
        self.mode.known = true;
        if let Some(v) = e.echo {
            self.mode.echo = v;
        }
        if let Some(v) = e.canonical {
            self.mode.canonical = v;
        }
        self.pred.input_mode(self.mode);
    }

    /// Repaints after dropped predictions and draws the ones pending, then
    /// writes it all out.
    pub fn paint(&mut self, t: &Terminal) {
        self.paint_buffered(t);
        self.flush();
    }

    fn paint_buffered(&mut self, t: &Terminal) {
        if self.pred.take_repaint() {
            self.repaint_later = true;
        }
        if self.repaint_later && !t.alt_screen() {
            self.write(&screen_redraw(t));
            self.repaint_later = false;
            self.displaced = false;
        }
        (self.real_x, self.real_y) = t.cursor();
        let (cols, _) = t.size();
        let ov = self.pred.overlay();
        // At the last column the cursor may be waiting to wrap, which any
        // cursor move would cancel; draw once it has moved on.
        if ov.is_empty() || self.real_x + 1 >= cols {
            if self.displaced {
                self.write(&cursor_to(t));
                self.displaced = false;
            }
            return;
        }
        let style = self.filter.style();
        self.write(&overlay_draw(&ov, &style));
        match self.pred.cursor() {
            Some((x, y)) => {
                self.write(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
                self.displaced = true;
            }
            None => {
                self.write(&cursor_to(t));
                self.displaced = false;
            }
        }
    }

    /// Shows msg on the bottom row for a while. Call it with the mirror
    /// locked.
    pub fn notice(&mut self, t: Option<&Terminal>, msg: &str) {
        let Some(t) = t else { return };
        if self.notice_at.is_some_and(|at| at.elapsed() < NOTICE_EVERY) {
            return;
        }
        self.notice_at = Some(Instant::now());
        let style = self.filter.style();
        self.write(&notice_draw(t, msg, &style));
        self.flush();
        self.displaced = false;
        self.notice_gen += 1;
        let Some(later) = &self.later else { return };
        let (gen, m, ui) = (self.notice_gen, later.m.clone(), later.ui.clone());
        std::thread::spawn(move || {
            std::thread::sleep(NOTICE_FOR);
            m.view(|t| {
                let (Some(t), Some(ui)) = (t, ui.upgrade()) else { return };
                let mut u = ui.lock().unwrap_or_else(|e| e.into_inner());
                if u.notice_gen != gen {
                    return; // a later notice took over
                }
                u.write(&screen_redraw(t));
                u.displaced = false;
                u.paint(t);
            });
        });
    }
}

/// Tells a viewer who owns the session and how to take it. The owner may be
/// another window on this device, which has the same name.
pub fn viewer_notice(owner: &str, self_id: &str) -> String {
    let mut who = owner_name(owner);
    if !owner.is_empty() && who == owner_name(self_id) {
        who = "another window on this device".into();
    }
    format!(" viewing: {who} owns this session; Ctrl-T takes over ")
}

/// The UI of one attached session: the mirror, the drawing state and the
/// session, shared by the stream's callbacks and the input thread.
#[derive(Clone)]
pub struct Attached {
    pub ui: Arc<Mutex<Ui>>,
    pub m: Mirror,
    pub s: Arc<Mutex<Option<Session>>>,
    pub id: String,
    /// Wakes the ticker (attach.rs) when predictions start pending.
    pub wake: Arc<Notify>,
    /// What the local terminal said of its appearance.
    look: Arc<Mutex<Look>>,
}

/// The local terminal's appearance, from its reports.
#[derive(Default, Clone, PartialEq)]
struct Look {
    /// Dark when true.
    scheme: Option<bool>,
    colors: osewa_client::TermColors,
}

fn lock(ui: &Mutex<Ui>) -> MutexGuard<'_, Ui> {
    ui.lock().unwrap_or_else(|e| e.into_inner())
}

impl Attached {
    pub fn new(mut ui: Ui, id: String) -> Attached {
        ui.id = id.clone();
        let ui = Arc::new(Mutex::new(ui));
        let (u1, u2, u3, u4, u5) = (ui.clone(), ui.clone(), ui.clone(), ui.clone(), ui.clone());
        let m = Mirror::new(osewa_mirror::Options {
            on_ready: Some(Box::new(move |t| lock(&u1).ready(t))),
            on_snapshot_end: Some(Box::new(move |t| lock(&u5).snapshot_end(t))),
            on_output: Some(Box::new(move |b| lock(&u2).output(b))),
            on_applied: Some(Box::new(move |t| match t {
                Some(t) => lock(&u3).applied(t),
                None => lock(&u3).flush(),
            })),
            on_event: Some(Box::new(move |e, live| lock(&u4).event(e, live))),
        });
        lock(&ui).later = Some(Later { m: m.clone(), ui: Arc::downgrade(&ui) });
        let s = lock(&ui).session.clone();
        Attached { ui, m, s, id, wake: Arc::default(), look: Arc::default() }
    }

    pub fn session(&self) -> Option<Session> {
        self.s.lock().unwrap().clone()
    }

    /// Shows a notice now, over the mirror.
    pub fn notice(&self, msg: &str) {
        self.m.view(|t| lock(&self.ui).notice(t.map(|t| &*t), msg));
    }

    /// Checks predictions for timeouts, while no output arrives.
    pub fn tick(&self) {
        self.m.view(|t| {
            if let Some(t) = t {
                lock(&self.ui).tick(t)
            }
        });
    }

    /// When to tick next (Ui::next_tick). Under the mirror lock, as send's
    /// arm_ticker is, so a wake can't fall between the two.
    pub fn next_tick(&self) -> Option<Instant> {
        self.m.view(|_| lock(&self.ui).next_tick())
    }

    /// Predicts and draws what typing text will show.
    pub fn predict(&self, text: &str) {
        self.m.view(|t| {
            let mut u = lock(&self.ui);
            u.typed_at = Some(Instant::now());
            if let Some(t) = t {
                u.pred.input(t, text.as_bytes());
                u.paint(t);
            }
            if u.arm_ticker() {
                self.wake.notify_one();
            }
        });
    }

    /// Tells the holder the local terminal's appearance, as far as it is
    /// known, when this client owns the session.
    pub async fn send_appearance(&self) {
        let look = self.look.lock().unwrap().clone();
        let Some(s) = self.session() else { return };
        if s.owner() != self.id {
            return;
        }
        if let Some(dark) = look.scheme {
            let _ = osewa_client::timeout(Duration::from_secs(5), s.color_scheme(dark)).await;
        }
        if look.colors != osewa_client::TermColors::default() {
            let _ = osewa_client::timeout(Duration::from_secs(5), s.colors(&look.colors)).await;
        }
    }

    /// Sets this terminal's appearance as its app knows it, rather than as
    /// reported (report); send_appearance tells the holder.
    pub fn set_appearance(&self, dark: Option<bool>, colors: osewa_client::TermColors) {
        *self.look.lock().unwrap() = Look { scheme: dark, colors };
    }

    /// A round trip sample, for the predictor; it is used with the mirror
    /// locked.
    pub fn rtt(&self, d: Duration) {
        self.m.view(|_| lock(&self.ui).pred.rtt(d));
    }
}

impl Attached {
    /// Predicts and sends typed text. A viewer's text is dropped with a
    /// notice, except that Ctrl-T takes over and the keys after it are
    /// sent.
    pub fn type_text(&self, text: &str) {
        let Some(s) = self.session() else { return };
        let owner = s.owner();
        if owner != self.id {
            if let Some((_, rest)) = text.split_once(TAKE_KEY) {
                self.take_over();
                if !rest.is_empty() {
                    self.type_text(rest);
                }
                return;
            }
            self.notice(&viewer_notice(&owner, &self.id));
            return;
        }
        self.predict(text);
        let _ = s.send_text(text);
    }

    /// Takes the owner from the session, as attaching and claiming it
    /// reported. The holder only sends an event when the owner changes, so
    /// one that attaches to its own session hears of it this way alone.
    pub fn sync_owner(&self) {
        let Some(s) = self.session() else { return };
        // Counted before the read: an event after it is newer than what the
        // read may return. Reading under the mirror can't settle that, as
        // events take the session's lock and then the mirror's. An event the
        // read does see reached the UI first, under the session's lock.
        let events = lock(&self.ui).owner_events;
        let owner = s.owner();
        self.m.view(|_| lock(&self.ui).read_owner(&owner, events));
    }

    /// Claims ownership for this client and says how it went. It blocks:
    /// never call it from the session's callbacks.
    pub fn take_over(&self) {
        let msg = match self.own() {
            Ok(()) => " you own this session now ".to_string(),
            Err(e) => format!(" taking over failed: {e} "),
        };
        self.m.view(|t| {
            let mut u = lock(&self.ui);
            u.notice_at = None; // always show how it went
            u.notice(t.map(|t| &*t), &msg);
        });
    }

    /// Claims ownership for this client, then tells the holder this
    /// terminal's appearance. It blocks, as take_over does.
    pub fn own(&self) -> Result<(), String> {
        let Some(s) = self.session() else { return Err("not attached".into()) };
        match osewa_client::block_on(osewa_client::timeout(Duration::from_secs(5), s.own())) {
            Some(Ok(())) => {
                self.sync_owner();
                osewa_client::block_on(self.send_appearance());
                Ok(())
            }
            Some(Err(e)) => Err(e.to_string()),
            None => Err("context deadline exceeded".into()),
        }
    }

    /// Takes in what the local terminal reported about itself, and tells
    /// the holder when its appearance changed. It blocks, as take_over
    /// does.
    pub fn report(&self, reports: &[Report]) {
        let changed = {
            let mut look = self.look.lock().unwrap();
            let old = look.clone();
            for r in reports {
                match *r {
                    Report::Scheme(dark) => look.scheme = Some(dark),
                    Report::Foreground(c) => look.colors.foreground = Some(c),
                    Report::Background(c) => look.colors.background = Some(c),
                    Report::Cursor(c) => look.colors.cursor = Some(c),
                    Report::Palette(i, c) => look.colors.palette[i as usize] = Some(c),
                    Report::Clipboard(ref data) => {
                        if let Some(s) = self.session() {
                            let _ = osewa_client::block_on(osewa_client::timeout(Duration::from_secs(5), s.clipboard(data)));
                        }
                    }
                }
            }
            *look != old
        };
        if changed {
            osewa_client::block_on(self.send_appearance());
        }
    }

    /// Repaints the whole screen, scrollback included (Ui::redraw).
    pub fn redraw(&self) {
        self.m.view(|t| {
            if let Some(t) = t {
                lock(&self.ui).redraw(t)
            }
        });
    }
}
