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
extern void osewa_rs_notify(void *ud, const uint8_t *title, size_t title_len,
                            const uint8_t *body, size_t body_len);
extern int osewa_rs_color_scheme(void *ud);
extern bool osewa_rs_clipboard_read(void *ud, int location,
                                    const uint8_t *const *mimes,
                                    const size_t *lens, size_t n, bool list,
                                    bool granted, const uint8_t **data,
                                    size_t *len);
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

// Clipboard reads are mostly left unanswered here: libghostty then answers
// them by default, and the Rust side holds that answer back so one can come
// later (clipboard.rs). The read following a paste event is answered now,
// as text, under the first text type it asked for.
#define CLIPBOARD_MIMES 16
static void on_clipboard_read(GhosttyTerminal t, void *ud,
                              const GhosttyClipboardRead *r) {
  (void)t;
  const uint8_t *mimes[CLIPBOARD_MIMES];
  size_t lens[CLIPBOARD_MIMES];
  size_t n = r->mimes_len < CLIPBOARD_MIMES ? r->mimes_len : CLIPBOARD_MIMES;
  for (size_t i = 0; i < n; i++) {
    mimes[i] = r->mimes[i].ptr;
    lens[i] = r->mimes[i].len;
  }
  const uint8_t *data = NULL;
  size_t len = 0;
  if (!osewa_rs_clipboard_read(ud, (int)r->location, mimes, lens, n, r->list,
                               r->granted, &data, &len)) {
    return;
  }
  GhosttyString text = {(const uint8_t *)"text/plain", 10};
  GhosttyString mime = text;
  for (size_t i = 0; i < n; i++) {
    if (lens[i] >= 5 && memcmp(mimes[i], "text/", 5) == 0) {
      mime = r->mimes[i];
      break;
    }
  }
  GhosttyClipboardContent c = {.mime = mime, .data = {data, len}};
  GhosttyClipboardReadReply reply = {
      .size = sizeof(reply),
      .result = GHOSTTY_CLIPBOARD_READ_RESULT_SUCCESS,
      .contents = &c,
      .contents_len = 1,
      .available = &text,
      .available_len = 1,
      .remember = false,
  };
  r->reply(r, &reply);
}

static void on_program_status(GhosttyTerminal t, void *ud,
                              const GhosttyTerminalProgramStatus *r) {
  (void)t;
  osewa_rs_status(ud, (int)r->state, (int)r->kind, (int)r->progress,
                  r->id.ptr, r->id.len, r->app.ptr, r->app.len, r->title.ptr,
                  r->title.len, r->message.ptr, r->message.len);
}

static void on_notify(GhosttyTerminal t, void *ud,
                      const GhosttyTerminalDesktopNotification *n) {
  (void)t;
  osewa_rs_notify(ud, n->title.ptr, n->title.len, n->body.ptr, n->body.len);
}

// DSR 996: the owner's light or dark appearance, once a client said which;
// until then the query goes unanswered, as on a terminal without it.
static bool on_color_scheme(GhosttyTerminal t, void *ud,
                            GhosttyColorScheme *out) {
  (void)t;
  int s = osewa_rs_color_scheme(ud);
  if (s < 0) {
    return false;
  }
  *out = s ? GHOSTTY_COLOR_SCHEME_DARK : GHOSTTY_COLOR_SCHEME_LIGHT;
  return true;
}

uint16_t osewa_term_cols(GhosttyTerminal t);
uint16_t osewa_term_rows(GhosttyTerminal t);
uint32_t osewa_term_width_px(GhosttyTerminal t);
uint32_t osewa_term_height_px(GhosttyTerminal t);

// Size reports (CSI 14/16/18/19 t) come from the terminal's own size, the
// owner's, so programs see the size the holder draws for.
static bool on_size(GhosttyTerminal t, void *ud, GhosttySizeReportSize *out) {
  (void)ud;
  out->columns = osewa_term_cols(t);
  out->rows = osewa_term_rows(t);
  out->cell_width = osewa_term_width_px(t) / (out->columns ? out->columns : 1);
  out->cell_height = osewa_term_height_px(t) / (out->rows ? out->rows : 1);
  return true;
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
  if (write_pty) {
    ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                         (const void *)on_write_pty);
    ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_SIZE, (const void *)on_size);
    ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
                         (const void *)on_color_scheme);
    ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ,
                         (const void *)on_clipboard_read);
  }
  ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                       (const void *)on_notify);
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

int osewa_term_resize(GhosttyTerminal t, uint16_t cols, uint16_t rows,
                      uint32_t cell_w, uint32_t cell_h) {
  return ghostty_terminal_resize(t, cols, rows, cell_w, cell_h);
}

int osewa_term_set_image_limit(GhosttyTerminal t, uint64_t limit) {
  return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                              &limit);
}

uint32_t osewa_term_width_px(GhosttyTerminal t) {
  uint32_t v = 0;
  ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_WIDTH_PX, &v);
  return v;
}

uint32_t osewa_term_height_px(GhosttyTerminal t) {
  uint32_t v = 0;
  ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_HEIGHT_PX, &v);
  return v;
}

// Colors cross as 0xRRGGBB, or -1 for none; the palette as 256 RGB
// triples.
static int32_t rgb_int(GhosttyTerminal t, GhosttyTerminalData key) {
  GhosttyColorRgb c;
  if (ghostty_terminal_get(t, key, &c) != GHOSTTY_SUCCESS) {
    return -1;
  }
  return (int32_t)c.r << 16 | (int32_t)c.g << 8 | (int32_t)c.b;
}

// The effective colors, or with defaults only what the embedder set.
void osewa_term_colors(GhosttyTerminal t, bool defaults, int32_t dyn[3],
                       uint8_t palette[768]) {
  if (defaults) {
    dyn[0] = rgb_int(t, GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND_DEFAULT);
    dyn[1] = rgb_int(t, GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND_DEFAULT);
    dyn[2] = rgb_int(t, GHOSTTY_TERMINAL_DATA_COLOR_CURSOR_DEFAULT);
  } else {
    dyn[0] = rgb_int(t, GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND);
    dyn[1] = rgb_int(t, GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND);
    dyn[2] = rgb_int(t, GHOSTTY_TERMINAL_DATA_COLOR_CURSOR);
  }
  GhosttyColorRgb p[256];
  memset(p, 0, sizeof(p));
  ghostty_terminal_get(t,
                       defaults ? GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT
                                : GHOSTTY_TERMINAL_DATA_COLOR_PALETTE,
                       p);
  for (int i = 0; i < 256; i++) {
    palette[3 * i] = p[i].r;
    palette[3 * i + 1] = p[i].g;
    palette[3 * i + 2] = p[i].b;
  }
}

static int set_rgb(GhosttyTerminal t, GhosttyTerminalOption key, int32_t v) {
  if (v < 0) {
    return ghostty_terminal_set(t, key, NULL);
  }
  GhosttyColorRgb c = {(uint8_t)(v >> 16), (uint8_t)(v >> 8), (uint8_t)v};
  return ghostty_terminal_set(t, key, &c);
}

// Sets the default colors; a NULL palette leaves the palette as it is.
int osewa_term_set_colors(GhosttyTerminal t, const int32_t dyn[3],
                          const uint8_t *palette) {
  int r = set_rgb(t, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, dyn[0]);
  if (r == GHOSTTY_SUCCESS) {
    r = set_rgb(t, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, dyn[1]);
  }
  if (r == GHOSTTY_SUCCESS) {
    r = set_rgb(t, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, dyn[2]);
  }
  if (r == GHOSTTY_SUCCESS && palette != NULL) {
    GhosttyColorRgb p[256];
    for (int i = 0; i < 256; i++) {
      p[i] = (GhosttyColorRgb){palette[3 * i], palette[3 * i + 1],
                               palette[3 * i + 2]};
    }
    r = ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, p);
  }
  return r;
}

// Advances animated images to now_ms; true with next_ms set while one is
// playing.
bool osewa_term_animate(GhosttyTerminal t, uint64_t now_ms,
                        uint64_t *next_ms) {
  GhosttyKittyGraphics g = NULL;
  if (ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &g) !=
          GHOSTTY_SUCCESS ||
      g == NULL) {
    return false;
  }
  return ghostty_kitty_graphics_animation_tick(g, now_ms, next_ms) ==
         GHOSTTY_SUCCESS;
}

int osewa_term_mouse_shape(GhosttyTerminal t) {
  GhosttyMouseShape v = GHOSTTY_MOUSE_SHAPE_TEXT;
  ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE, &v);
  return (int)v;
}

// Implemented in Rust: decodes PNG data into RGBA pixels it allocates with
// osewa_png_alloc(alloc, len).
extern bool osewa_rs_decode_png(const void *alloc, const uint8_t *data,
                                size_t len, uint32_t *w, uint32_t *h,
                                uint8_t **px, size_t *px_len);

uint8_t *osewa_png_alloc(const void *alloc, size_t len) {
  return ghostty_alloc((const GhosttyAllocator *)alloc, len);
}

static bool decode_png(void *ud, const GhosttyAllocator *alloc,
                       const uint8_t *data, size_t len, GhosttySysImage *out) {
  (void)ud;
  return osewa_rs_decode_png(alloc, data, len, &out->width, &out->height,
                             &out->data, &out->data_len);
}

void osewa_png_install(void) {
  ghostty_sys_set(GHOSTTY_SYS_OPT_DECODE_PNG, (const void *)decode_png);
}

// Implemented in Rust: one placement of the active screen's storage, as
// osewa_placements reports it.
extern void osewa_rs_placement(void *ud, const uint32_t *v, int32_t z,
                               int32_t col, int32_t row, bool visible,
                               bool is_virtual);

// Reports every placement of the active screen: image and placement ids,
// the placement's own values as the program sent them, and where it sits in
// the viewport. v holds image, placement, x offset, y offset, source x, y,
// width, height, columns, rows, then the grid columns and rows it covers.
int osewa_placements(GhosttyTerminal t, void *ud) {
  GhosttyKittyGraphics g = NULL;
  int r = ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &g);
  if (r != GHOSTTY_SUCCESS) return r;
  GhosttyKittyGraphicsPlacementIterator it = NULL;
  r = ghostty_kitty_graphics_placement_iterator_new(NULL, &it);
  if (r != GHOSTTY_SUCCESS) return r;
  r = ghostty_kitty_graphics_get(g, GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR,
                                 &it);
  if (r != GHOSTTY_SUCCESS) {
    ghostty_kitty_graphics_placement_iterator_free(it);
    return r;
  }
  static const GhosttyKittyGraphicsPlacementData keys[10] = {
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_X_OFFSET,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Y_OFFSET,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_X,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_Y,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_WIDTH,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_HEIGHT,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_COLUMNS,
      GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_ROWS,
  };
  while (ghostty_kitty_graphics_placement_next(it)) {
    uint32_t v[12] = {0};
    for (int i = 0; i < 10; i++)
      ghostty_kitty_graphics_placement_get(it, keys[i], &v[i]);
    int32_t z = 0;
    ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z, &z);
    bool is_virtual = false;
    ghostty_kitty_graphics_placement_get(
        it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL, &is_virtual);
    GhosttyKittyGraphicsPlacementRenderInfo info =
        GHOSTTY_INIT_SIZED(GhosttyKittyGraphicsPlacementRenderInfo);
    GhosttyKittyGraphicsImage img = ghostty_kitty_graphics_image(g, v[0]);
    if (img && ghostty_kitty_graphics_placement_render_info(it, img, t, &info) ==
                   GHOSTTY_SUCCESS) {
      v[10] = info.grid_cols;
      v[11] = info.grid_rows;
    } else {
      info.viewport_visible = false;
    }
    osewa_rs_placement(ud, v, z, info.viewport_col, info.viewport_row,
                       info.viewport_visible, is_virtual);
  }
  ghostty_kitty_graphics_placement_iterator_free(it);
  return GHOSTTY_SUCCESS;
}

// An image of the active screen's storage. Returns GHOSTTY_NO_VALUE for an
// unknown id; *px is NULL while its pixels are still pending (restored from
// a snapshot before they arrived). The pixels stay owned by the terminal
// until its next change.
int osewa_image(GhosttyTerminal t, uint32_t id, uint32_t *w, uint32_t *h,
                int *format, const uint8_t **px, size_t *len, uint64_t *gen) {
  GhosttyKittyGraphics g = NULL;
  int r = ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &g);
  if (r != GHOSTTY_SUCCESS) return r;
  GhosttyKittyGraphicsImage img = ghostty_kitty_graphics_image(g, id);
  if (!img) return GHOSTTY_NO_VALUE;
  GhosttyKittyImageFormat f = GHOSTTY_KITTY_IMAGE_FORMAT_RGBA;
  ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_WIDTH, w);
  ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_HEIGHT, h);
  ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_FORMAT, &f);
  ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_GENERATION, gen);
  *format = (int)f;
  *px = NULL;
  *len = 0;
  if (ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR,
                                       px) != GHOSTTY_SUCCESS) {
    *px = NULL;
    return GHOSTTY_SUCCESS;
  }
  ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN, len);
  return GHOSTTY_SUCCESS;
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
                 bool palette, bool active, void *ud) {
  GhosttyFormatterTerminalOptions opts =
      GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalOptions);
  opts.emit = vt ? GHOSTTY_FORMATTER_FORMAT_VT : GHOSTTY_FORMATTER_FORMAT_PLAIN;
  opts.trim = trim;
  opts.unwrap = unwrap;
  // The active area alone: walking the scrollback costs its length, on
  // every call.
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  if (active) {
    GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_ACTIVE};
    int r = ghostty_terminal_grid_ref(t, p, &sel.start);
    if (r != GHOSTTY_SUCCESS) return r;
    p.value.coordinate.x = osewa_term_cols(t) - 1;
    p.value.coordinate.y = osewa_term_rows(t) - 1;
    r = ghostty_terminal_grid_ref(t, p, &sel.end);
    if (r != GHOSTTY_SUCCESS) return r;
    opts.selection = &sel;
  }
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

int osewa_decoder_new(GhosttySnapshotDecoder *out, void *ud,
                      uint64_t image_limit) {
  GhosttyReader r = {.read = reader_read, .userdata = ud};
  int rc = ghostty_snapshot_decoder_new(NULL, out, r);
  if (rc != GHOSTTY_SUCCESS) return rc;
  return ghostty_snapshot_decoder_set(
      *out, GHOSTTY_SNAPSHOT_DECODER_OPT_KITTY_IMAGE_STORAGE_LIMIT,
      &image_limit);
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
static bool paste_read(void *ud, GhosttyString mime, GhosttyWriter w) {
  (void)mime;
  const GhosttyString *text = ud;
  return w.write(w.userdata, text->ptr, text->len);
}

// Pastes text from the clipboard; what it writes goes to write_pty.
int osewa_term_paste(GhosttyTerminal t, const uint8_t *text, size_t len) {
  GhosttyString data = {text, len};
  GhosttyString mime = {(const uint8_t *)"text/plain", 10};
  GhosttyPaste p = {
      .size = sizeof(p),
      .location = GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
      .source = GHOSTTY_PASTE_SOURCE_CLIPBOARD,
      .mimes = &mime,
      .mimes_len = 1,
      .reader = {paste_read, &data},
      .allow_unsafe = true,
  };
  return ghostty_terminal_paste(t, &p, NULL);
}

int osewa_encode_focus(bool focused, char *out, size_t cap, size_t *n) {
  return ghostty_focus_encode(focused ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST,
                              out, cap, n);
}
