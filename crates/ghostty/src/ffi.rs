// The C bridge (csrc/bridge.c). Only primitives and opaque handles cross.
use std::ffi::c_void;

pub type Term = *mut c_void;
pub type Decoder = *mut c_void;

extern "C" {
    pub fn osewa_term_new(cols: u16, rows: u16, out: *mut Term) -> i32;
    pub fn osewa_term_free(t: Term);
    pub fn osewa_term_install(t: Term, ud: *mut c_void, write_pty: bool);
    pub fn osewa_term_set_continuation(t: Term, limit: usize) -> i32;
    pub fn osewa_term_set_scrollback(t: Term, limit: usize) -> i32;
    pub fn osewa_term_write(t: Term, data: *const u8, len: usize);
    pub fn osewa_term_resize(t: Term, cols: u16, rows: u16) -> i32;
    pub fn osewa_term_cols(t: Term) -> u16;
    pub fn osewa_term_rows(t: Term) -> u16;
    pub fn osewa_term_cursor_x(t: Term) -> u16;
    pub fn osewa_term_cursor_y(t: Term) -> u16;
    pub fn osewa_term_scrollback_rows(t: Term) -> usize;
    pub fn osewa_term_alt_screen(t: Term) -> bool;
    pub fn osewa_term_at_ground(t: Term) -> bool;
    pub fn osewa_term_title(t: Term, ptr: *mut *const u8) -> usize;
    pub fn osewa_term_mode(t: Term, number: u16, ansi: bool) -> bool;
    pub fn osewa_snapshot_alloc(t: Term, ptr: *mut *mut u8, len: *mut usize) -> i32;
    pub fn osewa_free(ptr: *mut u8, len: usize);
    pub fn osewa_snapshot_encode(t: Term, ud: *mut c_void) -> i32;
    pub fn osewa_format(t: Term, vt: bool, trim: bool, unwrap: bool, palette: bool, ud: *mut c_void) -> i32;
    pub fn osewa_decoder_new(out: *mut Decoder, ud: *mut c_void) -> i32;
    pub fn osewa_decoder_ready(d: Decoder, out: *mut Term) -> i32;
    pub fn osewa_decoder_next(d: Decoder) -> i32;
    pub fn osewa_decoder_free(d: Decoder);
    pub fn osewa_out_of_space() -> i32;
    pub fn osewa_mods(shift: bool, ctrl: bool, alt: bool, sup: bool) -> u16;
    pub fn osewa_key_lookup(code: *const u8, len: usize) -> i32;
    #[allow(clippy::too_many_arguments)]
    pub fn osewa_encode_key(
        t: Term,
        key: i32,
        mods: u16,
        action: i32,
        utf8: *const u8,
        utf8_len: usize,
        unshifted: u32,
        out: *mut u8,
        cap: usize,
        n: *mut usize,
    ) -> i32;
    pub fn osewa_encode_paste(text: *const u8, len: usize, bracketed: bool, out: *mut u8, cap: usize, n: *mut usize) -> i32;
    pub fn osewa_encode_focus(focused: bool, out: *mut u8, cap: usize, n: *mut usize) -> i32;
}
