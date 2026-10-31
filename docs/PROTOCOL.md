# osewa protocol, v0

Status: draft. Breaking changes are allowed until v1.

osewa keeps terminal sessions alive on a host and lets clients attach,
detach and resume them. One libghostty terminal on each end interprets every
byte: the host runs the authoritative libghostty-vt terminal next to the PTY,
and a client restores a libghostty binary snapshot, then applies the raw PTY
bytes that follow it.

## Processes

| Process | Count | Role |
| --- | --- | --- |
| holder | one per session | Owns the PTY, the child, the libghostty-vt terminal, the output log, input encoding, the owner and the exit status. Listens on a Unix socket. |
| server | one per Unix user | Terminates TLS, checks device keys, spawns holders and relays frames between clients and holders. Keeps no session state. |
| client | many | Connects to the server, attaches to sessions and renders with libghostty. |

The holder and the server speak the same frame format. The server relays
frames; it never parses snapshot or output payloads.

### Holder sockets

Holders listen at `<run>/<session id>.sock`, where `<run>` is
`$XDG_RUNTIME_DIR/osewa` if set, else `~/.local/state/osewa/run`, mode 0700.
The server spawns a holder with `osewa holder --run <run> --id <id> --cols
<c> --rows <r> [--cwd <dir>] -- <argv...>`, detached with setsid; the holder
prints one line `ready` on stdout once its socket accepts connections. The
child's environment is the holder's environment, so the server passes
`session.create` `env` by adding it to the holder process's environment.

A holder never sends WELCOME; the server answers the client's HELLO itself.
The server closes a client connection whose HELLO has no `client_id`.

The server opens one holder connection per attached stream, and short-lived
connections for `session.list` and `session.kill`. On every holder
connection it first sends HELLO with the real client's `client_id`, then
REQUESTs. A holder answers these methods:

| Method | Notes |
| --- | --- |
| `session.info` | Result in `sessions[0]` |
| `session.attach` | Then streams data frames for `stream` on this connection |
| `session.detach`, `session.own`, `session.resize`, `session.kill` | As above |

The server forwards INPUT, ACK and CREDIT frames for a stream to that
stream's holder connection, and relays everything the holder sends back
unchanged. On `session.detach` the holder sends STREAM_END `detached` before
the RESPONSE. If a holder connection drops mid-stream, the server sends the
client STREAM_END `{"reason":"error"}` for that stream. Each stream has its
own bounded queue toward its holder (4 MiB), so one holder's input
backpressure never stalls the client's other streams; if that queue
overflows, only that stream ends with `error`. A holder socket that exists
but refuses connections gives `unavailable`; a missing one gives `not_found`. When the client
connection drops, the server closes its holder
connections; the holder keeps the client's input seq and ownership for
resume.

A holder whose child has exited keeps its socket for the exit-retention time
(default 24 h) so clients can read the final screen and exit code, then
exits.

## Frames

Every WebSocket binary message, and every frame on a holder socket, is one
frame. Integers are little-endian.

```
+---------+-----------+----------------+
| type u8 | stream u32| payload        |
+---------+-----------+----------------+
```

On a holder Unix socket, frames are length-prefixed: `len u32` (type + stream +
payload bytes), then the frame. On WebSocket the message boundary is the frame
boundary.

`stream` is 0 for connection-level frames. Data frames carry the stream id the
client chose in ATTACH.

Payloads marked JSON are one UTF-8 JSON object. Unknown JSON fields are
ignored. A receiver closes the connection on an unknown frame type.

**Features.** HELLO and WELCOME each list `features`. A peer sends the frame
types and methods of a feature only when both lists have it, so a feature
can be added without breaking older peers. Features in v0: `forward` and
`exec` (see Channel streams). A server may leave one out by configuration
(`osewa server --no-forward`, `--no-exec`); it then answers its methods with
`bad_request`.

A client may send REQUESTs right after HELLO without waiting for WELCOME;
the server handles frames in order. A reconnecting client sends HELLO and
its `session.attach` in one flight.

### Client to server

| Type | Name | Stream | Payload |
| --- | --- | --- | --- |
| 0x01 | HELLO | 0 | JSON `{"version":0,"client_id":"<id>","name":"<label>","features":[...]}` |
| 0x02 | REQUEST | 0 | JSON `{"id":<n>,"method":"<m>", ...args}` |
| 0x03 | INPUT | stream | JSON input event, see Input |
| 0x04 | ACK | stream | `generation u32`, `end u64`: everything up to log position `end` is applied |
| 0x05 | CREDIT | stream | `bytes u32`: the client has buffered this many more data bytes |
| 0x06 | DATA | stream | Bytes for a channel stream: to the forwarded connection, or the process's stdin |
| 0x07 | EOF | stream | empty: the client sends no more DATA on this stream |

### Server to client

| Type | Name | Stream | Payload |
| --- | --- | --- | --- |
| 0x81 | WELCOME | 0 | JSON `{"version":0,"server":"<name>","snapshot_version":1,"ghostty":"<commit>","carriers":[],"features":[...]}` |
| 0x82 | RESPONSE | 0 | JSON `{"id":<n>,"error":"<code>"?,"message":"<text>"?, ...result}` |
| 0x83 | INPUT_ACK | stream | `seq u64`: highest input seq applied for this client |
| 0x84 | GRANT | stream | `bytes u32`: the server accepts this many more DATA bytes |
| 0xA0 | SNAPSHOT_BEGIN | stream | JSON `{"generation":<g>,"epoch":"<e>","position":<p>,"cols":<c>,"rows":<r>}` |
| 0xA1 | SNAPSHOT_DATA | stream | Snapshot bytes before READY's end |
| 0xA2 | SNAPSHOT_READY | stream | empty: the client has every byte through READY |
| 0xA3 | SNAPSHOT_HISTORY | stream | Snapshot bytes after READY, through FINISH |
| 0xA4 | SNAPSHOT_END | stream | empty: FINISH was sent |
| 0xA5 | OUTPUT | stream | `end u64`, then raw PTY bytes |
| 0xA6 | EVENT | stream | `end u64`, then JSON event, see Events |
| 0xA7 | STREAM_END | stream | JSON `{"reason":"<code>","exit_code":<n>?}` |
| 0xA8 | CHANNEL_DATA | stream | `channel u8` (1 stdout, 2 stderr), then bytes |
| 0xA9 | CHANNEL_EOF | stream | `channel u8`: that channel has no more data |

## Sessions

Session ids are 16 random bytes in lowercase hex. They are not secrets.
`created` is Unix seconds. A killed session's exit code is 128 + the signal
(129 for SIGHUP, 137 for SIGKILL).

Methods (REQUEST `method`):

| Method | Arguments | Result |
| --- | --- | --- |
| `session.list` | | `{"sessions":[{"id","title","cols","rows","created","exited","exit_code","owner","attached","idle_seconds","status"?}]}` |
| `session.create` | `cols`, `rows`, `width`?, `height`?, `argv`?, `cwd`?, `env`? | `{"session_id"}` |
| `session.attach` | `session`, `stream`, `cols`, `rows`, `width`?, `height`?, `own`, `epoch`?, `position`? | `{"epoch","owner","resumed","head"}` |
| `session.detach` | `stream` | `{}` |
| `session.own` | `stream` | `{"owner"}` |
| `session.resize` | `stream`, `cols`, `rows`, `width`?, `height`? | `{}` (applies only for the owner) |
| `session.kill` | `session` | `{}` |

`status` is the session's latest root OSC 7501 record
(`state`, `kind`, `app`, `title`, `msg`, `progress`), absent when the
program never reported one or cleared it.

Sizes are 1..2000 columns and 1..1000 rows; anything else is
`bad_request`. `width` and `height` are the text area in pixels (a
winsize's `ws_xpixel` and `ws_ypixel`), 0 or absent when unknown; values
past 65535 count as unknown. The holder's terminal takes its cell size from
them, `width / cols` by `height / rows`: it places images and answers size
reports by it, and the PTY reports them to the program. A client connection
may hold at most 64 streams.

Error codes: `bad_request`, `not_found`, `not_owner`, `ended`, `unavailable`,
`unreachable` (forward.open), `internal`.

`argv` defaults to the user's login shell. `own: true` claims ownership on
attach.

## Output log

Each session has one output log. A record is either output bytes or an event.
A **position** is a byte offset in the log's record stream: every record
occupies `5 + len(payload)` bytes (1 type byte, 4 length bytes, payload),
whether or not anyone reads it that way. OUTPUT and EVENT frames carry `end`,
the position just after their record. The first record starts at 0.

A stream may skip records it must not see (clipboard records go only to the
owner), so the next record's start can be past the previous `end`. Clients
take `end` from each frame and treat a jump as normal.

The log is a ring capped by bytes (default 8 MiB). The **epoch** is a random
id fixed for the holder's life.

### Attach and resume

`session.attach` with `epoch` and `position`:

| Condition | Holder sends |
| --- | --- |
| Same epoch and `position` still in the ring | Log records from `position`, `resumed: true` |
| Same epoch, `position` evicted, or no position | A snapshot, then the log |
| Different epoch | Error `ended` |

`head` is the log end when the holder took the attach. Records with
`end <= head` are catch-up: clients apply them but never fire their effects.
Right after the attach response, the holder sends INPUT_ACK with the highest
seq it has applied for this `client_id` (0 if none), so a client process that
reuses an id continues above it.

Generations count per stream: the first snapshot on a stream is generation 0,
and each later one adds 1. A resumed attach starts again at 0.

A snapshot is cut on the holder loop between PTY reads: the holder encodes
its terminal at position P, sends SNAPSHOT_BEGIN with `position: P`, the
snapshot bytes, then log records from P. Frames on one stream are ordered, so
everything before a SNAPSHOT_BEGIN belongs to the previous generation.

Snapshot bytes are libghostty-vt's snapshot format exactly as encoded by the
pinned libghostty commit. SNAPSHOT_DATA carries the bytes through the end of
the READY record; SNAPSHOT_READY marks that point; SNAPSHOT_HISTORY carries
the rest. The holder may interleave OUTPUT and EVENT frames between
SNAPSHOT_READY and SNAPSHOT_END. A client renders at READY, applies output
as it arrives, and applies history pages whenever it has them.

### Lagging clients and catch-up

A stream is **behind** when its oldest unsent record has waited longer than
max(catch-up delay, 2 x smoothed credit round trip) (catch-up delay default
200 ms), or its unsent bytes exceed the lag threshold (default 4 MiB). A behind stream enters catch-up:

- The holder drops the stream's unsent records and sends a **screen
  snapshot**: SNAPSHOT_BEGIN with `"history": false`, SNAPSHOT_DATA through
  READY, SNAPSHOT_READY, SNAPSHOT_END. No history. The client replaces its
  mirror with the restored screen; it has no scrollback from this snapshot.
- While still behind, the holder sends the next screen snapshot as soon as
  the previous one is credited, so the screen stays as fresh as the link
  allows and a keystroke's echo appears in the next one.
- A screen snapshot may be **abandoned**: when a newer screen is ready
  (for example right after the owner's input) and the current one hasn't
  reached READY, the holder stops sending its remaining SNAPSHOT_DATA and
  sends the new SNAPSHOT_BEGIN. A client that receives SNAPSHOT_BEGIN while
  a snapshot is still before READY discards that partial snapshot and keeps
  showing its current mirror until the new one is ready. Abandoned bytes
  that were sent still count toward credit.
- When no unsent record is older than one smoothed credit round trip,
  after at least the catch-up delay, the holder sends one full snapshot (`"history": true`) to
  restore scrollback, then resumes streaming records.

Effects in skipped output never fire. Clients never slow the program.
SNAPSHOT_BEGIN `history` defaults to true when absent.

### Flow control

Each data stream has a credit window, so a keystroke's echo never waits
behind a deep queue of output already in flight.

- **Counted bytes** are the payload bytes of SNAPSHOT_DATA (except the
  first snapshot of an attach), SNAPSHOT_HISTORY, OUTPUT and EVENT frames
  (OUTPUT and EVENT including their 8-byte `end`).
- The holder keeps `sent - credited <= window` per stream. Every stream
  starts with 64 KiB of credit. Frames that don't fit wait in the holder's
  per-stream queue; that queue is what the lag threshold measures.
- The client sends CREDIT(n) for counted bytes it has received and buffered
  (not necessarily applied), at least every 16 KiB or 50 ms while data
  flows. Credit never waits on rendering, so a snapshot larger than the
  window always completes.
- The holder sizes the window from what credit shows the link carrying:
  about twice the bytes credited per credit round trip, using the minimum
  round trip of the last 10 s (a slow client's own queueing stretches later
  round trips, and sizing from those would grow the queue that causes it),
  clamped to 16 KiB..4 MiB. Output frames start at 4 KiB and grow to 64 KiB only while
  credit keeps up within one round trip.
- RESPONSE and INPUT_ACK are not counted and jump ahead of queued data.
  SNAPSHOT_BEGIN/READY/END and STREAM_END are not counted and never wait for
  credit themselves, but keep their place in stream order (READY after all
  of its DATA, STREAM_END after the final output). After READY, OUTPUT and
  EVENT go ahead of the remaining SNAPSHOT_HISTORY.
- The first snapshot of an attach sends its SNAPSHOT_DATA (the bytes
  through READY) at once, uncounted: it is bounded by the screen size, and
  the first screen must not wait on credit round trips. History, output and
  the screen snapshots of catch-up are counted.
- One OUTPUT frame may carry several consecutive output records; its `end`
  is the end of the last one. Positions are still record boundaries, so a
  client never derives a start position from `end` and the payload length.

## Input

INPUT payloads:

```json
{"seq":1,"type":"key","key":"KeyA","mods":["ctrl"],"text":"a","action":"press"}
{"seq":2,"type":"paste","text":"echo hi\n"}
{"seq":3,"type":"text","text":"ls\r"}
{"seq":4,"type":"focus","focused":true}
```

- `key` names are W3C `KeyboardEvent.code` values. `mods` is any of `shift`,
  `ctrl`, `alt`, `super`. `action` is `press` (default), `repeat` or
  `release`. `text` is the text the key produces, if any.
- The holder encodes key, paste and focus events with libghostty-vt for the
  terminal's current modes. `text` is written as-is.
- `seq` starts at 1 per `client_id` and increases by one. The holder applies
  events in order, drops any seq it has already applied, and acks the highest
  applied seq with INPUT_ACK. A client resends everything above the last ack
  after a reconnect.
- Only the owner's input is applied. The holder acks a non-owner's events
  without applying them, so the client never resends them.
- Pending input is capped at 64 KiB per session. Past that the holder parks
  further INPUT frames, unapplied and unacked, and keeps reading the
  connection (CREDIT and requests must still flow). A client keeps at most
  512 KiB of input payload unacked. Past 1 MiB parked, the holder acks and
  drops events in order without applying them, so an oversized paste is
  lost once instead of being resent forever.
- A key or paste event that fails to encode (unknown key or modifier) is
  acked and dropped.

## Owner

Each session has at most one owner, a `client_id`. The PTY takes the owner's
size. `session.own` takes ownership. Ownership lapses 30 s after the owner's
last stream closes.

## Events

EVENT JSON, by `kind`:

| kind | Fields |
| --- | --- |
| `resize` | `cols`, `rows`, `width`?, `height`? |
| `bell` | |
| `clipboard` | `data` (base64), only to the owner |
| `title` | `title` |
| `status` | OSC 7501 record: `state`, `id`?, `app`?, `msg`? |
| `owner` | `owner` |
| `exit` | `code` |
| `input_mode` | `echo`, `canonical`: the PTY's ECHO and ICANON flags, sent at attach and whenever they change |

Events are log records, so they are ordered exactly with output. Clients fire
effects (`bell`, `clipboard`) only for records received live, never during a
snapshot or a resume catch-up.

## Idle sessions

A holder ends its session after `idle` time (default 24 h, `0` disables)
with no attached stream and no PTY output, as if killed. A program still
printing is never idle.

## Echo prediction (client behavior)

Prediction is local to the client and never changes the protocol.

- **When:** predict while the latest `input_mode` has `echo` true or
  `canonical` false (line editors such as bash, zsh and fish turn both off
  and echo themselves), and never while `echo` is false and `canonical` true
  (password prompts), or on the alternate screen.
- **What:** printable characters (including wide ones), backspace, left and
  right, at the predicted cursor, drawn as a marked overlay that never
  touches the mirror.
- **Epochs (as mosh):** Enter, control keys and misses start a new epoch.
  Within an epoch the client predicts silently and draws nothing until one
  prediction from that epoch is confirmed by the mirror. A secret that is
  never echoed is therefore never drawn.
- **Confirm or drop:** a prediction is confirmed when the mirror shows that
  character at that cell, and dropped, with a repaint from the mirror, when
  the mirror disagrees or nothing confirms it within the timeout.
- **Show only when it matters:** predictions are drawn only while the
  measured round trip is at least 30 ms, and stop after repeated misses
  until the next confirmation.
- The holder sends `input_mode` before the output that follows a termios
  change, so `echo: false` precedes a password prompt in log order.

## Replies

The holder's terminal answers every terminal query (DSR, DA, DECRQM, kitty
keyboard, size reports, Kitty graphics) by writing to the PTY, whether or
not a client is attached. Client terminals must not reply.

## Images

The holder's terminal keeps Kitty graphics images, up to 64 MiB per screen,
and decodes PNG. Images, their placements and their pixels are part of a
snapshot; a screen snapshot carries images and placements but not pixels,
which come after READY. A mirror applying `resize` events takes the same
cell size, so images move the cursor there as they do on the holder.

Only images sent in the data itself (`t=d`) are kept: the holder never reads
files or shared memory a program names, which the program would have to
fall back from. A client that draws images on another terminal passes the
holder's graphics commands through with `q=2`, since the holder already
replied, and draws placements again from its mirror after redrawing from a
snapshot.

## Clipboard

OSC 52 writes become `clipboard` events, sent only to the owner; a client
puts them on its own clipboard. Clipboard reads are not answered.

## Channel streams

A channel stream carries a byte stream that is not a terminal: a TCP
connection the server opens for the client (`forward`), or a process
without a PTY (`exec`). The server handles channel streams itself; no
holder is involved. They share the connection's stream ids and the 64
stream limit with session streams.

| Method | Arguments | Result |
| --- | --- | --- |
| `forward.open` | `stream`, `host`, `port` | `{}` once connected |
| `exec.start` | `stream`, `argv`, `cwd`?, `env`? | `{}` once started |
| `stream.close` | `stream` | `{}`, written after the STREAM_END (see below for QUIC) |

- **forward.open** connects from the server to `host:port`, as `ssh -L`
  does; `localhost` reaches the server's own loopback. The connection is
  made as the server's user. Failure to connect within 10 s, or a refusal,
  is `unreachable`. The connection's bytes arrive as CHANNEL_DATA on channel
  1. When the remote end stops sending, the server sends CHANNEL_EOF 1; when
  both directions are done, STREAM_END `closed`.
- **exec.start** runs `argv` directly (no shell: send `["sh","-lc",
  "<command>"]` for one). A bare `argv[0]` is looked up in the `PATH` the
  process gets (from `env` if it sets one, otherwise the server's),
  skipping relative entries. It runs in its own process group, in `cwd` (default the
  home directory), with the server's environment plus `env`. stdout and
  stderr arrive as CHANNEL_DATA on channels 1 and 2, each followed by its
  CHANNEL_EOF; then STREAM_END `exited` with `exit_code` (128 + the signal
  for a signal). A missing program is `not_found`.
- **DATA and EOF** go from client to server: bytes to write to the
  connection or stdin, and the half-close (`CloseWrite`, or closing stdin).
- **stream.close** ends a stream early: the server closes the connection,
  or sends SIGHUP to the process group and SIGKILL 2 s later. The STREAM_END
  is `closed` for a forward and `exited` or `killed` for a process. The
  server writes the STREAM_END before the `{}`. Over a WebSocket they
  arrive in that order. Over QUIC they may not: the STREAM_END and the
  channel's last data travel on the channel's own QUIC stream (a lane,
  [QUIC.md](QUIC.md)) and the RESPONSE on the main stream, and QUIC
  orders nothing across streams. So a client keeps a channel's data, its
  exit code included, until the STREAM_END, and doesn't take the
  RESPONSE as the channel's end.
- **Flow control.** The client returns CREDIT for CHANNEL_DATA bytes it
  has consumed (payload bytes, the channel byte included), at least every
  8 KiB or 50 ms while it has unreturned bytes; the server returns GRANT
  for DATA bytes it has written to the connection or stdin, on the same
  cadence. Each side starts with 4 MiB of the other's credit and sends
  only within it: a stream carries at most one window per round trip, so
  the window is sized for fast links far away (34 MB/s at 120 ms), and
  the in-flight window below, not credit, keeps queues short. A sender reads each source (the socket, each pipe) at
  most one frame ahead of what it may send, so a slow reader slows the
  writer instead of growing a queue, and one idle source never holds
  credit another needs. DATA and CHANNEL_DATA carry at most 32 KiB each.
- **In flight.** Credit bounds buffering, not latency: on a slow link
  a window of credit is seconds of queue, and once bytes are in the TCP send buffer
  nothing can pass them. So the server also keeps each channel stream's
  uncredited CHANNEL_DATA near one bandwidth-delay product while someone
  types on the connection (a session INPUT, or DATA of at most 256 bytes
  on an exec stream, a control-mode client's keys, within the last 2 s):
  the bytes credited per round trip times the 10th percentile of the last 10 s's credit round trips (but at most 20 ms
  above their minimum) plus 40 ms, clamped to 16 KiB..4 MiB, so a
  keystroke's echo waits behind about 40 ms of it. The percentile keeps a
  path's jitter (a tailnet, Wi-Fi) from counting as queue and starving the
  stream; the cap keeps the stream's own queue from raising its base.
  With nobody typing it allows 200 ms instead: a bulk transfer fills a
  path that queues little at full speed (most do), and on one that would
  queue seconds (a deep buffer before a slow link) the next keystroke
  waits behind at most that. The first keystroke tightens the bound at
  once. The same bound also holds for all of a connection's channel
  streams together, sized from their combined credit, so that many
  streams (each with a 16 KiB floor) don't add up to seconds of queue; a
  stream that ends gives back what it had in flight. It keeps little unsent
  data in the kernel (TCP_NOTSENT_LOWAT, 16 KiB, where the platform has
  it), so the write priority below decides what goes next. Clients do the
  same for DATA (2 x the bytes granted per round trip), and send INPUT
  before DATA. A bound held all the time costs bulk throughput: 120 ms
  away, a forward got half of ssh's at 1.25 x and three quarters at 2 x;
  left open, it matched ssh but let a download fill a deep buffer, and a
  keystroke typed next waited a second behind it. On Linux the server also asks for
  BBR congestion control, where the kernel allows it.
- **Priority.** The server writes in order of class: replies and stream
  control (RESPONSE, INPUT_ACK, GRANT, STREAM_END), then session streams,
  then exec streams, then forwards, round-robin within a class. A bulk
  forward therefore never delays a keystroke's echo by more than one frame.
  So that a terminal printing without pause can't starve them (an app's
  gateway requests ride a forward), a lower class that has data waiting
  gets the next frame after 64 KiB of higher-class data, whichever class
  is next in line.
- **Lifetime.** Channel streams belong to their connection. When it drops,
  the server closes the forwarded connections and ends the processes as for
  stream.close. They don't resume: a client opens new ones after it
  reconnects.

## Auth

Each device has an Ed25519 key. A device enrolls once by running
`osewa enroll` on the host over SSH, writing `{"name","key"}` (key is base64
raw public key) as one JSON line to stdin. The host appends it to
`~/.config/osewa/devices` (mode 0600) and prints
`{"server_key","port","version"}`.

Connections use TLS 1.3. Both sides present self-signed certificates over
their Ed25519 keys. The server accepts a client only if its key is in
`devices`; the client accepts the server only if its key matches the one from
enrollment. The WebSocket path is `/osewa`, subprotocol `osewa.v0`.

The server compresses what it sends with WebSocket permessage-deflate,
keeping the compression context across messages (snapshots and text output
shrink about 6-8x). Clients send uncompressed: their frames carry
keystrokes, and compressing secrets next to attacker-influenced data is how
compression side channels work. Server-to-client compression has the same
trade-off as SSH or mosh compression; `osewa server --no-compress` turns it
off.
