# osewa-rs

Rust port of osewa (`../osewa`, the Go reference). Same product, same wire
protocol v0. `docs/RUST.md` is the brief: its rules and benchmark targets
are binding. `docs/PROTOCOL.md` is the wire contract; where it and the Go
code disagree, the Go code is right and the doc gets fixed.

## Layout

| Path | What |
| --- | --- |
| `crates/wire/` | Frames and messages. The only place wire types are defined |
| `crates/ghostty/` | Binding to libghostty-vt (C bridge + safe wrapper) |
| `crates/holder/` | One process per session: PTY, terminal, output log, queue, input, owner |
| `crates/server/` | TLS + WebSocket server: holder spawning and relaying, channel streams, flight control |
| `crates/execproc/` | Processes without a PTY for exec.start |
| `crates/osewa/` | Single binary, same CLI as the Go `osewa` |

## Rules

- Gate for every change: `make check` (clippy `-D warnings` + `cargo test`).
- libghostty-vt comes from `make libghostty` (pinned commit in
  `scripts/libghostty.sh`, mirrored in `osewa_ghostty::COMMIT`), into
  `.cache/libghostty/<os>-<arch>`. Never vendor or edit libghostty.
- Every crate has tests. A Go test's behavior is ported as a Rust test.
- Interop is the contract: every Rust piece must work with the Go pieces
  around it (holder socket, wire, on-disk state).
- Never cheat on benchmarks; same rig, same session as a Go run.
- No backwards-compatibility shims before v1, except for on-disk state.
- Comments explain why, not what.
