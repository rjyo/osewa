//! Binds the parts of libghostty-vt that osewa uses: the terminal, its binary
//! snapshot format, input encoders and formatters.
//!
//! libghostty-vt is linked statically from .cache/libghostty, built by
//! scripts/libghostty.sh at a pinned commit. Every osewa process must link
//! the same commit, because snapshot format version 1 is not yet
//! binary-stable.

use std::ffi::c_void;
use std::fmt;

mod ffi;
mod images;
mod input;
pub use images::*;
pub use input::*;

/// The pinned libghostty commit, kept in step with scripts/libghostty.sh.
/// Peers compare it at connect time.
pub const COMMIT: &str = "83fb95624b0439ee660c2679b43b38c8b63769cd";

/// The snapshot format version the pinned commit writes.
pub const SNAPSHOT_VERSION: i32 = 1;

/// Any libghostty failure, with what was called and its result code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub what: &'static str,
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ghostty: call failed: {}: result {}", self.what, self.code)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(r: i32, what: &'static str) -> Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(Error { what, code: r })
    }
}

/// One OSC 7501 program status report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// idle, working, done, blocked, error, clear
    pub state: String,
    /// permission, question, auth, or "" when not blocked
    pub kind: String,
    /// 0-100, or -1 when indeterminate
    pub progress: i32,
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

const STATUS_STATES: [&str; 6] = ["idle", "working", "done", "blocked", "error", "clear"];
const STATUS_KINDS: [&str; 4] = ["", "permission", "question", "auth"];

fn name_at(names: &[&str], i: i32) -> String {
    usize::try_from(i).ok().and_then(|i| names.get(i)).unwrap_or(&"").to_string()
}

/// A callback that receives bytes.
pub type BytesFn = Box<dyn FnMut(&[u8])>;

/// The terminal's side effects. Each runs synchronously inside
/// [`Terminal::write`]; it must not call back into the same terminal.
#[derive(Default)]
pub struct Callbacks {
    /// Query replies (DSR, DA, DECRQM and the like). A client terminal leaves
    /// it None so it never replies.
    pub write_pty: Option<BytesFn>,
    pub bell: Option<Box<dyn FnMut()>>,
    pub title: Option<Box<dyn FnMut(String)>>,
    /// OSC 52 writes, already base64-decoded.
    pub clipboard: Option<BytesFn>,
    /// Program status reports (OSC 7501).
    pub status: Option<Box<dyn FnMut(Status)>>,
    /// Desktop notifications (OSC 9, OSC 777): title, possibly empty, and
    /// body.
    pub notify: Option<Box<dyn FnMut(String, String)>>,
    /// Answers DSR 996 on a terminal that replies: Some(true) for dark,
    /// Some(false) for light, None to leave it unanswered.
    pub color_scheme: Option<Box<dyn FnMut() -> Option<bool>>>,
}

/// A color's red, green and blue.
pub type Rgb = [u8; 3];

/// A terminal's colors. Foreground, background and cursor are None when
/// there is no such color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub palette: [Rgb; 256],
}

/// Options for a new terminal.
#[derive(Default)]
pub struct Options {
    /// Caps scrollback; zero keeps libghostty's default.
    pub scrollback_bytes: usize,
    /// Continuation tracking, which a terminal needs before its first write
    /// if it will ever be snapshotted mid-sequence.
    pub continuation: bool,
    /// Keeps Kitty graphics images, up to IMAGE_BYTES per screen.
    pub images: bool,
    pub callbacks: Callbacks,
}

// The callback state libghostty's userdata points at. It lives in its own
// heap cell so its address is stable, and is reached only through that
// pointer while C may call back.
struct CbState {
    term: ffi::Term,
    cb: Callbacks,
}

/// Bounds how many bytes of an unfinished sequence the terminal keeps for a
/// snapshot. Longer sequences are aborted on both sides by libghostty itself.
const CONTINUATION_LIMIT: usize = 1 << 20;

/// A libghostty-vt terminal.
pub struct Terminal {
    t: ffi::Term,
    state: *mut CbState,
}

fn valid_size(cols: usize, rows: usize) -> bool {
    cols > 0 && rows > 0 && cols <= 0xffff && rows <= 0xffff
}

impl Terminal {
    /// Creates a terminal of the given size.
    pub fn new(cols: usize, rows: usize, opts: Options) -> Result<Terminal> {
        if !valid_size(cols, rows) {
            return Err(Error { what: "bad size", code: -2 });
        }
        images::install_png();
        let mut t: ffi::Term = std::ptr::null_mut();
        check(unsafe { ffi::osewa_term_new(cols as u16, rows as u16, &mut t) }, "terminal_new")?;
        let term = Terminal::wrap(t, opts.callbacks);
        if opts.continuation {
            check(unsafe { ffi::osewa_term_set_continuation(t, CONTINUATION_LIMIT) }, "continuation")?;
        }
        if opts.scrollback_bytes > 0 {
            check(unsafe { ffi::osewa_term_set_scrollback(t, opts.scrollback_bytes) }, "scrollback")?;
        }
        // libghostty keeps images by default; only a terminal that asks does.
        let limit = if opts.images { IMAGE_BYTES } else { 0 };
        check(unsafe { ffi::osewa_term_set_image_limit(t, limit) }, "image_limit")?;
        Ok(term)
    }

    fn wrap(t: ffi::Term, cb: Callbacks) -> Terminal {
        let write_pty = cb.write_pty.is_some();
        let state = Box::into_raw(Box::new(CbState { term: t, cb }));
        unsafe { ffi::osewa_term_install(t, state.cast(), write_pty) };
        Terminal { t, state }
    }

    /// Replaces the callbacks, for example on a terminal restored from a
    /// snapshot.
    pub fn set_callbacks(&mut self, cb: Callbacks) {
        let write_pty = cb.write_pty.is_some();
        unsafe {
            (*self.state).cb = cb;
            ffi::osewa_term_install(self.t, self.state.cast(), write_pty);
        }
    }

    /// Feeds PTY output to the terminal.
    pub fn write(&mut self, b: &[u8]) {
        if !b.is_empty() {
            unsafe { ffi::osewa_term_write(self.t, b.as_ptr(), b.len()) }
        }
    }

    /// Changes the terminal size, keeping the cell size.
    pub fn resize(&mut self, cols: usize, rows: usize) -> Result<()> {
        let (w, h) = self.cell_size();
        self.resize_cells(cols, rows, w, h)
    }

    /// Changes the terminal size and the cell size in pixels, which places
    /// images and answers size reports (CSI 14 t, 16 t). Zero means unknown.
    pub fn resize_cells(&mut self, cols: usize, rows: usize, cell_w: u32, cell_h: u32) -> Result<()> {
        if !valid_size(cols, rows) {
            return Err(Error { what: "bad size", code: -2 });
        }
        check(unsafe { ffi::osewa_term_resize(self.t, cols as u16, rows as u16, cell_w, cell_h) }, "resize")
    }

    /// The terminal size in cells.
    pub fn size(&self) -> (usize, usize) {
        unsafe { (ffi::osewa_term_cols(self.t) as usize, ffi::osewa_term_rows(self.t) as usize) }
    }

    /// One cell's size in pixels, or zeros when unknown.
    pub fn cell_size(&self) -> (u32, u32) {
        let (cols, rows) = self.size();
        let (w, h) = unsafe { (ffi::osewa_term_width_px(self.t), ffi::osewa_term_height_px(self.t)) };
        (w / cols.max(1) as u32, h / rows.max(1) as u32)
    }

    /// The cursor position in the active area, zero-based.
    pub fn cursor(&self) -> (usize, usize) {
        unsafe { (ffi::osewa_term_cursor_x(self.t) as usize, ffi::osewa_term_cursor_y(self.t) as usize) }
    }

    /// How many rows of history sit above the active area.
    pub fn scrollback_rows(&self) -> usize {
        unsafe { ffi::osewa_term_scrollback_rows(self.t) }
    }

    /// Whether the alternate screen is active.
    pub fn alt_screen(&self) -> bool {
        unsafe { ffi::osewa_term_alt_screen(self.t) }
    }

    /// The title set by OSC 0/2, or "".
    pub fn title(&self) -> String {
        title_of(self.t)
    }

    /// Reports a DEC private (ansi false) or ANSI mode.
    pub fn mode(&self, number: u16, ansi: bool) -> bool {
        unsafe { ffi::osewa_term_mode(self.t, number, ansi) }
    }

    /// The colors in effect: what programs set (OSC 4, 10, 11, 12), over
    /// the defaults.
    pub fn colors(&self) -> Colors {
        self.read_colors(false)
    }

    /// The default colors, as set_default_colors left them; the palette is
    /// the built-in one where none was set.
    pub fn default_colors(&self) -> Colors {
        self.read_colors(true)
    }

    fn read_colors(&self, defaults: bool) -> Colors {
        let (mut d, mut p) = ([0i32; 3], [0u8; 768]);
        unsafe { ffi::osewa_term_colors(self.t, defaults, d.as_mut_ptr(), p.as_mut_ptr()) };
        let rgb = |v: i32| (v >= 0).then_some([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
        let mut palette = [[0u8; 3]; 256];
        for (i, c) in palette.iter_mut().enumerate() {
            c.copy_from_slice(&p[3 * i..3 * i + 3]);
        }
        Colors { foreground: rgb(d[0]), background: rgb(d[1]), cursor: rgb(d[2]), palette }
    }

    /// Sets the colors the terminal has where programs set none, and
    /// answers color queries with; None clears one. Palette entries that
    /// programs set stay.
    pub fn set_default_colors(&mut self, c: &Colors) -> Result<()> {
        let int = |c: Option<Rgb>| c.map_or(-1, |[r, g, b]| (r as i32) << 16 | (g as i32) << 8 | b as i32);
        let d = [int(c.foreground), int(c.background), int(c.cursor)];
        let p: Vec<u8> = c.palette.iter().flatten().copied().collect();
        check(unsafe { ffi::osewa_term_set_colors(self.t, d.as_ptr(), p.as_ptr()) }, "set_colors")
    }

    /// The pointer shape the program asked for (OSC 22), as its CSS name;
    /// "text" until it asks.
    pub fn pointer_shape(&self) -> &'static str {
        let i = unsafe { ffi::osewa_term_mouse_shape(self.t) };
        POINTER_SHAPES.get(i as usize).copied().unwrap_or("text")
    }

    /// Whether the parser is between sequences, where bytes can be inserted
    /// without changing how the stream is interpreted.
    pub fn at_ground(&self) -> bool {
        unsafe { ffi::osewa_term_at_ground(self.t) }
    }

    /// Encodes the terminal's complete state in libghostty's snapshot format.
    /// The terminal must have been created with continuation unless its
    /// parser is at ground.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut n = 0usize;
        check(unsafe { ffi::osewa_snapshot_alloc(self.t, &mut ptr, &mut n) }, "snapshot_encode")?;
        if ptr.is_null() || n == 0 {
            return Ok(Vec::new());
        }
        let v = unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec();
        unsafe { ffi::osewa_free(ptr, n) };
        Ok(v)
    }

    /// Encodes the snapshot only through READY: the renderable screen and the
    /// parser continuation, without history. The encoder is stopped as soon
    /// as READY is written, so scrollback size doesn't matter.
    pub fn snapshot_screen(&self) -> Result<Vec<u8>> {
        let mut sink = Sink { b: Vec::new(), stop_at_ready: true, end: None };
        let r = unsafe { ffi::osewa_snapshot_encode(self.t, (&mut sink as *mut Sink).cast()) };
        if let Some(end) = sink.end {
            sink.b.truncate(end);
            return Ok(sink.b);
        }
        check(r, "snapshot_encode")?;
        Err(Error { what: "snapshot has no READY record", code: -5 })
    }

    /// [`snapshot_screen`](Self::snapshot_screen) without the scrollback rows
    /// that share a page with the screen. libghostty encodes whole pages, so
    /// a screen snapshot of a terminal with history carries hundreds of
    /// history rows it doesn't need (W1 at 80x24: 36 KB instead of 2 KB).
    /// The screen snapshot is restored into a scratch terminal, its
    /// scrollback erased (ED 3), and encoded again. Mid-sequence, where
    /// inserting ED 3 would change the parse, it returns the plain screen
    /// snapshot.
    pub fn snapshot_screen_compact(&self) -> Result<Vec<u8>> {
        let screen = self.snapshot_screen()?;
        if !self.at_ground() {
            return Ok(screen);
        }
        let mut dec = Decoder::new()?;
        dec.feed(&screen);
        let mut scratch = dec.ready(Callbacks::default())?;
        scratch.write(b"\x1b[3J");
        scratch.snapshot_screen()
    }

    /// Renders the active screen. trim drops trailing blanks; unwrap joins
    /// soft-wrapped lines.
    pub fn format(&self, f: Format, trim: bool, unwrap: bool) -> Result<Vec<u8>> {
        self.format_in(f, trim, unwrap, false)
    }

    /// The active area's plain text, one line per row, without the
    /// scrollback: it costs the screen's size, not the history's.
    pub fn active_text(&self) -> Result<Vec<u8>> {
        self.format_in(Format::Plain, false, false, true)
    }

    fn format_in(&self, f: Format, trim: bool, unwrap: bool, active: bool) -> Result<Vec<u8>> {
        let mut sink = Sink { b: Vec::new(), stop_at_ready: false, end: None };
        let vt = f != Format::Plain;
        let r = unsafe {
            ffi::osewa_format(self.t, vt, trim, unwrap, f == Format::VtPalette, active, (&mut sink as *mut Sink).cast())
        };
        check(r, "format")?;
        Ok(sink.b)
    }

    /// The active screen as trimmed plain text, scrollback included.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.format(Format::Plain, true, false).unwrap_or_default()).into_owned()
    }

    pub(crate) fn raw(&self) -> ffi::Term {
        self.t
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            ffi::osewa_term_free(self.t);
            drop(Box::from_raw(self.state));
        }
    }
}

fn title_of(t: ffi::Term) -> String {
    let mut ptr: *const u8 = std::ptr::null();
    let n = unsafe { ffi::osewa_term_title(t, &mut ptr) };
    if n == 0 || ptr.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, n) }).into_owned()
}

/// A formatter output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The screen's text.
    Plain,
    /// Redraws the screen and restores modes, style and cursor. It leaves the
    /// palette alone, which a client drawing onto its own terminal wants.
    Vt,
    /// Vt plus all 256 palette entries as OSC 4.
    VtPalette,
}

// Snapshot record framing, from libghostty's src/terminal/snapshot: a
// 10-byte envelope, then records of tag u16, length u32, CRC32C u32 and the
// payload. READY's tag is 5.
const SNAP_ENVELOPE: usize = 10;
const SNAP_HEADER: usize = 10;
const TAG_READY: u16 = 5;

/// The offset just past the READY record of an encoded snapshot, or its
/// length when it has none.
pub fn ready_end(snap: &[u8]) -> usize {
    find_ready(snap).unwrap_or(snap.len())
}

fn find_ready(b: &[u8]) -> Option<usize> {
    let mut off = SNAP_ENVELOPE;
    while off + SNAP_HEADER <= b.len() {
        let len = u32::from_le_bytes(b[off + 2..off + 6].try_into().unwrap()) as usize;
        let next = off + SNAP_HEADER + len;
        if next > b.len() {
            break;
        }
        if u16::from_le_bytes(b[off..off + 2].try_into().unwrap()) == TAG_READY {
            return Some(next);
        }
        off = next;
    }
    None
}

struct Sink {
    b: Vec<u8>,
    stop_at_ready: bool,
    end: Option<usize>,
}

#[no_mangle]
extern "C" fn osewa_rs_sink(ud: *mut c_void, data: *const u8, len: usize) -> bool {
    let s = unsafe { &mut *(ud as *mut Sink) };
    if len > 0 {
        s.b.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    }
    if s.stop_at_ready {
        if let Some(end) = find_ready(&s.b) {
            s.end = Some(end);
            return false;
        }
    }
    true
}

fn bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || data.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    }
}

fn string(data: *const u8, len: usize) -> String {
    String::from_utf8_lossy(bytes(data, len)).into_owned()
}

fn state<'a>(ud: *mut c_void) -> &'a mut CbState {
    unsafe { &mut *(ud as *mut CbState) }
}

#[no_mangle]
extern "C" fn osewa_rs_write_pty(ud: *mut c_void, data: *const u8, len: usize) {
    if let Some(f) = state(ud).cb.write_pty.as_mut() {
        f(bytes(data, len));
    }
}

#[no_mangle]
extern "C" fn osewa_rs_bell(ud: *mut c_void) {
    if let Some(f) = state(ud).cb.bell.as_mut() {
        f();
    }
}

#[no_mangle]
extern "C" fn osewa_rs_title_changed(ud: *mut c_void) {
    let s = state(ud);
    if let Some(f) = s.cb.title.as_mut() {
        f(title_of(s.term));
    }
}

#[no_mangle]
extern "C" fn osewa_rs_clipboard(ud: *mut c_void, data: *const u8, len: usize) {
    if let Some(f) = state(ud).cb.clipboard.as_mut() {
        f(bytes(data, len));
    }
}

#[no_mangle]
extern "C" fn osewa_rs_notify(ud: *mut c_void, title: *const u8, title_len: usize, body: *const u8, body_len: usize) {
    if let Some(f) = state(ud).cb.notify.as_mut() {
        f(String::from_utf8_lossy(bytes(title, title_len)).into_owned(), String::from_utf8_lossy(bytes(body, body_len)).into_owned());
    }
}

#[no_mangle]
extern "C" fn osewa_rs_color_scheme(ud: *mut c_void) -> i32 {
    match state(ud).cb.color_scheme.as_mut().and_then(|f| f()) {
        Some(dark) => dark as i32,
        None => -1,
    }
}

/// GhosttyMouseShape's values, in order, by the names OSC 22 takes.
const POINTER_SHAPES: [&str; 34] = [
    "default",
    "context-menu",
    "help",
    "pointer",
    "progress",
    "wait",
    "cell",
    "crosshair",
    "text",
    "vertical-text",
    "alias",
    "copy",
    "move",
    "no-drop",
    "not-allowed",
    "grab",
    "grabbing",
    "all-scroll",
    "col-resize",
    "row-resize",
    "n-resize",
    "e-resize",
    "s-resize",
    "w-resize",
    "ne-resize",
    "nw-resize",
    "se-resize",
    "sw-resize",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
    "zoom-in",
    "zoom-out",
];

#[no_mangle]
#[allow(clippy::too_many_arguments)]
extern "C" fn osewa_rs_status(
    ud: *mut c_void,
    st: i32,
    kind: i32,
    progress: i32,
    id: *const u8,
    id_len: usize,
    app: *const u8,
    app_len: usize,
    title: *const u8,
    title_len: usize,
    msg: *const u8,
    msg_len: usize,
) {
    if let Some(f) = state(ud).cb.status.as_mut() {
        f(Status {
            state: name_at(&STATUS_STATES, st),
            kind: name_at(&STATUS_KINDS, kind),
            progress,
            id: string(id, id_len),
            app: string(app, app_len),
            title: string(title, title_len),
            message: string(msg, msg_len),
        });
    }
}

struct DecBuf {
    buf: Vec<u8>,
    off: usize,
}

#[no_mangle]
extern "C" fn osewa_rs_read(ud: *mut c_void, out: *mut u8, cap: usize) -> usize {
    let d = unsafe { &mut *(ud as *mut DecBuf) };
    let n = cap.min(d.buf.len() - d.off);
    if n > 0 {
        unsafe { std::ptr::copy_nonoverlapping(d.buf.as_ptr().add(d.off), out, n) };
        d.off += n;
    }
    n
}

/// Restores a snapshot incrementally: feed bytes as they arrive, call ready
/// once every byte through READY is in, and finish once FINISH is in.
pub struct Decoder {
    d: ffi::Decoder,
    buf: *mut DecBuf,
}

impl Decoder {
    /// A decoder whose terminal keeps images as the holder's does.
    pub fn new() -> Result<Decoder> {
        images::install_png();
        let buf = Box::into_raw(Box::new(DecBuf { buf: Vec::new(), off: 0 }));
        let mut d: ffi::Decoder = std::ptr::null_mut();
        let r = unsafe { ffi::osewa_decoder_new(&mut d, buf.cast(), IMAGE_BYTES) };
        let dec = Decoder { d, buf };
        check(r, "decoder_new")?;
        Ok(dec)
    }

    /// Appends snapshot bytes.
    pub fn feed(&mut self, b: &[u8]) {
        let d = unsafe { &mut *self.buf };
        // Bytes already read are dropped, so a long history doesn't pile up.
        if d.off > 0 && d.off == d.buf.len() {
            d.buf.clear();
            d.off = 0;
        }
        d.buf.extend_from_slice(b);
    }

    /// Decodes through READY and returns the renderable terminal, with the
    /// given callbacks installed. The decoder keeps no ownership of it.
    pub fn ready(&mut self, cb: Callbacks) -> Result<Terminal> {
        let mut t: ffi::Term = std::ptr::null_mut();
        check(unsafe { ffi::osewa_decoder_ready(self.d, &mut t) }, "decoder_ready")?;
        Ok(Terminal::wrap(t, cb))
    }

    /// Applies every remaining history page and validates FINISH. Call it
    /// after all snapshot bytes were fed.
    pub fn finish(&mut self) -> Result<()> {
        loop {
            match unsafe { ffi::osewa_decoder_next(self.d) } {
                0 => return Ok(()),
                1 => continue,
                r => return check(r, "decoder_next"),
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.d.is_null() {
                ffi::osewa_decoder_free(self.d);
            }
            drop(Box::from_raw(self.buf));
        }
    }
}

/// Decodes a complete snapshot into a new terminal.
pub fn restore(snapshot: &[u8], cb: Callbacks) -> Result<Terminal> {
    let mut d = Decoder::new()?;
    d.feed(snapshot);
    let t = d.ready(cb)?;
    d.finish()?;
    Ok(t)
}

#[cfg(test)]
mod tests;
