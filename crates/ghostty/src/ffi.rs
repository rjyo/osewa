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
    pub fn osewa_term_resize(t: Term, cols: u16, rows: u16, cell_w: u32, cell_h: u32) -> i32;
    pub fn osewa_term_set_image_limit(t: Term, limit: u64) -> i32;
    pub fn osewa_term_width_px(t: Term) -> u32;
    pub fn osewa_term_height_px(t: Term) -> u32;
    pub fn osewa_png_alloc(alloc: *const c_void, len: usize) -> *mut u8;
    pub fn osewa_png_install();
    pub fn osewa_placements(t: Term, ud: *mut c_void) -> i32;
    #[allow(clippy::too_many_arguments)]
    pub fn osewa_image(
        t: Term,
        id: u32,
        w: *mut u32,
        h: *mut u32,
        format: *mut i32,
        px: *mut *const u8,
        len: *mut usize,
        gen: *mut u64,
    ) -> i32;
    pub fn osewa_term_cols(t: Term) -> u16;
    pub fn osewa_term_rows(t: Term) -> u16;
    pub fn osewa_term_cursor_x(t: Term) -> u16;
    pub fn osewa_term_cursor_y(t: Term) -> u16;
    pub fn osewa_term_scrollback_rows(t: Term) -> usize;
    pub fn osewa_term_alt_screen(t: Term) -> bool;
    pub fn osewa_term_at_ground(t: Term) -> bool;
    pub fn osewa_term_title(t: Term, ptr: *mut *const u8) -> usize;
    pub fn osewa_term_mode(t: Term, number: u16, ansi: bool) -> bool;
    pub fn osewa_term_mouse_shape(t: Term) -> i32;
    pub fn osewa_term_kitty_flags(t: Term) -> i32;
    pub fn osewa_term_animate(t: Term, now_ms: u64, next_ms: *mut u64) -> bool;
    pub fn osewa_term_colors(t: Term, defaults: bool, dyn_: *mut i32, palette: *mut u8);
    pub fn osewa_term_set_colors(t: Term, dyn_: *const i32, palette: *const u8) -> i32;
    pub fn osewa_snapshot_alloc(t: Term, ptr: *mut *mut u8, len: *mut usize) -> i32;
    pub fn osewa_free(ptr: *mut u8, len: usize);
    pub fn osewa_snapshot_encode(t: Term, ud: *mut c_void) -> i32;
    pub fn osewa_format(t: Term, vt: bool, trim: bool, unwrap: bool, palette: bool, keys: bool, active: bool, ud: *mut c_void) -> i32;
    pub fn osewa_decoder_new(out: *mut Decoder, ud: *mut c_void, image_limit: u64) -> i32;
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
    pub fn osewa_term_paste(t: Term, text: *const u8, len: usize, event_ok: bool) -> i32;
    pub fn osewa_encode_focus(focused: bool, out: *mut u8, cap: usize, n: *mut usize) -> i32;
}
