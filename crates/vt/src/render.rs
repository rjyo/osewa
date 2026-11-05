//! Drawing the mirror on the local terminal.
//!
//! libghostty's VT format is: a preamble (modes, tab stops), CSI H, then one
//! line per row (scrollback first) separated by CRLF, each styling itself
//! from a reset, then a tail that places the cursor and sets the current
//! style. The helpers below cut it along those lines; the render tests pin
//! the shape against the linked libghostty.

use std::collections::HashMap;

use osewa_ghostty::{Format, Image, PixelFormat, Placement, Terminal};
use osewa_mirror::{CellState, Prediction};

/// Starts the screen over before the mirror's VT dump.
pub const REDRAW: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

/// Turns off modes a skipped stretch of output may have left on; the
/// redraw's preamble turns back on the ones still set.
const MODE_RESET: &[u8] = b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b[?25h";

const HOME: &[u8] = b"\x1b[H";

/// The mouse encodings, oldest first: UTF-8 (1005), urxvt (1015), SGR
/// (1006), SGR pixels (1016).
const MOUSE_FORMATS: [u16; 4] = [1005, 1015, 1006, 1016];

/// Sets the mouse encodings that are on again, oldest first. A terminal
/// takes the encoding last set, which the VT format can't say: it lists
/// the modes by number, so a program's `?1015h ?1006h` (urxvt as a fallback,
/// then SGR) would come back as urxvt, whose reports programs that asked
/// for SGR (herdr) don't read. Programs that set several set the one they
/// want last, the newest, which this leaves in effect.
fn mouse_format(t: &Terminal) -> Vec<u8> {
    let mut out = Vec::new();
    for m in MOUSE_FORMATS {
        if t.mode(m, false) {
            out.extend_from_slice(format!("\x1b[?{m}h").as_bytes());
        }
    }
    out
}
const ORIGIN_ON: &[u8] = b"\x1b[?6h";

fn vt(t: &Terminal) -> Vec<u8> {
    t.format(Format::VtNoKeys, false, false).unwrap_or_default()
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
pub struct VtParts<'a> {
    pub pre: &'a [u8],
    pub rows: Vec<&'a [u8]>,
    pub tail: &'a [u8],
}

/// Splits a VT format into preamble, rows and tail.
pub fn vt_parts(vt: &[u8]) -> Option<VtParts<'_>> {
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
pub fn attach_redraw(t: &Terminal) -> Vec<u8> {
    let mut out = REDRAW.to_vec();
    out.extend_from_slice(&vt(t));
    out.extend_from_slice(&mouse_format(t));
    out
}

/// Repaints only the active area from the mirror, restoring cursor and
/// style, without touching the local scrollback. Rows go to absolute
/// positions with the scroll region and origin mode off, so a region the
/// remote set can't scroll them; both come back afterwards. Each row is
/// erased rather than the screen: erasing the screen deletes the images on
/// it, which the mirror still has.
pub fn screen_redraw(t: &Terminal) -> Vec<u8> {
    let v = vt(t);
    let Some(VtParts { pre, rows, tail }) = vt_parts(&v) else { return Vec::new() };
    let rows = &rows[t.scrollback_rows().min(rows.len())..];
    let mut out = b"\x1b[0m\x1b[r\x1b[?6l".to_vec();
    for (y, row) in rows.iter().enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H\x1b[2K", y + 1).as_bytes());
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
pub fn tail_style(t: &Terminal) -> Vec<u8> {
    let v = vt(t);
    let Some(VtParts { tail, .. }) = vt_parts(&v) else { return Vec::new() };
    let (_, e) = csi2_all(tail, b'H')[0];
    tail[e..].to_vec()
}

/// CUP to the mirror's cursor.
pub fn cursor_to(t: &Terminal) -> Vec<u8> {
    let (x, y) = t.cursor();
    format!("\x1b[{};{}H", y + 1, x + 1).into_bytes()
}

/// Draws tentative predictions in the remote's current style, underlined,
/// then restores that style. style is QueryFilter::style().
pub fn overlay_draw(preds: &[Prediction], style: &[u8]) -> Vec<u8> {
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
pub fn notice_draw(t: &Terminal, msg: &str, style: &[u8]) -> Vec<u8> {
    let (cols, rows) = t.size();
    // Short of the last column, where the cursor would wait to wrap.
    let keep = cols.saturating_sub(1);
    let msg: String = if msg.chars().count() > keep { msg.chars().take(keep).collect() } else { msg.into() };
    let mut out = format!("\x1b[{rows};1H\x1b[0;7m{msg}\x1b[0m\x1b[K").into_bytes();
    out.extend_from_slice(style);
    out.extend_from_slice(&cursor_to(t));
    out
}

/// What the local terminal holds of each mirror image: the mirror
/// hash of the pixels it was sent. A hash, not the mirror's generation:
/// generations mean nothing across the mirrors snapshots make, and an image
/// sent again replaces the local terminal's, its animation frames with it.
pub type SentImages = HashMap<u32, u64>;

/// What identifies an image's pixels for SentImages.
pub fn pixels_hash(img: &Image) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let px = img.pixels?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (img.width, img.height, img.format as u8, px).hash(&mut h);
    Some(h.finish())
}

/// Base64 bytes per graphics command; the protocol's suggested chunk size.
const IMAGE_CHUNK: usize = 4096;

/// Replaces the local terminal's image placements with the mirror's: the
/// placements go, then each one on screen is placed again, its image's
/// pixels sent first unless the local terminal has them (sent). An image
/// whose pixels are still pending, and were never sent, waits for the next
/// call. Leaves the cursor at the mirror's.
pub fn images_draw(t: &Terminal, sent: &mut SentImages) -> Vec<u8> {
    let mut out = b"\x1b_Ga=d,d=a,q=2\x1b\\".to_vec();
    let mut placed = false;
    for p in t.placements() {
        if !send_image(t, p.image, sent, &mut out) {
            continue;
        }
        if p.is_virtual {
            out.extend_from_slice(format!("\x1b_Ga=p,U=1,i={},{}c={},r={},q=2\x1b\\", p.image, placement_id(&p), p.cols, p.rows).as_bytes());
        } else if let Some(cmd) = place(t, &p) {
            out.extend_from_slice(&cmd);
            placed = true;
        }
    }
    if placed {
        if t.mode(6, false) {
            out.extend_from_slice(ORIGIN_ON);
        }
        out.extend_from_slice(&cursor_to(t));
    }
    out
}

fn placement_id(p: &Placement) -> String {
    if p.placement == 0 {
        String::new()
    } else {
        format!("p={},", p.placement)
    }
}

/// The command that places p where it is in the mirror, as the program
/// asked for it, or None when it is off screen. Its top may have scrolled
/// above the screen: the rows still showing are placed, from the matching
/// part of the image at the same scale.
fn place(t: &Terminal, p: &Placement) -> Option<Vec<u8>> {
    let (gc, gr) = p.grid;
    if !p.visible || gc == 0 || gr == 0 || p.row <= -(gr as i32) {
        return None;
    }
    let (mut sx, mut sy, mut sw, mut sh) = p.source;
    let (mut row, mut rows, mut y_off) = (p.row, p.rows, p.y_offset);
    if row < 0 {
        let img = t.image(p.image)?;
        // Zero width or height runs to the image's edge.
        sx = sx.min(img.width);
        sy = sy.min(img.height);
        if sw == 0 {
            sw = img.width - sx;
        }
        if sh == 0 {
            sh = img.height - sy;
        }
        let skip = (-row) as u32;
        let (cw, ch) = t.cell_size();
        // How tall it shows, in pixels: as asked for in rows, scaled with
        // the width asked for in columns, or the source's own height.
        let shown = match (p.cols, p.rows) {
            (_, r) if r > 0 => r as u64 * ch as u64,
            (c, _) if c > 0 && sw > 0 => sh as u64 * c as u64 * cw as u64 / sw as u64,
            _ => sh as u64,
        };
        let gone = (skip as u64 * ch as u64).saturating_sub(y_off as u64);
        let cut = if ch == 0 || shown == 0 { sh as u64 * skip as u64 / gr as u64 } else { (gone * sh as u64 / shown).min(sh as u64) };
        sy += cut as u32;
        sh -= cut as u32;
        if sh == 0 {
            return None;
        }
        if p.rows > 0 {
            rows = p.rows.saturating_sub(skip).max(1);
        }
        (row, y_off) = (0, 0);
    }
    let mut cmd = format!("\x1b[?6l\x1b[{};{}H\x1b_Ga=p,i={},{}", row + 1, p.col + 1, p.image, placement_id(p));
    for (k, v) in [('c', p.cols), ('r', rows), ('x', sx), ('y', sy), ('w', sw), ('h', sh), ('X', p.x_offset), ('Y', y_off)] {
        if v > 0 {
            cmd += &format!("{k}={v},");
        }
    }
    cmd += &format!("z={},C=1,q=2\x1b\\", p.z);
    Some(cmd.into_bytes())
}

/// Sends image id's pixels unless the local terminal has them. False when
/// it has none to place.
fn send_image(t: &Terminal, id: u32, sent: &mut SentImages, out: &mut Vec<u8>) -> bool {
    use base64::Engine as _;
    let Some(img) = t.image(id) else { return false };
    let (Some(px), Some(hash)) = (img.pixels, pixels_hash(&img)) else { return sent.contains_key(&id) };
    if sent.get(&id) == Some(&hash) {
        return true;
    }
    // Kitty takes 24 and 32-bit pixels only.
    let (f, px): (u8, std::borrow::Cow<[u8]>) = match img.format {
        PixelFormat::Rgb => (24, px.into()),
        PixelFormat::Rgba => (32, px.into()),
        PixelFormat::Gray => (24, px.iter().flat_map(|&g| [g, g, g]).collect::<Vec<_>>().into()),
        PixelFormat::GrayAlpha => (32, px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect::<Vec<_>>().into()),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&px);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(IMAGE_CHUNK).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        let keys = if i == 0 { format!("a=t,f={f},s={},v={},i={id},q=2,m={more}", img.width, img.height) } else { format!("m={more}") };
        out.extend_from_slice(format!("\x1b_G{keys};").as_bytes());
        out.extend_from_slice(c);
        out.extend_from_slice(b"\x1b\\");
    }
    sent.insert(id, hash);
    true
}

/// Puts the colors programs set in the mirror (OSC 4, 10, 11, 12) on the
/// local terminal, after resetting it to its own: the mirror's defaults
/// are another terminal's. Nothing when there are none and none were
/// passed before (set).
pub fn colors_redraw(t: &Terminal, set: bool) -> (Vec<u8>, bool) {
    let (now, d) = (t.colors(), t.default_colors());
    let mut sets = Vec::new();
    for (n, c, dc) in [(10, now.foreground, d.foreground), (11, now.background, d.background), (12, now.cursor, d.cursor)] {
        if let (Some(c), true) = (c, c != dc) {
            sets.extend_from_slice(format!("\x1b]{n};{}\x1b\\", osewa_wire::rgb_hex(c)).as_bytes());
        }
    }
    let pal: Vec<String> = (0..256).filter(|&i| now.palette[i] != d.palette[i]).map(|i| format!("{i};{}", osewa_wire::rgb_hex(now.palette[i]))).collect();
    if !pal.is_empty() {
        sets.extend_from_slice(format!("\x1b]4;{}\x1b\\", pal.join(";")).as_bytes());
    }
    let any = !sets.is_empty();
    if !any && !set {
        return (Vec::new(), false);
    }
    let mut out = COLORS_RESET.to_vec();
    out.extend_from_slice(&sets);
    (out, any)
}

/// Puts the local terminal's colors back to its own.
pub const COLORS_RESET: &[u8] = b"\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\";

/// OSC 22 that sets the local terminal's pointer shape; "text" is the
/// shape a terminal starts with, so it resets instead.
pub fn pointer_set(shape: &str) -> Vec<u8> {
    let shape = if shape == "text" { "" } else { shape };
    format!("\x1b]22;{shape}\x1b\\").into_bytes()
}

/// A desktop notification on the local terminal: OSC 777 with a title,
/// OSC 9 without, which more terminals take. Neither may carry controls,
/// nor the title a ';'.
pub fn notify_set(title: &str, body: &str) -> Vec<u8> {
    let body = String::from_utf8_lossy(&crate::vtfilter::strip_controls(body.as_bytes())).into_owned();
    let title = String::from_utf8_lossy(&crate::vtfilter::strip_controls(title.as_bytes())).replace(';', ",");
    if title.is_empty() {
        format!("\x1b]9;{body}\x1b\\").into_bytes()
    } else {
        format!("\x1b]777;notify;{title};{body}\x1b\\").into_bytes()
    }
}

/// OSC 52 that puts data on the local clipboard. The remote's selection
/// target isn't carried, so it is always the clipboard.
pub fn clipboard_set(data: &[u8]) -> Vec<u8> {
    use base64::Engine as _;
    let mut out = b"\x1b]52;c;".to_vec();
    out.extend_from_slice(base64::engine::general_purpose::STANDARD.encode(data).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Redraws the screen and modes from a snapshot taken mid-session.
pub fn snapshot_redraw(t: &Terminal) -> Vec<u8> {
    let v = vt(t);
    let Some(VtParts { pre, .. }) = vt_parts(&v) else {
        let mut out = REDRAW.to_vec();
        out.extend_from_slice(&v);
        out.extend_from_slice(&mouse_format(t));
        return out;
    };
    let mut out = MODE_RESET.to_vec();
    out.extend_from_slice(pre);
    out.extend_from_slice(&mouse_format(t));
    out.extend_from_slice(&screen_redraw(t));
    out
}
