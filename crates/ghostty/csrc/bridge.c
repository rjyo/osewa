// The C side of the libghostty binding. Rust sees only primitives and
// opaque pointers: every libghostty struct and enum stays in C, where the
// pinned headers define them, so a header change can't silently skew a
// Rust mirror of its layout.
#include <stdlib.h>
#include <string.h>
#include <ghostty/vt.h>
#include "keys.h"

// Implemented in Rust (src/lib.rs). ud is the Rust callback state.
extern void osewa_rs_write_pty(void *ud, const uint8_t *data, size_t len);
extern void osewa_rs_bell(void *ud);
extern void osewa_rs_title_changed(void *ud);
extern void osewa_rs_clipboard(void *ud, const uint8_t *data, size_t len);
extern void osewa_rs_status(void *ud, int state, int kind, int progress,
                            const uint8_t *id, size_t id_len,
                            const uint8_t *app, size_t app_len,
                            const uint8_t *title, size_t title_len,
                            const uint8_t *msg, size_t msg_len);
extern bool osewa_rs_sink(void *ud, const uint8_t *data, size_t len);
extern size_t osewa_rs_read(void *ud, uint8_t *buf, size_t cap);

static void on_write_pty(GhosttyTerminal t, void *ud, const uint8_t *data,
                         size_t len) {
  (void)t;
  osewa_rs_write_pty(ud, data, len);
}

static void on_bell(GhosttyTerminal t, void *ud) {
  (void)t;
  osewa_rs_bell(ud);
}

static void on_title_changed(GhosttyTerminal t, void *ud) {
  (void)t;
  osewa_rs_title_changed(ud);
}

// Clipboard writes are reported with their first representation's bytes and
// always accepted: the holder decides who sees them.
static void on_clipboard_write(GhosttyTerminal t, void *ud,
                               const GhosttyClipboardWrite *w) {
  (void)t;
  const uint8_t *data = NULL;
  size_t len = 0;
  if (w->contents_len > 0) {
    data = w->contents[0].data.ptr;
    len = w->contents[0].data.len;
  }
  osewa_rs_clipboard(ud, data, len);
  GhosttyClipboardWriteReply reply = {
      .size = sizeof(reply),
      .result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
      .remember = false,
  };
  w->reply(w, &reply);
}

static void on_program_status(GhosttyTerminal t, void *ud,
                              const GhosttyTerminalProgramStatus *r) {
  (void)t;
  osewa_rs_status(ud, (int)r->state, (int)r->kind, (int)r->progress,
                  r->id.ptr, r->id.len, r->app.ptr, r->app.len, r->title.ptr,
                  r->title.len, r->message.ptr, r->message.len);
}

int osewa_term_new(uint16_t cols, uint16_t rows, GhosttyTerminal *out) {
  return ghostty_terminal_new(NULL, out, cols, rows);
}

void osewa_term_free(GhosttyTerminal t) { ghostty_terminal_free(t); }

void osewa_term_set_userdata(GhosttyTerminal t, void *ud) {
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_USERDATA, ud);
}

// Installs the callbacks; the client terminal leaves out WRITE_PTY so it
// never replies to queries.
void osewa_term_install(GhosttyTerminal t, void *ud, bool write_pty) {
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_USERDATA, ud);
  if (write_pty)
    ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                         (const void *)on_write_pty);
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_BELL, (const void *)on_bell);
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                       (const void *)on_title_changed);
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                       (const void *)on_clipboard_write);
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS,
                       (const void *)on_program_status);
}

int osewa_term_set_continuation(GhosttyTerminal t, size_t limit) {
  return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
                              &limit);
}

int osewa_term_set_scrollback(GhosttyTerminal t, size_t limit) {
  return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                              &limit);
}

void osewa_term_write(GhosttyTerminal t, const uint8_t *data, size_t len) {
  ghostty_terminal_vt_write(t, data, len);
}

int osewa_term_resize(GhosttyTerminal t, uint16_t cols, uint16_t rows) {
  return ghostty_terminal_resize(t, cols, rows, 0, 0);
}

static uint16_t get_u16(GhosttyTerminal t, GhosttyTerminalData key) {
  uint16_t v = 0;
  ghostty_terminal_get(t, key, &v);
  return v;
}

uint16_t osewa_term_cols(GhosttyTerminal t) {
  return get_u16(t, GHOSTTY_TERMINAL_DATA_COLS);
}
uint16_t osewa_term_rows(GhosttyTerminal t) {
  return get_u16(t, GHOSTTY_TERMINAL_DATA_ROWS);
}
uint16_t osewa_term_cursor_x(GhosttyTerminal t) {
  return get_u16(t, GHOSTTY_TERMINAL_DATA_CURSOR_X);
}
uint16_t osewa_term_cursor_y(GhosttyTerminal t) {
  return get_u16(t, GHOSTTY_TERMINAL_DATA_CURSOR_Y);
}

size_t osewa_term_scrollback_rows(GhosttyTerminal t) {
  size_t v = 0;
  ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS, &v);
  return v;
}

bool osewa_term_alt_screen(GhosttyTerminal t) {
  GhosttyTerminalScreen v = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &v);
  return v == GHOSTTY_TERMINAL_SCREEN_ALTERNATE;
}

bool osewa_term_at_ground(GhosttyTerminal t) {
  bool v = false;
  if (ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_VT_GROUND, &v) !=
      GHOSTTY_SUCCESS)
    return false;
  return v;
}

// The title's bytes stay owned by the terminal until its next write.
size_t osewa_term_title(GhosttyTerminal t, const uint8_t **ptr) {
  GhosttyString s = {0};
  if (ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_TITLE, &s) !=
      GHOSTTY_SUCCESS)
    return 0;
  *ptr = s.ptr;
  return s.len;
}

bool osewa_term_mode(GhosttyTerminal t, uint16_t number, bool ansi) {
  GhosttyTerminalModeConfig cfg = {.mode = ghostty_mode_new(number, ansi)};
  if (ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_MODE, &cfg) !=
      GHOSTTY_SUCCESS)
    return false;
  return cfg.value;
}

int osewa_snapshot_alloc(GhosttyTerminal t, uint8_t **ptr, size_t *len) {
  return ghostty_snapshot_encode_alloc(t, NULL, ptr, len);
}

void osewa_free(uint8_t *ptr, size_t len) { ghostty_free(NULL, ptr, len); }

static bool sink_write(void *ud, const uint8_t *data, size_t len) {
  return osewa_rs_sink(ud, data, len);
}

// Encodes a snapshot into a Rust sink that may stop the encoder (by
// returning false) once it has what it needs.
int osewa_snapshot_encode(GhosttyTerminal t, void *ud) {
  GhosttyWriter w = {.write = sink_write, .userdata = ud};
  return ghostty_snapshot_encode(t, w);
}

int osewa_format(GhosttyTerminal t, bool vt, bool trim, bool unwrap,
                 bool palette, void *ud) {
  GhosttyFormatterTerminalOptions opts =
      GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalOptions);
  opts.emit = vt ? GHOSTTY_FORMATTER_FORMAT_VT : GHOSTTY_FORMATTER_FORMAT_PLAIN;
  opts.trim = trim;
  opts.unwrap = unwrap;
  if (vt) {
    opts.extra.size = sizeof(opts.extra);
    opts.extra.palette = palette;
    opts.extra.modes = true;
    opts.extra.scrolling_region = true;
    opts.extra.tabstops = true;
    opts.extra.pwd = true;
    opts.extra.keyboard = true;
    opts.extra.screen.size = sizeof(opts.extra.screen);
    opts.extra.screen.cursor = true;
    opts.extra.screen.style = true;
    opts.extra.screen.hyperlink = true;
    opts.extra.screen.protection = true;
    opts.extra.screen.kitty_keyboard = true;
    opts.extra.screen.charsets = true;
  }
  GhosttyFormatter f = NULL;
  int r = ghostty_formatter_terminal_new(NULL, &f, t, opts);
  if (r != GHOSTTY_SUCCESS) return r;
  GhosttyWriter w = {.write = sink_write, .userdata = ud};
  r = ghostty_formatter_format(f, w);
  ghostty_formatter_free(f);
  return r;
}

static bool reader_read(void *ud, uint8_t *buf, size_t cap, size_t *out) {
  *out = osewa_rs_read(ud, buf, cap);
  return true;
}

int osewa_decoder_new(GhosttySnapshotDecoder *out, void *ud) {
  GhosttyReader r = {.read = reader_read, .userdata = ud};
  return ghostty_snapshot_decoder_new(NULL, out, r);
}

int osewa_decoder_ready(GhosttySnapshotDecoder d, GhosttyTerminal *out) {
  return ghostty_snapshot_decoder_ready(d, out);
}

// 0 when every page is applied, 1 for another page, negative on error.
int osewa_decoder_next(GhosttySnapshotDecoder d) {
  int r = ghostty_snapshot_decoder_next(d);
  if (r == GHOSTTY_NO_VALUE) return 0;
  if (r == GHOSTTY_SUCCESS) return 1;
  return r;
}

void osewa_decoder_free(GhosttySnapshotDecoder d) {
  ghostty_snapshot_decoder_free(d);
}

int osewa_success(void) { return GHOSTTY_SUCCESS; }
int osewa_out_of_space(void) { return GHOSTTY_OUT_OF_SPACE; }

uint16_t osewa_mods(bool shift, bool ctrl, bool alt, bool super) {
  uint16_t m = 0;
  if (shift) m |= GHOSTTY_MODS_SHIFT;
  if (ctrl) m |= GHOSTTY_MODS_CTRL;
  if (alt) m |= GHOSTTY_MODS_ALT;
  if (super) m |= GHOSTTY_MODS_SUPER;
  return m;
}

// The libghostty key for a W3C code, or -1.
int osewa_key_lookup(const char *code, size_t len) {
  for (size_t i = 0; i < sizeof(osewa_keys) / sizeof(osewa_keys[0]); i++) {
    if (strlen(osewa_keys[i].code) == len &&
        memcmp(osewa_keys[i].code, code, len) == 0)
      return (int)osewa_keys[i].key;
  }
  return -1;
}

// action: 0 press, 1 repeat, 2 release. unshifted 0 leaves it unset.
int osewa_encode_key(GhosttyTerminal t, int key, uint16_t mods, int action,
                     const char *utf8, size_t utf8_len, uint32_t unshifted,
                     char *out, size_t cap, size_t *n) {
  GhosttyKeyEncoder enc;
  int r = ghostty_key_encoder_new(NULL, &enc);
  if (r != GHOSTTY_SUCCESS) return r;
  ghostty_key_encoder_setopt_from_terminal(enc, t);
  GhosttyKeyEvent ev;
  r = ghostty_key_event_new(NULL, &ev);
  if (r != GHOSTTY_SUCCESS) {
    ghostty_key_encoder_free(enc);
    return r;
  }
  GhosttyKeyAction a = GHOSTTY_KEY_ACTION_PRESS;
  if (action == 1) a = GHOSTTY_KEY_ACTION_REPEAT;
  if (action == 2) a = GHOSTTY_KEY_ACTION_RELEASE;
  ghostty_key_event_set_action(ev, a);
  ghostty_key_event_set_key(ev, (GhosttyKey)key);
  ghostty_key_event_set_mods(ev, (GhosttyMods)mods);
  if (utf8_len > 0) {
    ghostty_key_event_set_utf8(ev, utf8, utf8_len);
    if (unshifted) ghostty_key_event_set_unshifted_codepoint(ev, unshifted);
  }
  r = ghostty_key_encoder_encode(enc, ev, out, cap, n);
  ghostty_key_event_free(ev);
  ghostty_key_encoder_free(enc);
  return r;
}

// The encoder rewrites data in place, so it works on a copy.
int osewa_encode_paste(const char *text, size_t len, bool bracketed, char *out,
                       size_t cap, size_t *n) {
  char *data = malloc(len + 1);
  if (!data) return GHOSTTY_OUT_OF_MEMORY;
  memcpy(data, text, len);
  data[len] = 0;
  int r = ghostty_paste_encode(data, len, bracketed, out, cap, n);
  free(data);
  return r;
}

int osewa_encode_focus(bool focused, char *out, size_t cap, size_t *n) {
  return ghostty_focus_encode(focused ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST,
                              out, cap, n);
}
