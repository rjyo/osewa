//! The attach UI: one attached session drawn on the
//! local terminal, with predictions over it and notices.

use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use osewa_client::Session;
use osewa_ghostty::Terminal;
use osewa_mirror::{InputMode, Mirror, Predictor};
use osewa_wire as wire;
use tokio::sync::Notify;

use crate::cli::Ctx;
use crate::attach::Report;
use crate::commands::owner_name;
use crate::render::*;
use crate::status::status_text;
use crate::vtfilter::{printable, QueryFilter};

/// How long a notice stays on the bottom row, and how often one can show.
pub(crate) const NOTICE_FOR: Duration = Duration::from_secs(3);
pub(crate) const NOTICE_EVERY: Duration = Duration::from_secs(1);
/// How long after a keystroke status notices wait.
const TYPING_QUIET: Duration = Duration::from_secs(2);

/// Takes ownership when typed by a viewer. A viewer's keys are dropped
/// anyway, so it needs no escape; the owner's Ctrl-T reaches the program.
const TAKE_KEY: char = '\x14';

/// What the UI draws with, made to repaint later (a notice's timer).
struct Later {
    m: Mirror,
    ui: Weak<Mutex<Ui>>,
}

/// One attached session's drawing state. Everything that writes runs with
/// the mirror locked (the mirror's callbacks, or inside Mirror::view), and
/// then this, so writes never interleave.
pub(crate) struct Ui {
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
        }
    }

    /// Passes the remote's Kitty graphics to the local terminal, and redraws
    /// its images along with the screen.
    pub fn with_graphics(mut self) -> Ui {
        self.filter.graphics = true;
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
        let (colors, colored) = colors_redraw(t, self.colored || std::mem::take(&mut self.filter.passed_colors));
        self.colored = colored;
        self.write(&colors);
        if self.filter.graphics {
            let draw = images_draw(t, &mut self.sent);
            self.write(&draw);
        }
        self.flush();
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
pub(crate) fn viewer_notice(owner: &str, self_id: &str) -> String {
    let mut who = owner_name(owner);
    if !owner.is_empty() && who == owner_name(self_id) {
        who = "another window on this device".into();
    }
    format!(" viewing: {who} owns this session; Ctrl-T takes over ")
}

/// Where forward_input sends what the user does.
pub(crate) trait InputHandler {
    fn send(&self, text: &str);
    fn take(&self);
    /// What the local terminal reported about itself.
    fn reports(&self, reports: &[Report]);
    fn flush(&self, ctx: &Ctx);
}

/// The UI of one attached session: the mirror, the drawing state and the
/// session, shared by the stream's callbacks and the input thread.
#[derive(Clone)]
pub(crate) struct Attached {
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
    pub fn new(ui: Ui, id: String) -> Attached {
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
        Attached { ui, m, s: Arc::default(), id, wake: Arc::default(), look: Arc::default() }
    }

    fn session(&self) -> Option<Session> {
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

    /// A round trip sample, for the predictor; it is used with the mirror
    /// locked.
    pub fn rtt(&self, d: Duration) {
        self.m.view(|_| lock(&self.ui).pred.rtt(d));
    }
}

impl InputHandler for Attached {
    /// Predicts and sends typed text. A viewer's text is dropped with a
    /// notice, except that Ctrl-T takes over and the keys after it are
    /// sent.
    fn send(&self, text: &str) {
        let Some(s) = self.session() else { return };
        let owner = s.owner();
        if owner != self.id {
            if let Some((_, rest)) = text.split_once(TAKE_KEY) {
                self.take();
                if !rest.is_empty() {
                    self.send(rest);
                }
                return;
            }
            self.notice(&viewer_notice(&owner, &self.id));
            return;
        }
        self.predict(text);
        let _ = s.send_text(text);
    }

    /// Claims ownership for this client.
    fn take(&self) {
        let Some(s) = self.session() else { return };
        let msg = match osewa_client::block_on(osewa_client::timeout(Duration::from_secs(5), s.own())) {
            Some(Ok(())) => {
                osewa_client::block_on(self.send_appearance());
                " you own this session now ".to_string()
            }
            Some(Err(e)) => format!(" taking over failed: {e} "),
            None => " taking over failed: context deadline exceeded ".to_string(),
        };
        self.m.view(|t| {
            let mut u = lock(&self.ui);
            u.notice_at = None; // always show how it went
            u.notice(t.map(|t| &*t), &msg);
        });
    }

    fn reports(&self, reports: &[Report]) {
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
                }
            }
            *look != old
        };
        if changed {
            osewa_client::block_on(self.send_appearance());
        }
    }

    fn flush(&self, ctx: &Ctx) {
        if let Some(s) = self.session() {
            let _ = osewa_client::block_on(ctx.run(s.flush()));
        }
    }
}
