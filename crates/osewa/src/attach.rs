//! `osewa attach`: a session on the local
//! terminal through a Link, which redials and resumes it whenever the
//! connection dies or goes silent.

use std::io::{Read, Write};
use std::time::Duration;

use osewa_client::{AttachOptions, Link, Session};
use osewa_mirror::{parse_predict_mode, Predictor, PredictorConfig};

use crate::cli::{bounded, interrupted, Ctx, Env, Error, Flags, Kind, Result};
use crate::commands::pick_session;
use crate::tty;
use crate::ui::InputHandler;
pub(crate) use osewa_vt::{split_utf8, ticker, Report};
use osewa_vt::{viewer_notice, Attached, Ui};

/// Undoes modes the remote program may have left on our terminal (alt
/// screen, hidden cursor, mouse and focus reporting, bracketed paste, kitty
/// keyboard), since the local terminal outlives the attach.
pub(crate) const RESET_MODES: &str = "\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[<u\x1b[>4m\x1b[?2031l\x1b]22;\x1b\\\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\\r\n";

/// Asks the local terminal for its appearance, for the programs that ask
/// the holder: dark or light, now (DSR 996) and whenever that changes
/// (mode 2031), and its colors. Then for its device attributes (DA1), which
/// every terminal answers, in order: its reply says the others are in
/// (Report::Answered).
pub(crate) fn ask_appearance() -> String {
    let palette: Vec<String> = (0..256).map(|i| format!("{i};?")).collect();
    format!("\x1b[?2031h\x1b[?996n\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]12;?\x1b\\\x1b]4;{}\x1b\\\x1b[c", palette.join(";"))
}

/// How long leaving waits for the local terminal's replies.
const REPLIES_WAIT: Duration = Duration::from_secs(2);

/// Runs `attach [--new] [--take] [--quiet] [--predict auto|always|never]
/// [--graphics auto|always|never] [--clipboard-read] <host> [id]` and
/// returns the exit code.
pub(crate) async fn attach(ctx: &Ctx, env: &Env, args: &[String]) -> Result<i32> {
    let mut fs = Flags::new(&[("new", Kind::Bool), ("take", Kind::Bool), ("quiet", Kind::Bool), ("predict", Kind::Str), ("graphics", Kind::Str), ("clipboard-read", Kind::Bool)]);
    let pos = fs.parse(args)?;
    let predict = if fs.all("predict").is_empty() { "auto".to_string() } else { fs.str("predict") };
    let mode = parse_predict_mode(&predict).map_err(Error::Msg)?;
    let graphics = match fs.all("graphics").first().map_or("auto", |s| s.as_str()) {
        "auto" => shows_graphics(|k| std::env::var(k).ok()),
        "always" => true,
        "never" => false,
        g => return Err(Error::Msg(format!("osewa: graphics mode {}: want auto, always or never", osewa_wire::quote(g)))),
    };
    // --new starts a session, so it names none.
    if pos.is_empty() || pos.len() > 2 || (fs.bool("new") && pos.len() > 1) {
        return Err(Error::Usage);
    }
    let h = osewa_client::hosts::find(&env.dir, &pos[0])?;
    let cfg = env.config(&h)?;
    let l = Link::new(cfg);
    let r = attach_on(ctx, env, &l, &pos, fs.bool("new"), fs.bool("take"), fs.bool("quiet"), mode, graphics, fs.bool("clipboard-read")).await;
    l.close().await;
    r
}

#[allow(clippy::too_many_arguments)]
async fn attach_on(
    ctx: &Ctx,
    env: &Env,
    l: &Link,
    pos: &[String],
    new: bool,
    take: bool,
    quiet: bool,
    mode: osewa_mirror::PredictMode,
    graphics: bool,
    clipboard_read: bool,
) -> Result<i32> {
    let c = bounded(ctx, Duration::from_secs(15), async { Ok(l.conn().await?) }).await?;
    let (cols, rows) = tty::term_size(1);
    let id = match pos.get(1) {
        Some(id) => id.clone(),
        None if new => {
            let (width, height) = tty::term_pixels(1);
            let create = c.create(osewa_client::CreateOptions { cols, rows, width, height, ..Default::default() });
            match ctx.run(create).await {
                None => return Err(interrupted()),
                Some(r) => r?,
            }
        }
        None => match ctx.run(pick_session(&c, cols, rows, tty::term_pixels(1))).await {
            None => return Err(interrupted()),
            Some(r) => r?,
        },
    };

    // Raw mode before the first byte reaches the terminal, so nothing is
    // cooked or echoed. Undone last: after the session has stopped output,
    // so no late remote mode lands after the reset.
    let restore = make_raw(env)?;
    let me = env.client_id();
    let mut ui = Ui::new(Box::new(env.out()), quiet, Predictor::new(PredictorConfig { mode, ..Default::default() })).with_legacy_viewer().with_status_forward();
    if graphics {
        ui = ui.with_graphics();
    }
    if clipboard_read {
        ui = ui.with_clipboard_read();
    }
    let a = Attached::new(ui, me.clone());
    let asked = restore.is_some();
    if asked {
        let _ = env.out().write_all(ask_appearance().as_bytes());
    }
    let r = run_attached(ctx, env, l, &a, &id, cols, rows, take).await;
    if let Some(s) = a.s.lock().unwrap().take() {
        s.close();
    }
    a.m.close();
    a.clear_status();
    if asked {
        // Replies still on their way when we exit would land in the shell
        // (#19): an attach to an ended session is over before they come.
        if let Some(mut stdin) = env.take_stdin() {
            let h = a.clone();
            std::thread::spawn(move || drain_reports(&mut stdin, &h, Reports::default()));
        }
        let _ = tokio::time::timeout(REPLIES_WAIT, a.answered.notified()).await;
    }
    if let Some(restore) = restore {
        let _ = env.out().write_all(RESET_MODES.as_bytes());
        restore();
    }
    r
}

/// Whether the local terminal draws Kitty graphics, from what it says about
/// itself in the environment (getenv). Asking it would need its reply off
/// stdin before attach starts reading.
pub(crate) fn shows_graphics(getenv: impl Fn(&str) -> Option<String>) -> bool {
    let term = getenv("TERM").unwrap_or_default();
    let program = getenv("TERM_PROGRAM").unwrap_or_default();
    term.contains("kitty") || term.contains("ghostty") || getenv("KITTY_WINDOW_ID").is_some() || matches!(program.as_str(), "ghostty" | "WezTerm")
}

/// Puts stdin in raw mode when it is a terminal. None means there was
/// nothing to do.
fn make_raw(env: &Env) -> Result<Option<Box<dyn FnOnce() + Send>>> {
    match &env.raw {
        Some(f) => Ok(f()?),
        None => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_attached(ctx: &Ctx, env: &Env, l: &Link, a: &Attached, id: &str, cols: i64, rows: i64, take: bool) -> Result<i32> {
    let ar = a.clone();
    let (width, height) = tty::term_pixels(1);
    let opt = AttachOptions {
        cols,
        rows,
        width,
        height,
        renderer: Some(Box::new(a.m.clone())),
        on_rtt: Some(Box::new(move |d| ar.rtt(d))),
        ..Default::default()
    };
    let s = match ctx.run(l.attach(id, opt)).await {
        None => return Err(interrupted()),
        Some(r) => r?,
    };
    *a.s.lock().unwrap() = Some(s.clone());

    // Someone else's session is only taken when asked; otherwise we watch.
    // An ended one (#19) has nobody to own it: it shows its last screen.
    let owner = s.owner();
    if s.exit_code().is_some() {
    } else if owner.is_empty() || owner == a.id || take {
        match ctx.run(s.own()).await {
            None => return Ok(1), // interrupted, as in the loop below
            Some(Err(_)) if s.exit_code().is_some() => {}
            Some(r) => {
                r?;
                a.sync_owner();
                a.send_appearance().await;
            }
        }
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(5), a.m.wait_text("")).await;
        if s.exit_code().is_none() {
            a.notice(&viewer_notice(&owner, &a.id));
        }
    }

    let (detach_tx, detach_rx) = tokio::sync::oneshot::channel::<()>();
    if let Some(mut stdin) = env.take_stdin() {
        let (h, ictx) = (a.clone(), ctx.clone());
        std::thread::spawn(move || {
            forward_input(&ictx, &mut stdin, &h);
            let _ = detach_tx.send(());
        });
    } else {
        // No input at all: as an immediate EOF.
        let _ = detach_tx.send(());
    }
    let tasks = [tokio::spawn(ticker(a.clone(), {
        let s = s.clone();
        async move { s.done().await }
    })), tokio::spawn(resize_on_winch(s.clone()))];
    let r = tokio::select! {
        _ = s.done() => {
            // Say why we're back: the session ended for every client, maybe
            // from another window, which otherwise looks like a detach.
            match s.exit_code() {
                Some(code) => {
                    let _ = write!(env.err(), "\r\n[osewa: session ended, exit {code}]\r\n");
                    Ok(code as i32)
                }
                None => match s.err() {
                    Some(e) => Err(e.into()),
                    None => Ok(0),
                },
            }
        }
        _ = detach_rx => {
            // A new attach is a new process with a new client id: keeping
            // ownership through the grace would make it a viewer.
            let _ = tokio::time::timeout(Duration::from_secs(2), s.leave()).await;
            let _ = write!(env.err(), "\r\n[osewa: detached; the session is still running]\r\n");
            Ok(0)
        }
        _ = ctx.done() => Ok(1),
    };
    for t in tasks {
        t.abort();
    }
    r
}

/// Forwards terminal size changes. It runs off the main loop so a request
/// on a dying connection can't block detach or reconnect, and it always
/// sends the latest size.
async fn resize_on_winch(s: Session) {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut winch) = signal(SignalKind::window_change()) else { return };
    loop {
        tokio::select! {
            r = winch.recv() => if r.is_none() { return },
            _ = s.done() => return,
        }
        let ((cols, rows), (width, height)) = (tty::term_size(1), tty::term_pixels(1));
        let _ = tokio::time::timeout(Duration::from_secs(5), s.resize(cols, rows, width, height)).await;
    }
}

/// Sends stdin to the session as text until `~.` or EOF, then returns; `~t`
/// takes ownership. On EOF it first waits until the holder has everything,
/// so `printf 'make\r' | osewa attach` runs make. Nothing reads stdin after
/// `~.`: what is typed then is for the shell.
pub(crate) fn forward_input(ctx: &Ctx, r: &mut dyn Read, h: &dyn InputHandler) {
    let mut esc = Escaper { nl: true, tilde: false };
    let mut carry: Vec<u8> = Vec::new();
    let mut reports = Reports::default();
    let mut buf = [0u8; 4096];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => {
                h.flush(ctx);
                h.reports(&[Report::Answered]);
                return;
            }
            Ok(n) => n,
        };
        let (typed, got) = reports.feed(&buf[..n]);
        if !got.is_empty() {
            h.reports(&got);
        }
        let mut input = &typed[..];
        while !input.is_empty() {
            let (out, act, rest) = esc.feed(input);
            carry.extend_from_slice(&out);
            let (text, keep) = split_utf8(&carry);
            let (text, keep) = (text.to_vec(), keep.to_vec());
            carry = keep;
            if !text.is_empty() {
                h.send(&String::from_utf8_lossy(&text));
            }
            match act {
                EscAction::Detach => return,
                // Keys after ~t must wait for it, or the old owner still
                // refuses them.
                EscAction::Take => h.take(),
                EscAction::None => {}
            }
            input = rest;
        }
    }
}

/// Reads stdin for the local terminal's replies alone, until it ends, for an
/// attach that ended before it read any: what is typed has nowhere to go.
pub(crate) fn drain_reports(r: &mut dyn Read, h: &dyn InputHandler, mut reports: Reports) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => {
                h.reports(&[Report::Answered]);
                return;
            }
            Ok(n) => n,
        };
        let (_, got) = reports.feed(&buf[..n]);
        if !got.is_empty() {
            h.reports(&got);
        }
    }
}

/// Takes the local terminal's reports out of what is typed. A report cut
/// by a read is held for the next one, but only from "ESC [ ?" or "ESC ]"
/// and a digit on: a lone ESC is a key that must not wait. Except right
/// after a report: a burst of replies can be cut anywhere (a macOS pty
/// read stops near 1 KiB, and tmux's fixed-length replies put that on an
/// ESC), and a key never trails a reply in the same read.
#[derive(Default)]
pub(crate) struct Reports {
    held: Vec<u8>,
}

/// The longest color reply held for the next read.
const COLOR_REPORT_MAX: usize = 64;

/// The longest clipboard reply held for the next read.
const CLIPBOARD_REPORT_MAX: usize = 8 << 20;

impl Reports {
    /// The typed bytes, and the reports.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Vec<Report>) {
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(input);
        let (mut out, mut schemes) = (Vec::with_capacity(data.len()), Vec::new());
        let mut after_report = false;
        let mut i = 0;
        while i < data.len() {
            let rest = &data[i..];
            if rest[0] != 0x1b {
                out.push(rest[0]);
                i += 1;
                after_report = false;
                continue;
            }
            if after_report && rest.len() <= 2 {
                self.held = rest.to_vec();
                break;
            }
            after_report = false;
            if rest.get(1) == Some(&b']') {
                match color_report(rest) {
                    ColorReport::Done(len, r) => {
                        schemes.extend(r);
                        i += len;
                        after_report = true;
                    }
                    ColorReport::Partial => {
                        self.held = rest.to_vec();
                        break;
                    }
                    ColorReport::Not => {
                        out.push(rest[0]);
                        i += 1;
                    }
                }
                continue;
            }
            let n = rest.len().min(CSI_PRIVATE.len());
            if rest[..n] != CSI_PRIVATE[..n] {
                out.push(rest[0]);
                i += 1;
                continue;
            }
            if n < CSI_PRIVATE.len() {
                out.extend_from_slice(rest);
                break;
            }
            match private_report(&rest[n..]) {
                PrivateReport::Done(len, r) => {
                    schemes.extend(r);
                    i += n + len;
                    after_report = true;
                }
                PrivateReport::Partial => {
                    self.held = rest.to_vec();
                    break;
                }
                PrivateReport::Not => {
                    out.push(rest[0]);
                    i += 1;
                }
            }
        }
        (out, schemes)
    }
}

/// How every report this reads starts: CSI ?.
const CSI_PRIVATE: &[u8] = b"\x1b[?";

/// The longest parameters of a CSI ? reply held for the next read.
const PRIVATE_REPORT_MAX: usize = 32;

enum PrivateReport {
    /// A reply this long after CSI ?, and what it said if anything.
    Done(usize, Option<Report>),
    Partial,
    Not,
}

/// Reads the rest of a CSI ? reply: the color scheme (997;1|2 n, at most 3
/// digits after the 997), or primary device attributes (… c), which asks
/// nothing of us but says the terminal has answered all that came before.
fn private_report(b: &[u8]) -> PrivateReport {
    let params = b.iter().take_while(|c| c.is_ascii_digit() || **c == b';').count();
    let Some(&end) = b.get(params) else {
        return if params <= PRIVATE_REPORT_MAX { PrivateReport::Partial } else { PrivateReport::Not };
    };
    let p = &b[..params];
    match end {
        b'n' => match p.strip_prefix(b"997;") {
            Some(d) if (1..=3).contains(&d.len()) && d.iter().all(u8::is_ascii_digit) => PrivateReport::Done(
                params + 1,
                match d {
                    b"1" => Some(Report::Scheme(true)),
                    b"2" => Some(Report::Scheme(false)),
                    _ => None,
                },
            ),
            _ => PrivateReport::Not,
        },
        b'c' if !p.is_empty() && params <= PRIVATE_REPORT_MAX => PrivateReport::Done(params + 1, Some(Report::Answered)),
        _ => PrivateReport::Not,
    }
}

enum ColorReport {
    /// A reply this long, and what it said if it was well formed.
    Done(usize, Option<Report>),
    /// The start of one: wait for the rest.
    Partial,
    Not,
}

/// Reads an OSC 4, 10, 11, 12 or 52 reply at the start of b (ESC ] ...).
fn color_report(b: &[u8]) -> ColorReport {
    let digits = b[2..].iter().take_while(|c| c.is_ascii_digit()).count();
    let num = &b[2..2 + digits];
    if !matches!(num, b"4" | b"10" | b"11" | b"12" | b"52") {
        // Only a prefix of one of those can wait.
        let partial = 2 + digits == b.len() && digits > 0 && matches!(num, b"1" | b"5");
        return if partial { ColorReport::Partial } else { ColorReport::Not };
    }
    let max = if num == b"52" { CLIPBOARD_REPORT_MAX } else { COLOR_REPORT_MAX };
    let body = &b[2..];
    if body.get(digits).is_some_and(|&c| c != b';') {
        return ColorReport::Not;
    }
    let end = body.iter().position(|&c| c == 0x07 || c == 0x1b);
    let Some(end) = end else {
        return if b.len() < max { ColorReport::Partial } else { ColorReport::Not };
    };
    let term = match body[end] {
        0x07 => 1,
        _ if body.get(end + 1) == Some(&b'\\') => 2,
        _ if end + 1 == body.len() => return ColorReport::Partial,
        _ => return ColorReport::Not,
    };
    let len = 2 + end + term;
    let parts: Vec<&[u8]> = body[..end].split(|&c| c == b';').collect();
    let report = match (num, parts.as_slice()) {
        (b"4", [_, i, spec]) => std::str::from_utf8(i).ok().and_then(|i| i.parse::<u8>().ok()).zip(parse_xcolor(spec)).map(|(i, c)| Report::Palette(i, c)),
        (b"10", [_, spec]) => parse_xcolor(spec).map(Report::Foreground),
        (b"11", [_, spec]) => parse_xcolor(spec).map(Report::Background),
        (b"12", [_, spec]) => parse_xcolor(spec).map(Report::Cursor),
        (b"52", [_, _, data]) => {
            use base64::Engine as _;
            Some(Report::Clipboard(base64::engine::general_purpose::STANDARD.decode(data).unwrap_or_default()))
        }
        _ => return ColorReport::Not,
    };
    ColorReport::Done(len, report)
}

/// Parses an XParseColor rgb:r/g/b spec, 1 to 4 hex digits a part, as
/// terminals reply with.
fn parse_xcolor(spec: &[u8]) -> Option<[u8; 3]> {
    let s = std::str::from_utf8(spec).ok()?.strip_prefix("rgb:")?;
    let mut c = [0u8; 3];
    let mut parts = s.split('/');
    for v in &mut c {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 4 {
            return None;
        }
        let max = (1u32 << (4 * p.len())) - 1;
        *v = ((u32::from_str_radix(p, 16).ok()? * 255 + max / 2) / max) as u8;
    }
    parts.next().is_none().then_some(c)
}

/// ssh-style escapes right after a newline: `~.` detaches, `~t` takes
/// ownership, `~~` sends one `~`.
pub(crate) struct Escaper {
    pub nl: bool,
    pub tilde: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscAction {
    None,
    Detach,
    Take,
}

impl Escaper {
    /// The bytes to send up to the first escape, that escape, and the input
    /// after it.
    pub fn feed<'a>(&mut self, input: &'a [u8]) -> (Vec<u8>, EscAction, &'a [u8]) {
        let mut out = Vec::new();
        for (i, &b) in input.iter().enumerate() {
            if self.tilde {
                self.tilde = false;
                match b {
                    b'.' => return (out, EscAction::Detach, &input[i + 1..]),
                    b't' => return (out, EscAction::Take, &input[i + 1..]),
                    b'~' => {
                        out.push(b'~');
                        self.nl = false;
                        continue;
                    }
                    _ => out.push(b'~'),
                }
            }
            if self.nl && b == b'~' {
                self.tilde = true;
                continue;
            }
            out.push(b);
            self.nl = b == b'\r' || b == b'\n';
        }
        (out, EscAction::None, &[])
    }
}
