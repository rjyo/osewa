//! `osewa attach` (Go's cmd/osewa/attach.go): a session on the local
//! terminal through a Link, which redials and resumes it whenever the
//! connection dies or goes silent.

use std::io::{Read, Write};
use std::time::Duration;

use osewa_client::{AttachOptions, Link, Session};
use osewa_mirror::{parse_predict_mode, Predictor, PredictorConfig};

use crate::cli::{bounded, interrupted, Ctx, Env, Error, Flags, Kind, Result};
use crate::commands::pick_session;
use crate::tty;
use crate::ui::{viewer_notice, Attached, InputHandler, Ui};

/// Undoes modes the remote program may have left on our terminal (alt
/// screen, hidden cursor, mouse and focus reporting, bracketed paste, kitty
/// keyboard), since the local terminal outlives the attach.
pub(crate) const RESET_MODES: &str =
    "\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[<u\r\n";

/// Runs `attach [--take] [--quiet] [--predict auto|always|never] <host>
/// [id]` and returns the exit code.
pub(crate) async fn attach(ctx: &Ctx, env: &Env, args: &[String]) -> Result<i32> {
    let mut fs = Flags::new(&[("take", Kind::Bool), ("quiet", Kind::Bool), ("predict", Kind::Str)]);
    let pos = fs.parse(args)?;
    let predict = if fs.all("predict").is_empty() { "auto".to_string() } else { fs.str("predict") };
    let mode = parse_predict_mode(&predict).map_err(Error::Msg)?;
    if pos.is_empty() || pos.len() > 2 {
        return Err(Error::Usage);
    }
    let h = osewa_client::hosts::find(&env.dir, &pos[0])?;
    let cfg = env.config(&h)?;
    let l = Link::new(cfg);
    let r = attach_on(ctx, env, &l, &pos, fs.bool("take"), fs.bool("quiet"), mode).await;
    l.close().await;
    r
}

#[allow(clippy::too_many_arguments)]
async fn attach_on(
    ctx: &Ctx,
    env: &Env,
    l: &Link,
    pos: &[String],
    take: bool,
    quiet: bool,
    mode: osewa_mirror::PredictMode,
) -> Result<i32> {
    let c = bounded(ctx, Duration::from_secs(15), async { Ok(l.conn().await?) }).await?;
    let (cols, rows) = tty::term_size(1);
    let id = match pos.get(1) {
        Some(id) => id.clone(),
        None => match ctx.run(pick_session(&c, cols, rows)).await {
            None => return Err(interrupted()),
            Some(r) => r?,
        },
    };

    // Raw mode before the first byte reaches the terminal, so nothing is
    // cooked or echoed. Undone last: after the session has stopped output,
    // so no late remote mode lands after the reset.
    let restore = make_raw(env)?;
    let me = env.client_id();
    let a = Attached::new(Ui::new(Box::new(env.out()), quiet, Predictor::new(PredictorConfig { mode, ..Default::default() })), me.clone());
    let r = run_attached(ctx, env, l, &a, &id, cols, rows, take).await;
    if let Some(s) = a.s.lock().unwrap().take() {
        s.close();
    }
    a.m.close();
    if let Some(restore) = restore {
        let _ = env.out().write_all(RESET_MODES.as_bytes());
        restore();
    }
    r
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
    let opt = AttachOptions {
        cols,
        rows,
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
    let owner = s.owner();
    if owner.is_empty() || owner == a.id || take {
        match ctx.run(s.own()).await {
            None => return Ok(1), // interrupted, as in the loop below
            Some(r) => r?,
        }
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(5), a.m.wait_text("")).await;
        a.notice(&viewer_notice(&owner, &a.id));
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
    let tasks = [tokio::spawn(ticker(a.clone(), s.clone())), tokio::spawn(resize_on_winch(s.clone()))];
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
            let _ = tokio::time::timeout(Duration::from_secs(2), s.detach()).await;
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

/// Checks predictions for timeouts while attached.
async fn ticker(a: Attached, s: Session) {
    let mut t = tokio::time::interval(crate::ui::TICK_EVERY);
    loop {
        tokio::select! {
            _ = t.tick() => a.tick(),
            _ = s.done() => return,
        }
    }
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
        let (cols, rows) = tty::term_size(1);
        let _ = tokio::time::timeout(Duration::from_secs(5), s.resize(cols, rows)).await;
    }
}

/// Sends stdin to the session as text until `~.` or EOF, then returns; `~t`
/// takes ownership. On EOF it first waits until the holder has everything,
/// so `printf 'make\r' | osewa attach` runs make.
pub(crate) fn forward_input(ctx: &Ctx, r: &mut dyn Read, h: &dyn InputHandler) {
    let mut esc = Escaper { nl: true, tilde: false };
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => {
                h.flush(ctx);
                return;
            }
            Ok(n) => n,
        };
        let mut input = &buf[..n];
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

/// Holds back an incomplete trailing character, since a read can end in the
/// middle of one and text input travels as a JSON string.
pub(crate) fn split_utf8(b: &[u8]) -> (&[u8], &[u8]) {
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
