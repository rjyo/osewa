use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::fd::RawFd;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use osewa_ghostty as ghostty;
use osewa_wire as wire;
use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::input::PtyInput;
use crate::log::{OutputLog, Record, REC_EVENT, REC_OUTPUT};
use crate::pty::{self, InputMode};
use crate::queue::{Clock, SendQueue};
use crate::trace::Tracer;
use crate::{cell_px, pixels, valid_size, Config, MAX_RECORD_DATA};

/// Caps one connection's parked input payloads; past it events are acked
/// and dropped. Clients keep less than half this unacked (docs/PROTOCOL.md,
/// Input), so they never reach it.
pub(crate) const MAX_PARKED_INPUT: usize = 1 << 20;
/// Bounds SNAPSHOT_DATA and SNAPSHOT_HISTORY payloads.
const CHUNK_SIZE: usize = 64 << 10;

/// What a terminal callback raised inside term.write, in order.
pub(crate) enum Raised {
    Title(String),
    /// A clipboard read (OSC 52 or Kitty's OSC 5522), waiting for its
    /// answer.
    ClipboardRead(ghostty::ClipboardRead),
    Status(ghostty::Status),
    Event(wire::EventMsg),
}

pub(crate) struct Stream {
    /// Unique for the holder's life: what "this stream" means to a timer.
    pub uid: u64,
    pub id: u32,
    /// Snapshots sent; the next one's generation.
    pub snaps: u32,
    pub cols: i64,
    pub rows: i64,
    /// The client's text area in pixels, 0 when unknown.
    pub width: i64,
    pub height: i64,
    /// The last snapshot's position; earlier records are in it.
    pub from: u64,
    // Catch-up (docs/PROTOCOL.md, Lagging clients and catch-up).
    /// Sending screen snapshots; scrollback is owed.
    pub catch_up: bool,
    /// In catch-up, but records follow the last screen.
    pub streaming: bool,
    /// Record bytes since the last screen, while screens only.
    pub skipped: i64,
    /// The last screen's SNAPSHOT_DATA bytes.
    pub screen_bytes: i64,
    /// A screen snapshot is queued and not yet written.
    pub screen_pending: bool,
    /// Fell behind again while one was pending.
    pub need_screen: bool,
    /// The pending screen was cut for the owner's input.
    pub input_pending: bool,
    /// The needed screen is owed to the owner's input.
    pub need_input: bool,
    /// The pending screen abandoned a partly sent one.
    pub abandoner: bool,
    /// Bumped by every snapshot, to cancel a held screen.
    pub hold_token: u64,
    /// Bumped to cancel a pending exit from catch-up.
    pub catch_token: u64,
    pub ack_gen: u32,
    pub ack_end: u64,
}

impl Stream {
    pub fn new(uid: u64, id: u32, cols: i64, rows: i64, width: i64, height: i64) -> Stream {
        Stream {
            uid,
            id,
            snaps: 0,
            cols,
            rows,
            width,
            height,
            from: 0,
            catch_up: false,
            streaming: false,
            skipped: 0,
            screen_bytes: 0,
            screen_pending: false,
            need_screen: false,
            input_pending: false,
            need_input: false,
            abandoner: false,
            hold_token: 0,
            catch_token: 0,
            ack_gen: 0,
            ack_end: 0,
        }
    }

    /// Sending screens only; one that streams records again gets its echo
    /// as output.
    pub fn screens_only(&self) -> bool {
        self.catch_up && !self.streaming
    }
}

pub(crate) struct Parked {
    pub input: wire::Input,
    pub size: usize,
    /// An event that didn't fit: acked in its turn but never applied.
    pub drop: bool,
}

pub(crate) struct Conn {
    /// The connection's key in the session map.
    pub cid: u64,
    pub q: Rc<RefCell<SendQueue>>,
    pub wake: Rc<Notify>,
    /// Input waiting, unapplied and unacked, for the child to read stdin.
    /// The reader never stops reading, or CREDIT would stall too.
    pub parked: VecDeque<Parked>,
    pub parked_bytes: usize,
    pub client: String,
    pub stream: Option<Stream>,
    pub writer: Option<JoinHandle<()>>,
    pub reader: Option<JoinHandle<()>>,
}

impl Conn {
    pub fn new(cid: u64, q: Rc<RefCell<SendQueue>>) -> Conn {
        Conn {
            cid,
            q,
            wake: Rc::new(Notify::new()),
            parked: VecDeque::new(),
            parked_bytes: 0,
            client: String::new(),
            stream: None,
            writer: None,
            reader: None,
        }
    }

    /// The connection's stream if id names it; zero means its only stream.
    fn stream_for(&mut self, id: u32) -> Option<&mut Stream> {
        self.stream.as_mut().filter(|st| id == 0 || id == st.id)
    }

    fn st(&mut self) -> &mut Stream {
        self.stream.as_mut().expect("stream")
    }
}

/// Everything but the connections, so a function can hold the core and one
/// connection at once.
pub(crate) struct Core {
    pub cfg: Config,
    pub epoch: String,
    created: i64,

    pub term: ghostty::Terminal,
    fx: Rc<RefCell<Vec<Raised>>>,
    pub log: OutputLog,
    pub master: RawFd,
    pub input: Rc<RefCell<PtyInput>>,
    pub input_wake: Rc<Notify>,
    /// The owner's appearance, dark when true, once its client said; the
    /// terminal answers DSR 996 from it.
    color_scheme: Rc<Cell<Option<bool>>>,
    /// OSC 52 reads waiting for the owner's answer, oldest first.
    clipboard_reads: VecDeque<ghostty::ClipboardRead>,
    pub pid: i32,
    pub reaped: Rc<Cell<bool>>,

    pub owner: String,
    /// Bumped to cancel a pending ownership lapse.
    owner_token: u64,
    pub input_seq: HashMap<String, u64>,
    title: String,
    /// Events raised inside term.write, until the output that caused them is
    /// in the log.
    pending: Vec<wire::EventMsg>,

    mode: InputMode,
    mode_known: bool,
    pub tr: Option<Rc<Tracer>>,
    /// The latest root OSC 7501 record.
    pub status: Option<wire::ProgramStatus>,
    prompts: PromptScanner,
    // Input-triggered screens: echo_wait is set when the owner's input is
    // applied while a stream is catching up, echo_armed once it has reached
    // the PTY. The first read from echo_at on (or echo_quiet later without
    // one) cuts the screen.
    echo_wait: bool,
    echo_armed: bool,
    echo_at: Instant,
    echo_token: u64,
    /// The owner's last input during catch-up.
    last_input: Option<Instant>,
    /// When the session last had output or an attached stream, for the idle
    /// timeout.
    pub quiet_since: Instant,

    pub exited: bool,
    pub exit_code: i64,
    pub killed: bool,
    /// The loop has stopped: timers and connections do nothing more.
    pub done: bool,
    /// Set inside a (core, conn) function that would need every connection;
    /// the session runs it once the event is handled.
    need_lapse: bool,
    next_uid: u64,
    pub clock: Clock,
    pub me: Weak<RefCell<Session>>,
    /// Notified when the exit retention has passed.
    pub retention: Rc<Notify>,
    /// Notified once the child's exit is handled.
    pub exited_notify: Rc<Notify>,
}

pub(crate) struct Session {
    pub core: Core,
    pub conns: BTreeMap<u64, Conn>,
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The terminal's default colors with a look's over them; its empty
/// colors leave the terminal's.
fn merged_colors(term: &ghostty::Terminal, look: &wire::Look) -> Result<ghostty::Colors, String> {
    let color = |s: &str| match s {
        "" => Ok(None),
        s => wire::parse_rgb_hex(s).map(Some).ok_or(format!("bad color {}", wire::quote(s))),
    };
    let mut d = term.default_colors();
    for (slot, s) in [(&mut d.foreground, &look.foreground), (&mut d.background, &look.background), (&mut d.cursor, &look.cursor)] {
        if let Some(c) = color(s)? {
            *slot = Some(c);
        }
    }
    for (slot, s) in d.palette.iter_mut().zip(&look.palette) {
        if let Some(c) = color(s)? {
            *slot = c;
        }
    }
    Ok(d)
}

fn reason_name(r: &str) -> serde_json::Value {
    json!(r)
}

impl Session {
    /// Builds the session and its terminal, without a child.
    pub fn new(cfg: Config, master: RawFd, clock: Clock) -> Result<Rc<RefCell<Session>>, String> {
        let tr = Tracer::open(&cfg.trace_dir).map_err(|e| e.to_string())?.map(Rc::new);
        let fx: Rc<RefCell<Vec<Raised>>> = Rc::default();
        let input = Rc::new(RefCell::new(PtyInput::new(tr.clone())));
        let input_wake = Rc::new(Notify::new());
        let color_scheme = Rc::new(Cell::new(None));
        let callbacks = {
            let (f1, f2, f3, f4, f5, f6) = (fx.clone(), fx.clone(), fx.clone(), fx.clone(), fx.clone(), fx.clone());
            let scheme = color_scheme.clone();
            let (inp, wake) = (input.clone(), input_wake.clone());
            ghostty::Callbacks {
                write_pty: Some(Box::new(move |b| {
                    inp.borrow_mut().push(b, true);
                    wake.notify_one();
                })),
                bell: Some(Box::new(move || f1.borrow_mut().push(Raised::Event(wire::EventMsg::kind(wire::EVENT_BELL))))),
                title: Some(Box::new(move |t| f2.borrow_mut().push(Raised::Title(t)))),
                clipboard: Some(Box::new(move |b| {
                    let ev = wire::EventMsg { kind: wire::EVENT_CLIPBOARD.into(), data: b.to_vec(), ..Default::default() };
                    f3.borrow_mut().push(Raised::Event(ev))
                })),
                status: Some(Box::new(move |st| f4.borrow_mut().push(Raised::Status(st)))),
                notify: Some(Box::new(move |title, msg| {
                    f5.borrow_mut().push(Raised::Event(wire::EventMsg { kind: wire::EVENT_NOTIFY.into(), title, msg, ..Default::default() }))
                })),
                color_scheme: Some(Box::new(move || scheme.get())),
                clipboard_read: Some(Box::new(move |r| f6.borrow_mut().push(Raised::ClipboardRead(r)))),
            }
        };
        let mut term = ghostty::Terminal::new(
            cfg.cols as usize,
            cfg.rows as usize,
            ghostty::Options { scrollback_bytes: cfg.scrollback_bytes, continuation: true, images: true, callbacks },
        )
        .map_err(|e| e.to_string())?;
        let (cw, ch) = cell_px(cfg.cols, cfg.rows, cfg.width, cfg.height);
        term.resize_cells(cfg.cols as usize, cfg.rows as usize, cw, ch).map_err(|e| e.to_string())?;
        // The creator's look, before any program can ask for it.
        cfg.look.check().map_err(|e| format!("holder: look: {e}"))?;
        let d = merged_colors(&term, &cfg.look).map_err(|e| format!("holder: look: {e}"))?;
        term.set_default_colors(&d).map_err(|e| e.to_string())?;
        if !cfg.look.color_scheme.is_empty() {
            color_scheme.set(Some(cfg.look.color_scheme == "dark"));
        }
        let mut e = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut e);
        let now = clock();
        let core = Core {
            log: OutputLog::new(cfg.log_bytes as u64),
            cfg,
            epoch: hex::encode(e),
            created: now_unix(),
            term,
            fx,
            master,
            input,
            input_wake,
            color_scheme,
            clipboard_reads: VecDeque::new(),
            pid: 0,
            reaped: Rc::default(),
            owner: String::new(),
            owner_token: 0,
            input_seq: HashMap::new(),
            title: String::new(),
            pending: Vec::new(),
            mode: InputMode::default(),
            mode_known: false,
            tr,
            status: None,
            prompts: PromptScanner::default(),
            echo_wait: false,
            echo_armed: false,
            echo_at: now,
            echo_token: 0,
            last_input: None,
            quiet_since: now,
            exited: false,
            exit_code: 0,
            killed: false,
            done: false,
            need_lapse: false,
            next_uid: 0,
            clock,
            me: Weak::new(),
            retention: Rc::new(Notify::new()),
            exited_notify: Rc::new(Notify::new()),
        };
        let s = Rc::new(RefCell::new(Session { core, conns: BTreeMap::new() }));
        s.borrow_mut().core.me = Rc::downgrade(&s);
        Ok(s)
    }

    pub fn split(&mut self, cid: u64) -> Option<(&mut Core, &mut Conn)> {
        let Session { core, conns } = self;
        conns.get_mut(&cid).map(|c| (core, c))
    }

    /// Runs what a handled event left for the whole session, and wakes the
    /// writers. Every entry point ends here.
    pub fn finish(&mut self) {
        if self.core.need_lapse {
            self.core.need_lapse = false;
            self.check_lapse();
        }
        for c in self.conns.values() {
            c.wake.notify_one();
        }
    }

    pub fn attached(&self) -> i64 {
        self.conns.values().filter(|c| c.stream.is_some()).count() as i64
    }

    pub fn handle_output(&mut self, b: &[u8]) {
        let now = (self.core.clock)();
        self.core.quiet_since = now;
        // A mode change is logged before the output it came with: a program
        // turns echo off and then prints its password prompt, and a
        // predicting client must know before it sees the prompt.
        self.poll_mode(false);
        // The terminal takes the bytes before the log does, so a snapshot cut
        // while broadcasting (on lag) already contains them.
        self.core.write_term(b);
        // Records are cut small so frames can start small. Every piece, and
        // every event the bytes raised, is in the log before any is
        // broadcast, so a lag snapshot cut mid-way covers them all
        // (send_record skips what a snapshot covers) and effects of skipped
        // output never fire.
        let mut recs: Vec<Record> = b.chunks(MAX_RECORD_DATA).map(|p| self.core.log.append(REC_OUTPUT, p, false, None)).collect();
        for ev in std::mem::take(&mut self.core.pending) {
            recs.push(self.core.log_event(&ev));
        }
        for r in &recs {
            self.broadcast(r);
        }
        if self.core.echo_armed && (self.core.clock)() >= self.core.echo_at {
            self.input_screen();
        }
        self.finish();
    }

    pub fn emit(&mut self, ev: wire::EventMsg) {
        let r = self.core.log_event(&ev);
        self.broadcast(&r);
    }

    fn broadcast(&mut self, r: &Record) {
        let Session { core, conns } = self;
        for c in conns.values_mut() {
            if c.stream.is_some() {
                core.send_record(c, r);
            }
        }
    }

    pub fn handle_exit(&mut self, code: i64) {
        self.core.exited = true;
        self.core.exit_code = code;
        self.core.end_active_status();
        self.emit(wire::EventMsg { kind: wire::EVENT_EXIT.into(), code: Some(code), ..Default::default() });
        let reason = self.core.end_reason();
        let Session { core, conns } = self;
        for c in conns.values_mut() {
            if c.stream.is_some() {
                core.end_stream(c, reason);
            }
        }
        // Input waiting on the PTY has nowhere to go now.
        self.core.input.borrow_mut().close();
        self.core.input_wake.notify_one();
        self.unpark();
        let retention = self.core.retention.clone();
        let wait = self.core.cfg.exit_retention;
        tokio::task::spawn_local(async move {
            tokio::time::sleep(wait).await;
            retention.notify_one();
        });
        self.core.exited_notify.notify_one();
        self.finish();
    }

    fn catching_up(&self) -> bool {
        self.conns.values().any(|c| c.stream.as_ref().is_some_and(|st| st.screens_only()))
    }

    /// Whether some stream would show the owner's next echo sooner in a
    /// screen than behind its queued output (echo_behind_backlog).
    fn backlogged(&self) -> bool {
        self.conns.values().any(echo_behind_backlog)
    }

    /// Arms the input-triggered screen once the owner's input has all
    /// reached the PTY: from then on, the child's echo is in its output.
    pub fn input_written(&mut self) {
        let core = &mut self.core;
        if !core.echo_wait || core.input.borrow().pending() > 0 {
            return;
        }
        core.echo_wait = false;
        core.echo_armed = true;
        core.echo_at = (core.clock)() + core.cfg.tune.echo_settle;
        core.echo_token += 1;
        let token = core.echo_token;
        core.after(core.cfg.tune.echo_settle + core.cfg.tune.echo_quiet, move |s| {
            if s.core.echo_armed && s.core.echo_token == token {
                s.input_screen();
            }
        });
    }

    /// Cuts a screen for every catching-up stream right after the owner
    /// typed, so the echo goes out in the next screen rather than the one
    /// after. It preempts a screen the flood cut: whatever of that hasn't
    /// been written yet is dropped (the client discards a generation that a
    /// new SNAPSHOT_BEGIN interrupts). A screen written but not yet credited
    /// is followed at once instead of a round trip later. Counted bytes
    /// still stay inside the credit window. It never drops part of another
    /// input screen, so fast typing can't keep the client from finishing
    /// one.
    pub fn input_screen(&mut self) {
        self.core.echo_armed = false;
        let Session { core, conns } = self;
        for c in conns.values_mut() {
            match &c.stream {
                // Keeping up, or streaming again after catch-up: the echo is
                // a few bytes of output, and a screen would only cost link
                // time and hold off the full snapshot that restores
                // scrollback.
                Some(st) if st.screens_only() => {}
                // Streaming, but behind enough output that the echo would
                // come sooner in a screen: catch up now rather than once the
                // backlog has aged past the catch-up delay, which on a slow
                // link is half a second of a keystroke going unseen.
                _ if echo_behind_backlog(c) => {
                    core.behind(c, "input");
                    if let Some(st) = c.stream.as_mut() {
                        if st.need_screen {
                            st.need_input = true;
                        } else {
                            st.input_pending = true;
                        }
                    }
                    continue;
                }
                _ => continue,
            }
            c.st().catch_token += 1; // not caught up yet: a screen is on its way
            if c.q.borrow_mut().drop_queued_snapshot() {
                // One already queued behind a screen in progress: this one
                // replaces it.
                core.send_snapshot(c, false, "input");
                if let Some(st) = c.stream.as_mut() {
                    st.input_pending = true;
                }
                continue;
            }
            // A screen part way out, before READY, is abandoned
            // (docs/PROTOCOL.md): its unsent DATA is dropped and the new
            // SNAPSHOT_BEGIN goes at once. Except one that itself abandoned
            // another: that one finishes, with the new one queued right
            // behind it, so typing faster than a screen takes can't keep the
            // client from ever finishing one. "Part way out" starts with its
            // SNAPSHOT_BEGIN: from then on the client has seen it, DATA or
            // not.
            let (pending, prev_abandoner) = (c.st().screen_pending, c.st().abandoner);
            let (begun, partial) = {
                let q = c.q.borrow();
                let begun = pending && !q.begin_queued();
                (begun, begun && q.snapshot_queued())
            };
            if partial && prev_abandoner {
                c.q.borrow_mut().drop_records();
                core.send_snapshot(c, false, "input");
                if let Some(st) = c.stream.as_mut() {
                    st.input_pending = true;
                }
                continue;
            }
            // Replacing one the client never saw keeps its standing.
            let abandoner = partial || (pending && !begun && prev_abandoner);
            c.q.borrow_mut().drop_data();
            core.send_snapshot(c, false, "input");
            if let Some(st) = c.stream.as_mut() {
                st.input_pending = true;
                st.abandoner = abandoner;
            }
        }
    }

    /// Catches a backlog going stale with no new output to notice it, as
    /// when a flood stops.
    fn check_behind(&mut self) {
        let (lag, delay) = (self.core.cfg.lag_bytes as i64, self.core.cfg.catch_up_delay);
        let Session { core, conns } = self;
        for c in conns.values_mut() {
            let behind = match &c.stream {
                Some(st) => !st.need_screen && c.q.borrow().lagging(lag, delay),
                None => false,
            };
            if behind {
                core.behind(c, "behind");
            }
        }
    }

    /// Ends a session nobody watches and nothing writes to.
    fn check_idle(&mut self) {
        let idle = self.core.cfg.idle;
        if !idle.is_zero()
            && !self.core.exited
            && self.attached() == 0
            && (self.core.clock)().saturating_duration_since(self.core.quiet_since) >= idle
        {
            self.core.terminate();
        }
    }

    /// The loop's periodic work.
    pub fn tick(&mut self) {
        if self.attached() > 0 {
            self.core.quiet_since = (self.core.clock)();
            self.poll_mode(false);
            self.check_behind();
        }
        self.check_idle();
        self.finish();
    }

    /// Rereads the PTY's echo and canonical flags and logs an input_mode
    /// event when they change, or always when forced (on attach, so the new
    /// stream learns the mode even from a snapshot).
    fn poll_mode(&mut self, force: bool) {
        let Ok(m) = pty::read_mode(self.core.master) else { return };
        if self.core.mode_known && m == self.core.mode && !force {
            return;
        }
        self.core.mode = m;
        self.core.mode_known = true;
        self.emit(input_mode_event(m));
    }

    pub fn drop_conn(&mut self, cid: u64) {
        let Some(c) = self.conns.remove(&cid) else { return };
        c.q.borrow_mut().close();
        c.wake.notify_one();
        if c.stream.is_some() {
            self.core.quiet_since = (self.core.clock)();
            self.check_lapse();
        }
        self.finish();
    }

    pub fn handle_frame(&mut self, cid: u64, f: wire::Frame) {
        match f.ty {
            wire::TYPE_HELLO => {
                if let (Ok(h), Some(c)) = (f.decode::<wire::Hello>(), self.conns.get_mut(&cid)) {
                    c.client = h.client_id;
                }
            }
            wire::TYPE_REQUEST => match f.decode::<wire::Request>() {
                Ok(req) => self.handle_request(cid, req),
                Err(e) => {
                    if let Some(c) = self.conns.get(&cid) {
                        let resp = wire::Response::error(0, wire::ERR_BAD_REQUEST, e.to_string());
                        c.q.borrow_mut().control(wire::json(wire::TYPE_RESPONSE, 0, &resp));
                    }
                }
            },
            wire::TYPE_INPUT => {
                let Some(c) = self.conns.get(&cid) else { return };
                if c.stream.as_ref().is_none_or(|st| st.id != f.stream) {
                    return;
                }
                let Ok(input) = f.decode::<wire::Input>() else { return };
                if let Some(tr) = &self.core.tr {
                    tr.emit("input_read", &[("client_id", json!(c.client)), ("stream", json!(f.stream)), ("seq", json!(input.seq))]);
                }
                self.handle_input(cid, input, f.payload.len());
            }
            wire::TYPE_CREDIT => {
                let Ok(n) = wire::parse_credit(&f.payload) else { return };
                let Some((core, c)) = self.split(cid) else { return };
                // The queue checks the stream, not c.stream: an ended stream
                // still needs credit to deliver its final output and
                // STREAM_END.
                let done = c.q.borrow_mut().credit(f.stream, n);
                if let Some(gen) = done {
                    core.snapshot_done(c, gen);
                }
            }
            wire::TYPE_ACK => {
                let Ok((gen, end)) = wire::parse_ack(&f.payload) else { return };
                if let Some(st) = self.conns.get_mut(&cid).and_then(|c| c.stream.as_mut()).filter(|st| st.id == f.stream) {
                    st.ack_gen = gen;
                    st.ack_end = end;
                }
            }
            _ => {}
        }
        self.finish();
    }

    /// The writer reports that a SNAPSHOT_END it wrote is credited.
    pub fn snapshot_done(&mut self, cid: u64, gen: u32) {
        if let Some((core, c)) = self.split(cid) {
            core.snapshot_done(c, gen);
        }
        self.finish();
    }

    fn handle_request(&mut self, cid: u64, req: wire::Request) {
        let Some(c) = self.conns.get_mut(&cid) else { return };
        if c.client.is_empty() {
            c.client = req.client_id.clone();
        }
        let mut resp = wire::Response { id: req.id, ..Default::default() };
        let mut then = None;
        let res: Result<(), (&str, String)> = match req.method.as_str() {
            wire::METHOD_INFO => {
                resp.sessions = vec![self.info()];
                Ok(())
            }
            wire::METHOD_ATTACH => self.attach(cid, &req, &mut resp).map(|plan| then = Some(plan)),
            wire::METHOD_DETACH => {
                if let Some((core, c)) = self.split(cid) {
                    if c.stream_for(req.stream).is_some() {
                        core.end_stream(c, wire::END_DETACHED);
                        self.check_lapse();
                    }
                }
                Ok(())
            }
            wire::METHOD_OWN => self.own(cid, &req, &mut resp),
            wire::METHOD_RESIZE => self.resize_req(cid, &req),
            wire::METHOD_COLOR_SCHEME => self.color_scheme_req(cid, &req),
            wire::METHOD_COLORS => self.colors_req(cid, &req),
            wire::METHOD_CLIPBOARD => self.clipboard_req(cid, &req),
            wire::METHOD_KILL => self.kill(&req),
            m => Err((wire::ERR_BAD_REQUEST, format!("unknown method {}", wire::quote(m)))),
        };
        if let Err((code, msg)) = res {
            resp = wire::Response::error(req.id, code, msg);
            then = None;
        }
        if let Some(c) = self.conns.get(&cid) {
            c.q.borrow_mut().control(wire::json(wire::TYPE_RESPONSE, 0, &resp));
        }
        if let Some(plan) = then {
            self.attach_then(cid, plan);
        }
    }

    pub fn info(&self) -> wire::SessionInfo {
        let core = &self.core;
        let (cols, rows) = core.term.size();
        let attached = self.attached();
        let mut info = wire::SessionInfo {
            id: core.cfg.id.clone(),
            title: core.term.title(),
            cols: cols as i64,
            rows: rows as i64,
            created: core.created,
            exited: core.exited,
            exit_code: core.exit_code,
            owner: core.owner.clone(),
            attached,
            idle_seconds: 0,
            status: core.status.clone(),
            build: Some(wire::Build {
                version: wire::VERSION,
                ghostty: ghostty::COMMIT.into(),
                snapshot_version: ghostty::SNAPSHOT_VERSION,
            }),
            incompatible: String::new(),
            tag: core.cfg.tag.clone(),
            creator: core.cfg.creator.clone(),
        };
        if attached == 0 {
            info.idle_seconds = (core.clock)().saturating_duration_since(core.quiet_since).as_secs() as i64;
        }
        info
    }

    fn attach(&mut self, cid: u64, req: &wire::Request, resp: &mut wire::Response) -> Result<AttachPlan, (&'static str, String)> {
        if !req.session.is_empty() && req.session != self.core.cfg.id {
            return Err((wire::ERR_NOT_FOUND, format!("no session {}", req.session)));
        }
        if req.stream == 0 {
            return Err((wire::ERR_BAD_REQUEST, "stream 0".into()));
        }
        if !req.epoch.is_empty() && req.epoch != self.core.epoch {
            return Err((wire::ERR_ENDED, format!("epoch {} ended", req.epoch)));
        }
        let Some((core, c)) = self.split(cid) else { return Err((wire::ERR_INTERNAL, "connection gone".into())) };
        if c.stream.is_some() {
            core.end_stream(c, wire::END_DETACHED);
        }
        core.next_uid += 1;
        let (width, height) = (pixels(req.width), pixels(req.height));
        let st = Stream::new(core.next_uid, req.stream, req.cols, req.rows, width, height);
        let client = c.client.clone();
        if !self.core.exited {
            if req.own {
                self.set_owner(&client);
            }
            if !client.is_empty() && client == self.core.owner {
                self.core.owner_token += 1;
                self.resize(req.cols, req.rows, width, height);
            }
        }
        let mut recs = None;
        if !req.epoch.is_empty() {
            if let Some(p) = req.position {
                recs = self.core.log.since(p);
            }
        }
        resp.resumed = recs.is_some();
        resp.epoch = self.core.epoch.clone();
        resp.owner = self.core.owner.clone();
        resp.head = Some(self.core.log.end);
        Ok(AttachPlan { st, recs })
    }

    fn attach_then(&mut self, cid: u64, plan: AttachPlan) {
        let uid = plan.st.uid;
        let Some((core, c)) = self.split(cid) else { return };
        let id = plan.st.id;
        c.stream = Some(plan.st);
        {
            let mut q = c.q.borrow_mut();
            q.reset_flow(id);
            // A client process reusing its id learns where its seqs
            // continue.
            q.control(wire::input_ack(id, core.input_seq.get(&c.client).copied().unwrap_or(0)));
        }
        match plan.recs {
            Some(recs) => {
                for r in &recs {
                    if c.stream.is_some() {
                        core.send_record(c, r);
                    }
                }
            }
            None => core.send_snapshot(c, true, "attach"),
        }
        let still = |s: &Session| s.conns.get(&cid).and_then(|c| c.stream.as_ref()).is_some_and(|st| st.uid == uid);
        if !self.core.exited && still(self) {
            self.poll_mode(true);
        }
        if self.core.exited && still(self) {
            let reason = self.core.end_reason();
            if let Some((core, c)) = self.split(cid) {
                core.end_stream(c, reason);
            }
        }
    }

    fn own(&mut self, cid: u64, req: &wire::Request, resp: &mut wire::Response) -> Result<(), (&'static str, String)> {
        let c = self.conns.get_mut(&cid).ok_or((wire::ERR_INTERNAL, String::new()))?;
        let client = c.client.clone();
        let Some(st) = c.stream_for(req.stream) else {
            return Err((wire::ERR_BAD_REQUEST, format!("stream {} is not attached", req.stream)));
        };
        let (cols, rows, width, height) = (st.cols, st.rows, st.width, st.height);
        if self.core.exited {
            return Err((wire::ERR_ENDED, "session exited".into()));
        }
        self.set_owner(&client);
        self.resize(cols, rows, width, height);
        resp.owner = self.core.owner.clone();
        Ok(())
    }

    /// Takes the owner's light or dark appearance. A program that asked
    /// for changes (mode 2031) hears of one as a terminal would tell it.
    fn color_scheme_req(&mut self, cid: u64, req: &wire::Request) -> Result<(), (&'static str, String)> {
        let owner = self.core.owner.clone();
        let c = self.conns.get_mut(&cid).ok_or((wire::ERR_INTERNAL, String::new()))?;
        let client = c.client.clone();
        if c.stream_for(req.stream).is_none() {
            return Err((wire::ERR_BAD_REQUEST, format!("stream {} is not attached", req.stream)));
        }
        let dark = match req.color_scheme.as_str() {
            "dark" => true,
            "light" => false,
            s => return Err((wire::ERR_BAD_REQUEST, format!("color scheme {}: want light or dark", wire::quote(s)))),
        };
        if client.is_empty() || client != owner {
            return Err((wire::ERR_NOT_OWNER, "not the owner".into()));
        }
        let core = &mut self.core;
        if core.color_scheme.replace(Some(dark)) != Some(dark) && !core.exited && core.term.mode(2031, false) {
            let report: &[u8] = if dark { b"\x1b[?997;1n" } else { b"\x1b[?997;2n" };
            core.input.borrow_mut().push(report, true);
            core.input_wake.notify_one();
        }
        Ok(())
    }

    /// Answers the oldest clipboard read with the owner's clipboard, in the
    /// read's own protocol. With none waiting there is nothing to answer.
    fn clipboard_req(&mut self, cid: u64, req: &wire::Request) -> Result<(), (&'static str, String)> {
        let owner = self.core.owner.clone();
        let c = self.conns.get_mut(&cid).ok_or((wire::ERR_INTERNAL, String::new()))?;
        let client = c.client.clone();
        if c.stream_for(req.stream).is_none() {
            return Err((wire::ERR_BAD_REQUEST, format!("stream {} is not attached", req.stream)));
        }
        if client.is_empty() || client != owner {
            return Err((wire::ERR_NOT_OWNER, "not the owner".into()));
        }
        let core = &mut self.core;
        let Some(read) = core.clipboard_reads.pop_front() else { return Ok(()) };
        if core.exited {
            return Ok(());
        }
        core.input.borrow_mut().push(&read.answer(&req.data), false);
        core.input_wake.notify_one();
        Ok(())
    }

    /// Takes the owner's terminal colors as the defaults, which programs'
    /// color queries (OSC 4, 10, 11, 12) are answered with. What the owner
    /// didn't say stays as it was.
    fn colors_req(&mut self, cid: u64, req: &wire::Request) -> Result<(), (&'static str, String)> {
        let owner = self.core.owner.clone();
        let c = self.conns.get_mut(&cid).ok_or((wire::ERR_INTERNAL, String::new()))?;
        let client = c.client.clone();
        if c.stream_for(req.stream).is_none() {
            return Err((wire::ERR_BAD_REQUEST, format!("stream {} is not attached", req.stream)));
        }
        if !req.palette.is_empty() && req.palette.len() != 256 {
            return Err((wire::ERR_BAD_REQUEST, format!("palette of {} colors: want 256", req.palette.len())));
        }
        let d = merged_colors(&self.core.term, &wire::Look::of(req)).map_err(|e| (wire::ERR_BAD_REQUEST, e))?;
        if client.is_empty() || client != owner {
            return Err((wire::ERR_NOT_OWNER, "not the owner".into()));
        }
        self.core.term.set_default_colors(&d).map_err(|e| (wire::ERR_INTERNAL, e.to_string()))
    }

    fn resize_req(&mut self, cid: u64, req: &wire::Request) -> Result<(), (&'static str, String)> {
        let owner = self.core.owner.clone();
        let c = self.conns.get_mut(&cid).ok_or((wire::ERR_INTERNAL, String::new()))?;
        let client = c.client.clone();
        let Some(st) = c.stream_for(req.stream) else {
            return Err((wire::ERR_BAD_REQUEST, format!("stream {} is not attached", req.stream)));
        };
        if !valid_size(req.cols, req.rows) {
            return Err((wire::ERR_BAD_REQUEST, format!("bad size {}x{}", req.cols, req.rows)));
        }
        st.cols = req.cols;
        st.rows = req.rows;
        st.width = pixels(req.width);
        st.height = pixels(req.height);
        let (width, height) = (st.width, st.height);
        if client.is_empty() || client != owner {
            return Err((wire::ERR_NOT_OWNER, "not the owner".into()));
        }
        if !self.core.exited {
            self.resize(req.cols, req.rows, width, height);
        }
        Ok(())
    }

    pub fn set_owner(&mut self, client: &str) {
        // Taking ownership, even again, cancels a pending lapse.
        self.core.owner_token += 1;
        if client == self.core.owner {
            return;
        }
        self.core.owner = client.into();
        self.emit(wire::EventMsg { kind: wire::EVENT_OWNER.into(), owner: client.into(), ..Default::default() });
    }

    /// Starts the owner's grace period once its last stream is gone.
    fn check_lapse(&mut self) {
        let core = &mut self.core;
        if core.owner.is_empty() || core.exited {
            return;
        }
        if self.conns.values().any(|c| c.stream.is_some() && c.client == core.owner) {
            return;
        }
        core.owner_token += 1;
        let token = core.owner_token;
        core.after(core.cfg.owner_grace, move |s| {
            if s.core.owner_token == token && !s.core.exited {
                s.set_owner("");
            }
        });
    }

    /// Gives the PTY and the terminal the owner's size. The terminal goes
    /// first so output drawn for the new size lands on a terminal of that
    /// size. The cell size places images, so a change of font size alone is
    /// a resize too.
    fn resize(&mut self, cols: i64, rows: i64, width: i64, height: i64) {
        if !valid_size(cols, rows) {
            return;
        }
        let (cw, ch) = cell_px(cols, rows, width, height);
        let t = &mut self.core.term;
        if t.size() == (cols as usize, rows as usize) && t.cell_size() == (cw, ch) {
            return;
        }
        if t.resize_cells(cols as usize, rows as usize, cw, ch).is_err() {
            return;
        }
        let _ = pty::set_size(self.core.master, cols as u16, rows as u16, width as u16, height as u16);
        self.emit(wire::EventMsg { kind: wire::EVENT_RESIZE.into(), cols, rows, width, height, ..Default::default() });
    }

    fn kill(&mut self, req: &wire::Request) -> Result<(), (&'static str, String)> {
        if !req.session.is_empty() && req.session != self.core.cfg.id {
            return Err((wire::ERR_NOT_FOUND, format!("no session {}", req.session)));
        }
        let cfg = &mut self.core.cfg;
        cfg.exit_retention = cfg.exit_retention.min(crate::KILLED_RETENTION);
        if !self.core.exited {
            self.core.terminate();
        } else {
            // Exited already, its retention running: end it soon instead.
            let (retention, wait) = (self.core.retention.clone(), self.core.cfg.exit_retention);
            tokio::task::spawn_local(async move {
                tokio::time::sleep(wait).await;
                retention.notify_one();
            });
        }
        Ok(())
    }

    /// Applies parked input, in order, while the child has room.
    pub fn unpark(&mut self) {
        let cids: Vec<u64> = self.conns.keys().copied().collect();
        for cid in cids {
            while let Some(c) = self.conns.get_mut(&cid) {
                let full = self.core.input.borrow().full();
                let Some(front) = c.parked.front() else { break };
                if !front.drop && full {
                    break;
                }
                let p = c.parked.pop_front().unwrap();
                c.parked_bytes -= p.size;
                if p.drop {
                    self.ack(cid, p.input.seq);
                    continue;
                }
                self.apply_and_ack(cid, p.input);
            }
        }
    }

    /// The PTY writer made room for input.
    pub fn on_room(&mut self) {
        self.unpark();
        self.input_written();
        self.finish();
    }

    pub fn handle_input(&mut self, cid: u64, input: wire::Input, size: usize) {
        let Some(c) = self.conns.get_mut(&cid) else { return };
        // Only the owner's input waits for the child. A viewer's is acked and
        // dropped at once, and never queues behind the owner's.
        let owner = !c.client.is_empty() && c.client == self.core.owner && !self.core.exited;
        if owner && (!c.parked.is_empty() || self.core.input.borrow().full()) {
            park(c, input, size);
            return;
        }
        self.apply_and_ack(cid, input);
    }

    /// Counts seq as handled without applying it.
    fn ack(&mut self, cid: u64, seq: u64) {
        let Some((core, c)) = self.split(cid) else { return };
        let v = core.input_seq.entry(c.client.clone()).or_insert(0);
        *v = (*v).max(seq);
        if let Some(st) = &c.stream {
            c.q.borrow_mut().control(wire::input_ack(st.id, *v));
        }
    }

    fn apply_and_ack(&mut self, cid: u64, input: wire::Input) {
        let Some(c) = self.conns.get(&cid) else { return };
        let client = c.client.clone();
        let applied = self.core.input_seq.get(&client).copied().unwrap_or(0);
        if input.seq > applied {
            self.core.input_seq.insert(client.clone(), input.seq);
            if !client.is_empty() && client == self.core.owner && !self.core.exited {
                self.apply_input(&client, &input);
            }
        }
        let seq = self.core.input_seq.get(&client).copied().unwrap_or(0);
        if let Some(c) = self.conns.get(&cid) {
            if let Some(st) = &c.stream {
                c.q.borrow_mut().control(wire::input_ack(st.id, seq));
            }
        }
    }

    /// Encodes input for the terminal's current modes. An event that fails to
    /// encode is dropped but still counts as applied, so a bad event can't
    /// wedge the client's resend queue.
    fn apply_input(&mut self, client: &str, input: &wire::Input) {
        let term = &mut self.core.term;
        let b = match input.ty.as_str() {
            "text" => Ok(input.text.as_bytes().to_vec()),
            "paste" => term.paste(&input.text),
            "focus" => term.encode_focus(input.focused),
            "key" => {
                let Ok(mods) = ghostty::parse_mods(&input.mods) else { return };
                let action = match input.action.as_str() {
                    "repeat" => ghostty::KeyAction::Repeat,
                    "release" => ghostty::KeyAction::Release,
                    _ => ghostty::KeyAction::Press,
                };
                term.encode_key(&ghostty::Key { code: input.key.clone(), mods, text: input.text.clone(), action })
            }
            _ => Ok(Vec::new()),
        };
        let Ok(b) = b else { return };
        self.core.input.borrow_mut().push_input(&b, client, input.seq);
        self.core.input_wake.notify_one();
        if !b.is_empty() && (self.catching_up() || self.backlogged()) {
            self.core.echo_wait = true;
            self.core.last_input = Some((self.core.clock)());
        }
    }
}

/// Whether c's stream, streaming records, has more output queued unsent
/// than a screen of its terminal takes: an echo queued behind that output
/// reaches the client later than one in a screen cut now. The last screen's
/// size stands for the next one's; before any, a window (what the link
/// carries in about a round trip) does.
fn echo_behind_backlog(c: &Conn) -> bool {
    let Some(st) = &c.stream else { return false };
    if st.screens_only() || st.need_screen {
        return false;
    }
    let q = c.q.borrow();
    let screen = if st.screen_bytes > 0 { st.screen_bytes } else { q.window };
    q.lag_bytes() > screen
}

struct AttachPlan {
    st: Stream,
    recs: Option<Vec<Record>>,
}

/// Holds the owner's input while the child is behind on stdin, so acks stop
/// and a client that bounds its unacked input stops sending. An event that
/// would take parked input past MAX_PARKED_INPUT is rejected
/// deterministically: acked in order and not applied, so even a client that
/// doesn't bound itself never resends it in a loop.
fn park(c: &mut Conn, input: wire::Input, size: usize) {
    let p = if c.parked_bytes + size > MAX_PARKED_INPUT {
        Parked { input: wire::Input { seq: input.seq, ..Default::default() }, size: 0, drop: true }
    } else {
        Parked { input, size, drop: false }
    };
    c.parked_bytes += p.size;
    c.parked.push_back(p);
}

fn input_mode_event(m: InputMode) -> wire::EventMsg {
    wire::EventMsg { kind: wire::EVENT_INPUT_MODE.into(), echo: Some(m.echo), canonical: Some(m.canonical), ..Default::default() }
}

fn status_event(st: &ghostty::Status) -> wire::EventMsg {
    wire::EventMsg {
        kind: wire::EVENT_STATUS.into(),
        state: st.state.clone(),
        id: st.id.clone(),
        app: st.app.clone(),
        msg: st.message.clone(),
        progress: (st.progress >= 0).then_some(st.progress as i64),
        ..Default::default()
    }
}

impl Core {
    /// Runs f on the session after d, unless the session has stopped.
    pub fn after(&self, d: Duration, f: impl FnOnce(&mut Session) + 'static) {
        let me = self.me.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(d).await;
            if let Some(s) = me.upgrade() {
                let mut s = s.borrow_mut();
                if !s.core.done {
                    f(&mut s);
                    s.finish();
                }
            }
        });
    }

    fn log_event(&mut self, ev: &wire::EventMsg) -> Record {
        let owner_only = ev.kind == wire::EVENT_CLIPBOARD || ev.kind == wire::EVENT_CLIPBOARD_READ;
        self.log.append(REC_EVENT, &wire::marshal_event(ev), owner_only, None)
    }

    /// Feeds b to the terminal in pieces split at prompt marks, so a status
    /// report and a prompt in one read keep their order.
    fn write_term(&mut self, mut b: &[u8]) {
        loop {
            let Some(i) = self.prompts.next(b) else {
                self.term.write(b);
                self.drain_raised();
                return;
            };
            self.term.write(&b[..i]);
            self.drain_raised();
            self.end_active_status();
            b = &b[i..];
        }
    }

    fn drain_raised(&mut self) {
        let raised = std::mem::take(&mut *self.fx.borrow_mut());
        for r in raised {
            match r {
                Raised::Title(t) => {
                    if t != self.title {
                        self.title = t.clone();
                        self.pending.push(wire::EventMsg { kind: wire::EVENT_TITLE.into(), title: t, ..Default::default() });
                    }
                }
                Raised::Status(st) => {
                    self.note_status(&st);
                    self.pending.push(status_event(&st));
                }
                Raised::Event(ev) => self.pending.push(ev),
                Raised::ClipboardRead(r) => {
                    // Programs wait for each answer; a flood of reads keeps
                    // only so many waiting.
                    if self.clipboard_reads.len() < CLIPBOARD_READS {
                        let selection = (r.selection as char).to_string();
                        self.clipboard_reads.push_back(r);
                        self.pending.push(wire::EventMsg { kind: wire::EVENT_CLIPBOARD_READ.into(), selection, ..Default::default() });
                    }
                }
            }
        }
    }

    /// Keeps the latest root OSC 7501 record for session.info. A record with
    /// an id is about one task of the program, not the program; `clear`
    /// without an id removes every record, the root included.
    pub fn note_status(&mut self, st: &ghostty::Status) {
        if !st.id.is_empty() {
            return;
        }
        if st.state == "clear" {
            self.status = None;
            return;
        }
        self.status = Some(wire::ProgramStatus {
            state: st.state.clone(),
            kind: st.kind.clone(),
            app: st.app.clone(),
            title: st.title.clone(),
            msg: st.message.clone(),
            progress: (st.progress >= 0).then_some(st.progress as i64),
        });
    }

    /// Drops a root status that claims the program is still busy, once it
    /// can't be: the child exited, or the shell started a new prompt. A
    /// finished or idle report stays.
    pub fn end_active_status(&mut self) {
        if self.status.as_ref().is_some_and(|s| s.state == "working" || s.state == "blocked") {
            self.status = None;
        }
    }

    fn end_reason(&self) -> &'static str {
        if self.killed {
            wire::END_KILLED
        } else {
            wire::END_EXITED
        }
    }

    fn send_record(&mut self, c: &mut Conn, r: &Record) {
        let owner_ok = !self.owner.is_empty() && c.client == self.owner;
        let st = c.st();
        if r.start < st.from || r.only.is_some_and(|u| u != st.uid) || (r.owner_only && !owner_ok) {
            return;
        }
        if st.screens_only() {
            // Screens only while behind: one screen in flight at most, so the
            // next one (and an echo in it) never waits behind queued output.
            st.skipped += r.cost() as i64;
            return;
        }
        if st.need_screen {
            return;
        }
        let id = st.id;
        let lagging = {
            let mut q = c.q.borrow_mut();
            q.record(id, r.clone());
            q.lagging(self.cfg.lag_bytes as i64, self.cfg.catch_up_delay)
        };
        if lagging {
            self.behind(c, "behind");
        }
    }

    /// Puts a stream that can't keep up into catch-up: its unsent records
    /// go, and it gets the current screen, without history, as often as the
    /// link takes one.
    fn behind(&mut self, c: &mut Conn, reason: &str) {
        let st = c.stream.as_mut().expect("stream");
        if let Some(tr) = &self.tr {
            if !st.catch_up {
                tr.emit("catchup", &[("stream", json!(st.id)), ("on", json!(true))]);
            }
        }
        st.catch_up = true;
        st.streaming = false;
        st.catch_token += 1;
        {
            let mut q = c.q.borrow_mut();
            if st.screen_pending && (!q.screen_unstarted() || (st.abandoner && !q.begin_queued())) {
                // Let the screen in progress land, so the client always
                // finishes one; the next follows when it is credited. One
                // that abandoned another counts as in progress from its
                // SNAPSHOT_BEGIN.
                st.need_screen = true;
                q.drop_records();
                return;
            }
            // Nothing of a queued screen has gone out yet: only the newest
            // matters.
            q.drop_data();
        }
        self.send_snapshot(c, false, reason);
    }

    /// Runs when generation gen's SNAPSHOT_END has been written and
    /// everything before it credited. Only the newest snapshot counts: one an
    /// input screen preempted can complete late.
    fn snapshot_done(&mut self, c: &mut Conn, gen: u32) {
        let Some(st) = c.stream.as_mut() else { return };
        if !st.screen_pending || gen + 1 != st.snaps {
            return;
        }
        st.screen_pending = false;
        if st.need_screen {
            // The follow-up a keystroke waited for is an input screen too, so
            // the next keystroke lets it finish.
            let input = st.need_input;
            st.need_screen = false;
            st.need_input = false;
            c.q.borrow_mut().drop_data();
            self.send_snapshot(c, false, if input { "input" } else { "lag" });
            if let Some(st) = c.stream.as_mut() {
                st.input_pending = input;
            }
            return;
        }
        if st.screens_only() {
            if st.skipped == 0 {
                // Nothing happened since the screen: stream from here.
                st.streaming = true;
                c.q.borrow_mut().set_floor(0);
            } else if st.skipped <= st.screen_bytes / 2 {
                // The output skipped while that screen went out is well under
                // what the link just carried: streaming it is cheaper than
                // another screen, so stream again, from a fresh screen.
                c.q.borrow_mut().drop_data();
                self.send_snapshot(c, false, "lag");
                if let Some(st) = c.stream.as_mut() {
                    st.streaming = true;
                }
                c.q.borrow_mut().set_floor(0);
                return;
            } else {
                let typing =
                    self.last_input.is_some_and(|t| (self.clock)().saturating_duration_since(t) < self.cfg.tune.typing_window);
                if typing {
                    // The owner is typing: keep the link clear a moment for
                    // the next keystroke's screen rather than start a flood
                    // screen it would queue behind.
                    self.hold_screen(c);
                    return;
                }
                c.q.borrow_mut().drop_data();
                self.send_snapshot(c, false, "lag");
                return;
            }
        }
        self.await_caught_up(c);
    }

    /// Finds the connection a timer is about, if it still has that stream.
    fn timer_target(s: &mut Session, cid: u64, uid: u64) -> Option<(&mut Core, &mut Conn)> {
        s.split(cid).filter(|(_, c)| c.stream.as_ref().is_some_and(|st| st.uid == uid))
    }

    /// Sends the next flood screen typing_hold from now, unless a keystroke's
    /// screen goes first.
    fn hold_screen(&mut self, c: &mut Conn) {
        let cid = c.cid;
        let st = c.st();
        let (uid, token) = (st.uid, st.hold_token);
        self.after(self.cfg.tune.typing_hold, move |s| {
            let Some((core, c)) = Core::timer_target(s, cid, uid) else { return };
            let st = c.st();
            if st.hold_token != token || !st.screens_only() || st.screen_pending {
                return;
            }
            c.q.borrow_mut().drop_data();
            core.send_snapshot(c, false, "lag");
        });
    }

    /// Sends the full snapshot that restores scrollback once a catching-up
    /// stream has kept up for the catch-up delay: no record older than a
    /// round trip when it checks, and no fall-behind in between (which bumps
    /// the token). Steady output the link carries doesn't hold it off.
    fn await_caught_up(&mut self, c: &mut Conn) {
        let cid = c.cid;
        let st = c.st();
        st.catch_token += 1;
        let (uid, token) = (st.uid, st.catch_token);
        self.after(self.cfg.catch_up_delay, move |s| {
            let Some((core, c)) = Core::timer_target(s, cid, uid) else { return };
            let st = c.st();
            if st.catch_token != token || !st.catch_up || st.screen_pending {
                return;
            }
            let (lag, delay) = (core.cfg.lag_bytes as i64, core.cfg.catch_up_delay);
            let (keeping_up, lagging) = {
                let q = c.q.borrow();
                (q.keeping_up(delay), q.lagging(lag, delay))
            };
            if !keeping_up {
                if lagging {
                    core.behind(c, "behind");
                } else {
                    core.await_caught_up(c);
                }
                return;
            }
            let st = c.st();
            st.catch_up = false;
            if let Some(tr) = &core.tr {
                tr.emit("catchup", &[("stream", json!(st.id)), ("on", json!(false))]);
            }
            c.q.borrow_mut().drop_data();
            core.send_snapshot(c, true, "full");
        });
    }

    /// Cuts a snapshot at the log's end and queues it as a new generation:
    /// with history, or only the screen (through READY) for catch-up.
    /// Records from the end on follow through broadcast.
    pub fn send_snapshot(&mut self, c: &mut Conn, history: bool, reason: &str) {
        // A screen snapshot stops the encoder at READY, so its cost doesn't
        // grow with scrollback.
        let cut = Instant::now();
        let snap = if history {
            if self.cfg.tune.fail_encode {
                Err(())
            } else {
                self.term.snapshot().map_err(|_| ())
            }
        } else {
            self.term.snapshot_screen_compact().map_err(|_| ())
        };
        let Ok(snap) = snap else {
            self.end_stream(c, wire::END_ERROR);
            self.need_lapse = true;
            return;
        };
        let snap: Rc<[u8]> = Rc::from(snap);
        let ready = ghostty::ready_end(&snap);
        let (cols, rows) = self.term.size();
        let position = self.log.end;
        let input_seq = self.input_seq.get(&c.client).copied().unwrap_or(0);
        let st = c.stream.as_mut().expect("stream");
        let gen = st.snaps;
        st.snaps += 1;
        st.from = position;
        let mut begin = wire::SnapshotBegin {
            generation: gen,
            epoch: self.epoch.clone(),
            position,
            cols: cols as i64,
            rows: rows as i64,
            history: None,
        };
        st.input_pending = false;
        st.abandoner = false;
        st.hold_token += 1;
        let mut q = c.q.borrow_mut();
        if !history {
            begin.history = Some(false);
            st.screen_pending = true;
            st.skipped = 0;
            st.screen_bytes = ready as i64;
            if st.screens_only() {
                q.set_floor(2 * ready as i64);
            }
        }
        if let Some(tr) = &self.tr {
            tr.emit(
                "screen_cut",
                &[
                    ("stream", json!(st.id)),
                    ("generation", json!(gen)),
                    ("position", json!(position)),
                    ("reason", reason_name(reason)),
                    ("after_seq", json!(input_seq)),
                    ("encode_ns", json!(cut.elapsed().as_nanos() as u64)),
                    ("bytes", json!(snap.len())),
                ],
            );
        }
        let id = st.id;
        q.snapshot_marker(wire::json(wire::TYPE_SNAPSHOT_BEGIN, id, &begin), gen);
        // The first snapshot's bytes through READY are bounded by the screen
        // size and go out uncounted, so the first screen never waits on
        // credit round trips. Later ones are counted: catch-up screens are
        // paced by credit.
        let mut off = 0;
        while off < ready {
            let end = (off + CHUNK_SIZE).min(ready);
            if gen == 0 {
                q.marker_bytes(wire::TYPE_SNAPSHOT_DATA, id, &snap, off, end);
            } else {
                q.chunk(wire::TYPE_SNAPSHOT_DATA, id, &snap, off, end);
            }
            off = end;
        }
        q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_READY, id, Vec::new()), gen);
        if history {
            while off < snap.len() {
                let end = (off + CHUNK_SIZE).min(snap.len());
                q.chunk(wire::TYPE_SNAPSHOT_HISTORY, id, &snap, off, end);
                off = end;
            }
        }
        q.snapshot_marker(wire::Frame::new(wire::TYPE_SNAPSHOT_END, id, Vec::new()), gen);
        drop(q);
        if reason != "attach" {
            self.restore_state(c);
        }
    }

    /// Follows a catch-up snapshot (a screen, or the full one that ends
    /// catch-up) with the session state a snapshot doesn't carry: the owner
    /// and the input mode. Their events among the output catch-up skipped are
    /// lost to this stream, and a client must not keep a stale owner, or
    /// predict echo at a password prompt. An attach needs none: its response
    /// has the owner, and the input mode follows it. The records are this
    /// stream's alone, logged at the screen's position, so they sit exactly
    /// where the screen left the stream and a resume from them misses
    /// nothing.
    fn restore_state(&mut self, c: &mut Conn) {
        let st = c.stream.as_ref().expect("stream");
        let (uid, id) = (st.uid, st.id);
        let mut evs = vec![wire::EventMsg { kind: wire::EVENT_OWNER.into(), owner: self.owner.clone(), ..Default::default() }];
        if self.mode_known {
            evs.push(input_mode_event(self.mode));
        }
        for ev in evs {
            let r = self.log.append(REC_EVENT, &wire::marshal_event(&ev), false, Some(uid));
            c.q.borrow_mut().record(id, r);
        }
    }

    fn end_stream(&mut self, c: &mut Conn, reason: &str) {
        self.quiet_since = (self.clock)();
        let Some(st) = c.stream.take() else { return };
        let end = wire::StreamEnd { reason: reason.into(), exit_code: self.exited.then_some(self.exit_code) };
        let f = wire::json(wire::TYPE_STREAM_END, st.id, &end);
        let mut q = c.q.borrow_mut();
        if reason == wire::END_EXITED || reason == wire::END_KILLED {
            // The final output comes first, so STREAM_END keeps its place.
            q.end(f);
        } else {
            // Nobody wants the rest of a detached or failed stream, and
            // STREAM_END must go ahead of the RESPONSE that follows it
            // (detach, or an attach replacing this stream).
            q.drop_data();
            q.control(f);
        }
        // Parked input belongs to the stream; the client resends it unacked.
        c.parked.clear();
        c.parked_bytes = 0;
    }

    /// Hangs up the child's process group, then kills it if it's still there
    /// after the kill grace.
    pub fn terminate(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        self.signal_group(libc::SIGHUP);
        self.after(self.cfg.tune.kill_grace, |s| s.core.signal_group(libc::SIGKILL));
    }

    /// Signals the child's process group while the child is still unreaped.
    /// Once it is reaped (up to the drain wait before the exit is handled)
    /// its pid, and so its group id, may belong to someone else.
    pub fn signal_group(&self, sig: libc::c_int) {
        if !self.reaped.get() && self.pid > 0 {
            kill_group(self.pid, sig);
        }
    }
}

#[cfg(not(test))]
fn kill_group(pgid: i32, sig: libc::c_int) {
    unsafe { libc::kill(-pgid, sig) };
}

#[cfg(test)]
pub(crate) type KillGroup = Box<dyn Fn(i32, libc::c_int)>;

#[cfg(test)]
thread_local! {
    /// Replaces kill_group on this thread, for tests.
    pub(crate) static KILL_GROUP: RefCell<Option<KillGroup>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn kill_group(pgid: i32, sig: libc::c_int) {
    if KILL_GROUP.with(|k| k.borrow().as_ref().map(|f| f(pgid, sig)).is_none()) {
        unsafe { libc::kill(-pgid, sig) };
    }
}

/// OSC 133;A, a shell's "prompt starts" mark.
const PROMPT_MARK: &[u8] = b"\x1b]133;A";

/// The most OSC 52 reads waiting for an answer.
const CLIPBOARD_READS: usize = 16;

/// Finds prompt marks in PTY output, across reads.
#[derive(Default)]
pub(crate) struct PromptScanner {
    matched: usize,
}

impl PromptScanner {
    /// The offset just past the first prompt mark in b.
    pub fn next(&mut self, b: &[u8]) -> Option<usize> {
        for (i, &c) in b.iter().enumerate() {
            if c == PROMPT_MARK[self.matched] {
                self.matched += 1;
            } else if c == PROMPT_MARK[0] {
                self.matched = 1;
            } else {
                self.matched = 0;
            }
            if self.matched == PROMPT_MARK.len() {
                self.matched = 0;
                return Some(i + 1);
            }
        }
        None
    }
}
