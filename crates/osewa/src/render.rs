//! Drawing the mirror on the local terminal.
//!
//! libghostty's VT format is: a preamble (modes, tab stops), CSI H, then one
//! line per row (scrollback first) separated by CRLF, each styling itself
//! from a reset, then a tail that places the cursor and sets the current
//! style. The helpers below cut it along those lines; the render tests pin
//! the shape against the linked libghostty.

use osewa_ghostty::{Format, Terminal};
use osewa_mirror::{CellState, Prediction};

/// Starts the screen over before the mirror's VT dump.
pub(crate) const REDRAW: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

/// Turns off modes a skipped stretch of output may have left on; the
/// redraw's preamble turns back on the ones still set.
const MODE_RESET: &[u8] = b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b[?25h";

const HOME: &[u8] = b"\x1b[H";
const ORIGIN_ON: &[u8] = b"\x1b[?6h";

fn vt(t: &Terminal) -> Vec<u8> {
    t.format(Format::Vt, false, false).unwrap_or_default()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Matches ESC [ digits ; digits <end> at i; returns its length.
fn csi2(b: &[u8], i: usize, end: u8) -> Option<usize> {
    if !b[i..].starts_with(b"\x1b[") {
        return None;
    }
    let mut j = i + 2;
    let digits = |j: &mut usize| {
        let s = *j;
        while *j < b.len() && b[*j].is_ascii_digit() {
            *j += 1;
        }
        *j > s
    };
    if !digits(&mut j) || b.get(j) != Some(&b';') {
        return None;
    }
    j += 1;
    if !digits(&mut j) || b.get(j) != Some(&end) {
        return None;
    }
    Some(j + 1 - i)
}

/// Every non-overlapping match of ESC [ \d+ ; \d+ <end>, as (start, end).
fn csi2_all(b: &[u8], end: u8) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match csi2(b, i, end) {
            Some(n) => {
                out.push((i, i + n));
                i += n;
            }
            None => i += 1,
        }
    }
    out
}

/// A VT format cut into preamble, rows and tail.
pub(crate) struct VtParts<'a> {
    pub pre: &'a [u8],
    pub rows: Vec<&'a [u8]>,
    pub tail: &'a [u8],
}

/// Splits a VT format into preamble, rows and tail.
pub(crate) fn vt_parts(vt: &[u8]) -> Option<VtParts<'_>> {
    let i = find(vt, HOME)?;
    let (pre, body) = (&vt[..i], &vt[i + HOME.len()..]);
    let (t, _) = *csi2_all(body, b'H').last()?;
    let mut rows = Vec::new();
    let mut rest = &body[..t];
    loop {
        match find(rest, b"\r\n") {
            Some(k) => {
                rows.push(&rest[..k]);
                rest = &rest[k + 2..];
            }
            None => {
                rows.push(rest);
                break;
            }
        }
    }
    Some(VtParts { pre, rows, tail: &body[t..] })
}

/// Draws the whole mirror, scrollback included, on a fresh attach. The VT
/// format leaves out the palette: it is libghostty's default, not the
/// remote program's choice, and would recolor the user's terminal.
pub(crate) fn attach_redraw(t: &Terminal) -> Vec<u8> {
    let mut out = REDRAW.to_vec();
    out.extend_from_slice(&vt(t));
    out
}

/// Repaints only the active area from the mirror, restoring cursor and
/// style, without touching the local scrollback. Rows go to absolute
/// positions with the scroll region and origin mode off, so a region the
/// remote set can't scroll them; both come back afterwards.
pub(crate) fn screen_redraw(t: &Terminal) -> Vec<u8> {
    let v = vt(t);
    let Some(VtParts { pre, rows, tail }) = vt_parts(&v) else { return Vec::new() };
    let rows = &rows[t.scrollback_rows().min(rows.len())..];
    let mut out = b"\x1b[0m\x1b[r\x1b[?6l\x1b[2J".to_vec();
    for (y, row) in rows.iter().enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H", y + 1).as_bytes());
        out.extend_from_slice(row);
    }
    if let Some(&(s, e)) = csi2_all(pre, b'r').first() {
        out.extend_from_slice(&pre[s..e]);
    }
    if find(pre, ORIGIN_ON).is_some() {
        out.extend_from_slice(ORIGIN_ON);
    }
    out.extend_from_slice(tail);
    out
}

/// The SGR sequences at the end of a VT format: the style the redraw leaves
/// in effect.
pub(crate) fn tail_style(t: &Terminal) -> Vec<u8> {
    let v = vt(t);
    let Some(VtParts { tail, .. }) = vt_parts(&v) else { return Vec::new() };
    let (_, e) = csi2_all(tail, b'H')[0];
    tail[e..].to_vec()
}

/// CUP to the mirror's cursor.
pub(crate) fn cursor_to(t: &Terminal) -> Vec<u8> {
    let (x, y) = t.cursor();
    format!("\x1b[{};{}H", y + 1, x + 1).into_bytes()
}

/// Draws tentative predictions in the remote's current style, underlined,
/// then restores that style. style is QueryFilter::style().
pub(crate) fn overlay_draw(preds: &[Prediction], style: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in preds {
        if p.state != CellState::Tentative {
            continue;
        }
        out.extend_from_slice(format!("\x1b[{};{}H\x1b[4m{}", p.y + 1, p.x + 1, p.rune).as_bytes());
    }
    if !out.is_empty() {
        out.extend_from_slice(style);
    }
    out
}

/// Shows msg in reverse video on the bottom row, then restores style and
/// cursor. The caller repaints the screen later to take it away.
pub(crate) fn notice_draw(t: &Terminal, msg: &str, style: &[u8]) -> Vec<u8> {
    let (cols, rows) = t.size();
    // Short of the last column, where the cursor would wait to wrap.
    let keep = cols.saturating_sub(1);
    let msg: String = if msg.chars().count() > keep { msg.chars().take(keep).collect() } else { msg.into() };
    let mut out = format!("\x1b[{rows};1H\x1b[0;7m{msg}\x1b[0m\x1b[K").into_bytes();
    out.extend_from_slice(style);
    out.extend_from_slice(&cursor_to(t));
    out
}

/// Redraws the screen and modes from a snapshot taken mid-session.
pub(crate) fn snapshot_redraw(t: &Terminal) -> Vec<u8> {
    let v = vt(t);
    let Some(VtParts { pre, .. }) = vt_parts(&v) else {
        let mut out = REDRAW.to_vec();
        out.extend_from_slice(&v);
        return out;
    };
    let mut out = MODE_RESET.to_vec();
    out.extend_from_slice(pre);
    out.extend_from_slice(&screen_redraw(t));
    out
}
