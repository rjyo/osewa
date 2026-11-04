/* osewa: one remote terminal session for an app's own terminal.
 *
 * The app's terminal gets VT bytes through `output`: a full repaint after
 * each snapshot, then the remote output as it comes, with local echo
 * predictions drawn over it. Keys go up as raw bytes from the app's key
 * encoder. The connection survives network changes by itself: it redials
 * (QUIC or TLS WebSocket) and resumes the session where it left off.
 *
 * Callbacks run on osewa's threads; never call osewa_* (other than
 * osewa_send, osewa_resize and osewa_paste) from inside a callback. */
#ifndef OSEWA_H
#define OSEWA_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct osewa_session osewa_session;

enum { OSEWA_CONNECTING = 0, OSEWA_CONNECTED = 1, OSEWA_RECONNECTING = 2, OSEWA_CLOSED = 3 };

/* osewa_send and osewa_paste: 0 ok; OSEWA_ERR for a closed session or bad
   arguments; OSEWA_ERR_FULL when 1 MiB of input is already outstanding
   (queued, or sent and not yet acked by the host, as during an outage):
   nothing was taken, try again later. */
enum { OSEWA_ERR = -1, OSEWA_ERR_FULL = -2 };

typedef struct osewa_callbacks {
  void *ctx;
  /* VT bytes for the app's own terminal (a full repaint after each snapshot,
     then incremental). Borrowed for the call only. */
  void (*output)(void *ctx, const uint8_t *bytes, size_t len);
  /* state: OSEWA_*; detail NULL or UTF-8 (on CLOSED: why, e.g. "exit 0",
     "incompatible: ...", "unauthorized: ..."). "incompatible: session <id>
     can't be attached: ..." is a session whose host process is another
     build than the server: it never attaches, but osewa_kill_id ends it. */
  void (*state)(void *ctx, int32_t state, const char *detail);
  /* smoothed RTT in microseconds and the carrier: "quic" or "ws". */
  void (*stats)(void *ctx, uint32_t srtt_us, const char *carrier);
  /* JSON events: {"type":"session","id":"<hex>"} once attached (persist it),
     {"type":"title","title":...}, {"type":"bell"}, {"type":"exit","code":n},
     {"type":"notify",...}, anything else the holder sends, passed through. */
  void (*event)(void *ctx, const char *json);
} osewa_callbacks;

typedef struct osewa_open_options {
  const char *addr;            /* "host:port" or "[v6]:port" (WebSocket/TLS port, default 7322) */
  const char *server_key;      /* base64, from enroll */
  uint16_t quic_port;          /* 0 = unknown, learned from WELCOME */
  const uint8_t *device_key;   /* PKCS#8 DER Ed25519 */
  size_t device_key_len;
  const char *client_id;       /* stable per install */
  const char *device_name;
  const char *session_id;      /* NULL or "" = create a new session */
  const char *argv_json;       /* JSON array for create; NULL = the user's login shell */
  const char *cwd;             /* NULL = home */
  uint16_t cols, rows;
  uint32_t width_px, height_px;
  int32_t predict;             /* 1 = local echo prediction */
  const char *appearance_json; /* NULL or {"color_scheme":"dark"|"light","fg":"#rrggbb","bg":"#rrggbb","palette":["#..",...]} */
  const char *tag;             /* NULL or what the session is for (a card): a create
                                  reuses this client's live session of the same tag, or
                                  replaces it when argv/cwd differ; osewa_reconcile ends
                                  tags no longer kept. Printable, at most 128 bytes. */
} osewa_open_options;

/* Never blocks on the network; connection progress arrives via callbacks.
   NULL + *err (free with osewa_string_free) on bad options. */
osewa_session *osewa_open(const osewa_open_options *opts, osewa_callbacks cb, char **err);
/* Raw bytes from the app's key encoder; split UTF-8 runes are held back.
   0, OSEWA_ERR or OSEWA_ERR_FULL. */
int32_t osewa_send(osewa_session *s, const uint8_t *buf, size_t len);
int32_t osewa_paste(osewa_session *s, const char *utf8); /* as osewa_send */
void osewa_resize(osewa_session *s, uint16_t cols, uint16_t rows, uint32_t width_px, uint32_t height_px);
/* Repaint the whole screen through `output` (the app's view was recreated). */
void osewa_redraw(osewa_session *s);
/* Wake/foreground hint: probe the link now instead of waiting for keepalive. */
void osewa_network_changed(osewa_session *s);
/* Background/hidden: close the connection and stop keepalive and redials
   until osewa_wake. The session stays resumable; no state change is
   reported for the pause. osewa_wake dials at once and resumes it. */
void osewa_pause(osewa_session *s);
void osewa_wake(osewa_session *s);
/* Close locally; the session stays alive and resumable on the host. Frees s.
   No callbacks fire after it returns.
   Threads: osewa_* calls on one handle may come from any thread, but none
   may run concurrently with osewa_close or osewa_kill on it from another
   thread, nor after either: synchronize them. Only calls made from inside
   this handle's callbacks are guaranteed safe while it closes (close waits
   for a running callback first). */
void osewa_close(osewa_session *s);
/* Ask the host to end the session (kill), then close. Frees s. */
void osewa_kill(osewa_session *s);
/* Ask the host to end session opts->session_id (required) without opening
   it, e.g. one that closed "incompatible: ...". Uses opts' addr, keys,
   client_id and device_name; the rest is ignored. Blocks for the network
   (up to 15 s): call it off the main thread. 0 (also when the session is
   already gone), or OSEWA_ERR + *err (free with osewa_string_free). */
int32_t osewa_kill_id(const osewa_open_options *opts, char **err);
/* End this client's tagged sessions whose tag isn't in keep_json (a JSON
   array of strings), except those started in the last 2 minutes. Uses opts'
   addr, keys, client_id and device_name. Blocks for the network (up to
   15 s). 0 and *out (a JSON array of the ended sessions; free with
   osewa_string_free), or OSEWA_ERR + *err. A server before tags ends
   nothing: "[]". */
int32_t osewa_reconcile(const osewa_open_options *opts, const char *keep_json, char **out, char **err);

/* Links: one connection to a server, kept up and redialed as a session's
   is, that several sessions share (osewa_open_on) and that carries forwards.
   Its callbacks: state as for a session (CLOSED once nothing will get
   through: "incompatible: ...", "unauthorized: ..."); stats with the
   carrier when it changes (srtt_us 0: the link measures none of its own;
   a session's stats do); event {"type":"carriers","quic_port":n}; output
   never. */
typedef struct osewa_link osewa_link;

typedef struct osewa_link_options {
  const char *addr;            /* as osewa_open_options */
  const char *server_key;
  uint16_t quic_port;
  const uint8_t *device_key;
  size_t device_key_len;
  const char *client_id;
  const char *device_name;
} osewa_link_options;

osewa_link *osewa_link_open(const osewa_link_options *opts, osewa_callbacks cb, char **err);
/* A session on l's connection: opts' addr, server_key, quic_port,
   device_key, client_id and device_name are ignored. osewa_pause, osewa_wake
   and osewa_network_changed on it act on the link, so on all its sessions. */
osewa_session *osewa_open_on(osewa_link *l, const osewa_open_options *opts, osewa_callbacks cb, char **err);
/* Listens on 127.0.0.1:local_port (0 = any free port) and forwards each
   connection to host:port as the server dials it ("localhost" is the
   server's loopback), as ssh -L. Returns the port, or 0 and *err. Forwarded
   connections end with the connection they rode; new ones take the next. */
uint16_t osewa_link_forward(osewa_link *l, uint16_t local_port, const char *host, uint16_t port, char **err);
void osewa_link_unforward(osewa_link *l, uint16_t local_port);
void osewa_link_network_changed(osewa_link *l);
void osewa_link_pause(osewa_link *l);
void osewa_link_wake(osewa_link *l);
/* Ends the forwards and closes the connection; close its sessions first.
   Waits (3 s at most) for those closes to reach the server, so sessions
   killed just before (as an app quits) end on the host. Frees l; no
   callbacks fire after it returns. */
void osewa_link_close(osewa_link *l);

/* Device keys. */
int32_t osewa_generate_key(uint8_t *out, size_t cap, size_t *len); /* PKCS#8 DER into out */
char *osewa_public_key(const uint8_t *der, size_t len);            /* base64 raw 32 bytes, NULL on error */
/* Static JSON: {"osewa":"<crate version>","version":0,"ghostty":"<commit>","snapshot_version":1};
   two builds interoperate iff version and snapshot_version are equal. */
const char *osewa_build_info(void);
void osewa_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif
