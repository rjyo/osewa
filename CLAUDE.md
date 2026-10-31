# osewa

Persistent remote terminals over QUIC and TLS WebSocket, built on
libghostty-vt. Wire protocol v0: `docs/PROTOCOL.md` is the wire contract,
`docs/QUIC.md` the QUIC carrier.

## Layout

| Path | What |
| --- | --- |
| `crates/wire/` | Frames and messages. The only place wire types are defined |
| `crates/ghostty/` | Binding to libghostty-vt (C bridge + safe wrapper) |
| `crates/holder/` | One process per session: PTY, terminal, output log, queue, input, owner |
| `crates/server/` | TLS + WebSocket and QUIC server: holder spawning and relaying, channel streams, flight control |
| `crates/carrier/` | Carriers: WebSocket (permessage-deflate) and QUIC (quinn) |
| `crates/auth/` | Device keys, the devices file, pinned mutual TLS, enrollment |
| `crates/execproc/` | Processes without a PTY for exec.start |
| `crates/client/` | Client library (no libghostty): connect, attach, resume, channels, Link, hosts.json, install, login |
| `crates/mirror/` | libghostty mirror terminal (a client Renderer) and echo prediction |
| `crates/flag/`, `crates/paths/` | Command-line flags and durations; per-user directories |
| `crates/bench/` | `osewa-bench`, the e2e benchmarks' helper |
| `crates/osewa/` | The single `osewa` binary |

## Rules

- Gate for every change: `make check` (clippy `-D warnings` + `cargo test`).
- libghostty-vt comes from `make libghostty` (pinned commit in
  `scripts/libghostty.sh`, mirrored in `osewa_ghostty::COMMIT`), into
  `.cache/libghostty/<os>-<arch>`. Never vendor or edit libghostty.
- Every crate has tests.
- Never cheat on benchmarks: same rig, same session for every side of a
  comparison.
- No backwards-compatibility shims before v1, except for on-disk state.
- Comments explain why, not what.
