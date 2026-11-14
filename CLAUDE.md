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
| `crates/vt/` | Draws an attached session for a local terminal: the mirror re-rendered, predictions, notices (CLI and FFI) |
| `crates/ffi/` | The C API (`include/osewa.h`) apps embed: one session as VT bytes for the app's terminal; `Osewa.xcframework` on iOS |
| `crates/flag/`, `crates/paths/` | Command-line flags and durations; per-user directories |
| `crates/bench/` | `osewa-bench`, the e2e benchmarks' helper |
| `crates/osewa/` | The single `osewa` binary |
| `site/` | osewa.dev: a static Astro site, served by Cloudflare Workers static assets (Bun) |

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

## Issues

On `rjyo/osewa`:

1. Check the reported cause against `main` before fixing.
2. One PR per issue: `Fixes #N`, a test that fails without the fix.
3. Fable 5.2 reviews each PR.
4. Merge fast-forward only.
5. Release notes link the issue.
