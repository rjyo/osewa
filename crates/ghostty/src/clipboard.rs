// Clipboard reads a holder answers late, and pastes that can become Kitty
// paste events.
//
// libghostty wants a read answered before its callback returns, and
// otherwise answers it itself: empty for OSC 52, EPERM for Kitty's
// clipboard protocol (OSC 5522). A holder must ask a client first, so the
// binding holds that answer back, raises the read, and later writes the
// real one in the same form.
use std::ffi::c_void;

use base64::Engine as _;

use crate::{ffi, state, Result, Terminal};

/// A clipboard read a program made, left for the embedder to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRead {
    /// c (the clipboard), p (primary) or s (selection), as OSC 52 names
    /// them.
    pub selection: u8,
    /// Set for a Kitty clipboard protocol read.
    pub kitty: Option<KittyRead>,
    /// The read ended with BEL, and so should the answer.
    pub bel: bool,
}

/// What a Kitty clipboard protocol read asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyRead {
    /// Echoed in the answer; empty when the program gave none.
    pub id: String,
    /// The MIME types it wants, in order.
    pub mimes: Vec<String>,
    /// It wants the list of types the clipboard holds.
    pub list: bool,
}

/// Raw bytes per DATA packet, as the protocol has it.
const KITTY_CHUNK: usize = 4096;

impl ClipboardRead {
    /// The answer to write to the program, with text as the clipboard's
    /// contents; empty text is an empty clipboard. Only text is served:
    /// under the first text type the read asked for.
    pub fn answer(&self, text: &[u8]) -> Vec<u8> {
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let st: &[u8] = if self.bel { b"\x07" } else { b"\x1b\\" };
        let Some(k) = &self.kitty else {
            let mut out = format!("\x1b]52;{};{}", self.selection as char, b64(text)).into_bytes();
            out.extend_from_slice(st);
            return out;
        };
        let id = if k.id.is_empty() { String::new() } else { format!(":id={}", k.id) };
        let mut out = Vec::new();
        let mut packet = |status: &str, extra: &str, payload: Option<&[u8]>| {
            out.extend_from_slice(format!("\x1b]5522;type=read:status={status}{extra}").as_bytes());
            if let Some(p) = payload {
                out.push(b';');
                out.extend_from_slice(b64(p).as_bytes());
            }
            out.extend_from_slice(st);
        };
        let loc = if self.selection == b'c' { "" } else { ":loc=primary" };
        packet("OK", &format!("{loc}{id}"), None);
        if k.list {
            let held: &[u8] = if text.is_empty() { b"" } else { b"text/plain\n" };
            packet("DATA", &format!("{id}:mime={}", b64(b".")), (!held.is_empty()).then_some(held));
        }
        if let Some(mime) = k.mimes.iter().find(|m| m.starts_with("text/")) {
            for chunk in text.chunks(KITTY_CHUNK) {
                packet("DATA", &format!("{id}:mime={}", b64(mime.as_bytes())), Some(chunk));
            }
        }
        packet("DONE", &id, None);
        out
    }
}

/// A read libghostty is about to answer by default, and what the binding
/// needs to raise it once that answer shows which protocol it was.
pub(crate) struct Pending {
    selection: u8,
    mimes: Vec<String>,
    list: bool,
}

/// Where the clipboard callbacks keep their state, in the terminal's
/// callback state.
#[derive(Default)]
pub(crate) struct ClipboardState {
    pending: Option<Pending>,
    /// The text of the last paste, for the read that follows a paste event.
    pasted: Option<Vec<u8>>,
    /// What write_pty produced during a paste.
    capture: Option<Vec<u8>>,
}

/// Takes libghostty's default answer to a pending read instead of writing
/// it, and raises the read. True when data was such an answer.
pub(crate) fn intercept(ud: *mut c_void, data: &[u8]) -> bool {
    let s = state(ud);
    let Some(p) = s.clip.pending.take() else { return false };
    let bel = data.ends_with(b"\x07");
    let kitty = if data.starts_with(b"\x1b]52;") {
        None
    } else if let Some(rest) = data.strip_prefix(b"\x1b]5522;type=read:status=EPERM".as_slice()) {
        let meta = rest.strip_suffix(if bel { b"\x07".as_slice() } else { b"\x1b\\".as_slice() }).unwrap_or(rest);
        let id = meta.split(|&c| c == b':').find_map(|kv| kv.strip_prefix(b"id=")).unwrap_or_default();
        Some(KittyRead { id: String::from_utf8_lossy(id).into_owned(), mimes: p.mimes, list: p.list })
    } else {
        return false;
    };
    if let Some(f) = s.cb.clipboard_read.as_mut() {
        f(ClipboardRead { selection: p.selection, kitty, bel });
    }
    true
}

/// Called by libghostty for a read. A read that follows a paste event
/// (granted) is answered now, from the paste: true with data set. Others
/// are left pending.
#[no_mangle]
extern "C" fn osewa_rs_clipboard_read(
    ud: *mut c_void,
    location: i32,
    mimes: *const *const u8,
    mime_lens: *const usize,
    n: usize,
    list: bool,
    granted: bool,
    data: *mut *const u8,
    len: *mut usize,
) -> bool {
    let s = state(ud);
    if s.cb.clipboard_read.is_none() {
        return false;
    }
    if granted {
        if let Some(p) = &s.clip.pasted {
            unsafe {
                *data = p.as_ptr();
                *len = p.len();
            }
            return true;
        }
    }
    let mimes = (0..n)
        .map(|i| unsafe { String::from_utf8_lossy(crate::bytes(*mimes.add(i), *mime_lens.add(i))).into_owned() })
        .collect();
    // GhosttyClipboardLocation: standard, selection, primary.
    let selection = match location {
        1 => b's',
        2 => b'p',
        _ => b'c',
    };
    s.clip.pending = Some(Pending { selection, mimes, list });
    false
}

/// Keeps what write_pty is given during a paste, for paste to return.
pub(crate) fn capture(ud: *mut c_void, data: &[u8]) -> bool {
    match state(ud).clip.capture.as_mut() {
        Some(c) => {
            c.extend_from_slice(data);
            true
        }
        None => false,
    }
}

impl Terminal {
    /// What pasting text from the clipboard writes to the PTY, as the
    /// program's modes have it: bracketed (mode 2004), or a Kitty paste
    /// event (mode 5522) when the terminal answers clipboard reads, the
    /// program's follow-up read then answered with text. Unsafe bytes are
    /// replaced; the text is the user's to paste, so newlines are not
    /// refused.
    pub fn paste(&mut self, text: &str) -> Result<Vec<u8>> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let st = unsafe { &mut *self.state };
        st.clip.capture = Some(Vec::new());
        let r = unsafe { ffi::osewa_term_paste(self.t, text.as_ptr(), text.len()) };
        let out = st.clip.capture.take().unwrap_or_default();
        crate::check(r, "paste")?;
        st.clip.pasted = Some(text.as_bytes().to_vec());
        Ok(out)
    }
}
