//! The mirror terminal: the session's screen kept client side.

use std::sync::{Arc, Mutex, MutexGuard};

use osewa_client::{Renderer, RendererError};
use osewa_ghostty::{self as ghostty, Callbacks, Decoder, Terminal};
use osewa_wire as wire;
use tokio::sync::watch;

use crate::SendCell;

type Cb<T> = Option<Box<T>>;
type BytesFn = dyn FnMut(&[u8]) + Send;
type EventFn = dyn FnMut(&wire::EventMsg, bool) + Send;
type AppliedFn = dyn FnMut(Option<&mut Terminal>) + Send;

/// A Mirror's callbacks. They run with the mirror locked, in stream order:
/// they must not call `view`, `text` or `wait_text`.
#[derive(Default)]
pub struct Options {
    /// Runs when a snapshot becomes renderable, with the new mirror.
    pub on_ready: Cb<dyn FnMut(&mut Terminal) + Send>,
    /// Runs with each OUTPUT's bytes after the mirror applied them.
    pub on_output: Cb<BytesFn>,
    /// Runs with every applied event; live is false for events from a
    /// snapshot or resume catch-up, whose effects must not fire.
    pub on_event: Cb<EventFn>,
    /// Runs after each frame that changed the mirror, after the callbacks
    /// above.
    pub on_applied: Cb<AppliedFn>,
}

struct State {
    opt: Options,
    /// None before the first READY.
    term: Option<SendCell<Terminal>>,
    /// The snapshot in progress.
    dec: Option<SendCell<Decoder>>,
    /// The snapshot in progress brings history.
    history: bool,
    ended: bool,
}

struct Inner {
    st: Mutex<State>,
    changed: watch::Sender<u64>,
}

/// A session's terminal on this side: no replies, no effects. Clones share
/// it; pass one as `AttachOptions::renderer` and keep another to read it.
#[derive(Clone)]
pub struct Mirror {
    inner: Arc<Inner>,
}

impl Mirror {
    /// An empty mirror.
    pub fn new(o: Options) -> Mirror {
        Mirror {
            inner: Arc::new(Inner {
                st: Mutex::new(State {
                    opt: o,
                    term: None,
                    dec: None,
                    history: false,
                    ended: false,
                }),
                changed: watch::Sender::new(0),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Called with the state locked, so a waiter that subscribed under the
    // lock never misses a change.
    fn notify(&self) {
        self.inner.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn applied(&self, st: &mut State) {
        if let Some(f) = st.opt.on_applied.as_mut() {
            f(st.term.as_mut().map(|t| &mut t.0));
        }
        self.notify();
    }

    /// Starts decoding a snapshot. One begun before the previous one's
    /// READY abandons that one; the current terminal stays until the new
    /// READY.
    pub fn snapshot_begin(&self, b: &wire::SnapshotBegin) -> Result<(), ghostty::Error> {
        let mut st = self.lock();
        st.dec = None;
        let dec = Decoder::new()?;
        st.dec = Some(SendCell(dec));
        st.history = b.history.unwrap_or(true);
        Ok(())
    }

    /// Feeds snapshot bytes, through READY or after it.
    pub fn snapshot_data(&self, data: &[u8], _history: bool) -> Result<(), ghostty::Error> {
        // The transport checks frame order; no decoder means close came
        // first.
        if let Some(d) = self.lock().dec.as_mut() {
            d.0.feed(data);
        }
        Ok(())
    }

    /// Swaps in the decoded terminal.
    pub fn snapshot_ready(&self) -> Result<(), ghostty::Error> {
        let mut st = self.lock();
        let Some(dec) = st.dec.as_mut() else {
            return Ok(());
        };
        let t = dec.0.ready(Callbacks::default())?;
        st.term = Some(SendCell(t));
        let st = &mut *st;
        if let (Some(f), Some(t)) = (st.opt.on_ready.as_mut(), st.term.as_mut()) {
            f(&mut t.0);
        }
        self.applied(st);
        Ok(())
    }

    /// Decodes the history into the terminal.
    pub fn snapshot_end(&self) -> Result<(), ghostty::Error> {
        let mut st = self.lock();
        let mut res = Ok(());
        // A screen snapshot ends at READY: asking the decoder for history
        // would hit end of input and fail.
        if st.history {
            if let Some(d) = st.dec.as_mut() {
                res = d.0.finish();
            }
        }
        st.dec = None;
        self.notify();
        res
    }

    /// Applies output bytes.
    pub fn output(&self, data: &[u8]) -> Result<(), ghostty::Error> {
        let mut st = self.lock();
        if let Some(t) = st.term.as_mut() {
            t.0.write(data);
        }
        if let Some(f) = st.opt.on_output.as_mut() {
            f(data);
        }
        self.applied(&mut st);
        Ok(())
    }

    /// Applies an event's effect on the terminal.
    pub fn event(&self, e: &wire::EventMsg, live: bool) -> Result<(), ghostty::Error> {
        let mut st = self.lock();
        // The holder's terminal resized at exactly this log position; the
        // mirror must follow here or the bytes after it render differently.
        if e.kind == wire::EVENT_RESIZE && e.cols > 0 && e.rows > 0 {
            if let Some(t) = st.term.as_mut() {
                let _ = t.0.resize(e.cols as usize, e.rows as usize);
            }
        }
        if let Some(f) = st.opt.on_event.as_mut() {
            f(e, live);
        }
        self.applied(&mut st);
        Ok(())
    }

    /// The session ending: the terminal stays readable until close.
    pub fn end(&self) {
        let mut st = self.lock();
        st.dec = None;
        st.ended = true;
        self.notify();
    }

    /// Frees the terminal.
    pub fn close(&self) {
        let mut st = self.lock();
        st.dec = None;
        st.term = None;
        if !st.ended {
            st.ended = true;
            self.notify();
        }
    }

    /// Runs f with the terminal, or None before the first snapshot. The
    /// mirror is locked meanwhile.
    pub fn view<R>(&self, f: impl FnOnce(Option<&mut Terminal>) -> R) -> R {
        let mut st = self.lock();
        f(st.term.as_mut().map(|t| &mut t.0))
    }

    /// The terminal as trimmed plain text, scrollback included.
    pub fn text(&self) -> String {
        self.lock()
            .term
            .as_ref()
            .map(|t| t.0.text())
            .unwrap_or_default()
    }

    /// Only the active area as plain text: one line per row, trailing
    /// blanks and trailing empty rows trimmed.
    pub fn visible_text(&self) -> String {
        visible_text(self.lock().term.as_ref().map(|t| &t.0))
    }

    /// Waits until the active area (not the scrollback) contains substr.
    /// An empty substr waits for the first snapshot. Timeouts are the
    /// caller's.
    pub async fn wait_text(&self, substr: &str) -> Result<(), String> {
        loop {
            let mut rx = {
                let st = self.lock();
                let rx = self.inner.changed.subscribe();
                if st.term.is_some()
                    && visible_text(st.term.as_ref().map(|t| &t.0)).contains(substr)
                {
                    return Ok(());
                }
                if st.ended {
                    return Err(format!(
                        "osewa: session ended without {}",
                        wire::quote(substr)
                    ));
                }
                rx
            };
            let _ = rx.changed().await;
        }
    }
}

impl Renderer for Mirror {
    fn snapshot_begin(&mut self, b: &wire::SnapshotBegin) -> Result<(), RendererError> {
        Ok(Mirror::snapshot_begin(self, b)?)
    }
    fn snapshot_data(&mut self, data: &[u8], history: bool) -> Result<(), RendererError> {
        Ok(Mirror::snapshot_data(self, data, history)?)
    }
    fn snapshot_ready(&mut self) -> Result<(), RendererError> {
        Ok(Mirror::snapshot_ready(self)?)
    }
    fn snapshot_end(&mut self) -> Result<(), RendererError> {
        Ok(Mirror::snapshot_end(self)?)
    }
    fn output(&mut self, data: &[u8]) -> Result<(), RendererError> {
        Ok(Mirror::output(self, data)?)
    }
    fn event(&mut self, e: &wire::EventMsg, live: bool) -> Result<(), RendererError> {
        Ok(Mirror::event(self, e, live)?)
    }
    fn end(&mut self) {
        Mirror::end(self)
    }
}

pub(crate) fn visible_text(t: Option<&Terminal>) -> String {
    let Some(t) = t else { return String::new() };
    let Ok(b) = t.active_text() else {
        return String::new();
    };
    let s = String::from_utf8_lossy(&b);
    let lines: Vec<&str> = s.split('\n').collect();
    let (_, rows) = t.size();
    let lines = &lines[..rows.min(lines.len())];
    let mut out: Vec<&str> = lines.iter().map(|l| l.trim_end_matches(' ')).collect();
    while out.last() == Some(&"") {
        out.pop();
    }
    out.join("\n")
}

/// Spells a ghostty key event in protocol names, for `Session::key`.
pub fn key(k: &ghostty::Key) -> (String, Vec<String>, String, String) {
    let action = match k.action {
        ghostty::KeyAction::Repeat => "repeat",
        ghostty::KeyAction::Release => "release",
        ghostty::KeyAction::Press => "",
    };
    let mut mods = Vec::new();
    for name in ["shift", "ctrl", "alt", "super"] {
        let bit = ghostty::parse_mods(&[name]).map(|m| m.0).unwrap_or(0);
        if k.mods.0 & bit != 0 {
            mods.push(name.to_string());
        }
    }
    (k.code.clone(), mods, k.text.clone(), action.to_string())
}

#[cfg(test)]
mod tests;
