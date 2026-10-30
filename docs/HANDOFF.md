# Handoff: polishing the Rust port

For the next lead agent. Written 2026-10-31 at `main` 22a6e56. Read
this, then [RUST.md](RUST.md) (the binding brief: rules, contract,
targets), [STATUS.md](STATUS.md) (what exists, the scoreboard, raw
outputs in `bench/2026-10-31/`), [PROTOCOL.md](PROTOCOL.md) and
[QUIC.md](QUIC.md).

## Start prompt

Paste in a fresh session in `/Users/jyo/projects/ai/moshi/osewa-rs`:

> Keep polishing the Rust port of osewa. Read docs/HANDOFF.md first, then
> docs/RUST.md (binding rules and targets) and docs/STATUS.md (scoreboard).
> Work the "Open, by priority" list top down. Every fix: a test that
> fails without it, `make check` green, a commit with the trailer, and the
> affected benchmark rerun against the Go image in the same session on a
> quiet machine. Report every target as pass or fail with numbers, losses
> included.

## Where things are

- Port: `osewa-rs` (this repo), 13 crates, 607 tests, steps 1-4 of
  RUST.md done. Step 5 (FFI for app-hook/iOS) waits for the user.
- Go reference: `../osewa` (read-only for us; another session may be
  benchmarking there, don't touch its working tree).
- Go seam for mixed deployments: worktree `../wt-osewa/rust-seam`,
  branch `rust-holder-seam` (0e9aae2): the Go server honors
  `OSEWA_HOLDER`. Its `.cache/libghostty` is a symlink to `../osewa`'s.
  Run the Go e2e harness from there.
- Old subagent worktrees under `../osewa-rs-wt/` (auth, carrier,
  holder-tests, server, quic, client) are all merged; remove them with
  `git worktree remove` or reuse them for new branches.

## Gate and loops

- `make check`: clippy `-D warnings` + `cargo test` (~3-4 min). Tests
  that need Go (`go` on PATH and `../osewa`) build Go helpers and skip
  without them.
- Subagent worktrees have no `.cache`: build with
  `OSEWA_LIBGHOSTTY=/Users/jyo/projects/ai/moshi/osewa-rs/.cache/libghostty/darwin-arm64`.
- Load-sensitive tests: `holder tests::catchup::input_triggered_screen`
  and `osewa go_client::go_cli_against_rust_server` each failed once
  under full-machine load and pass alone. Treat a single failure while
  other builds run as load; on a quiet machine, as a bug.

## The rig

Docker is OrbStack:

    export DOCKER_HOST=unix:///Users/jyo/.orbstack/run/docker.sock
    export DOCKER_CONFIG=<scratch>/dockercfg   # cli-plugins -> symlinks to ~/.docker/cli-plugins/*

Images (build in this repo; `GO_IMAGE=osewa-go-seam` is the Go image
built from the seam worktree with `docker build -t osewa-go-seam .`):

| Target | Contents | Use |
| --- | --- | --- |
| `mixed` | Go image + Rust osewa as holder (`OSEWA_HOLDER`) | step-1 interop |
| `rust` | Go image with the Rust `osewa`; Go's `osewa-bench` | **all benchmarks** (same harness as Go) |
| `rust-all` | `rust` + Rust `osewa-bench` | functional e2e of everything Rust |

    docker build --build-arg GO_IMAGE=osewa-go-seam --target rust -t osewa-rust .
    cd ../wt-osewa/rust-seam
    OSEWA_E2E_IMAGE=osewa-rust go test -tags e2e -count=1 -timeout 25m -v ./e2e/...

Benchmarks, always Go image then Rust image back to back, nothing else
running (no subagents compiling: that corrupted numbers twice):

| What | Env | Test | Time per image |
| --- | --- | --- | --- |
| resources | `OSEWA_E2E_RES=1` (`OSEWA_RES_QUICK=1`) | `TestResourceUse` | ~7.5 min |
| vs mosh | `OSEWA_E2E_MOSH=1` | `TestMoshHeadToHead` | ~13.5 min |
| vs ssh | `OSEWA_E2E_VS=1` (`OSEWA_VS_QUICK=1 OSEWA_VS_ONLY=<link>`) | `TestVsSSH` | ~20 min |
| echo under flood | none | `TestM2EchoUnderFlood` | ~1 min |

They write `bench/RESULTS.md` / `bench/MOSH.md` in the seam worktree:
copy them out per image, then `git checkout bench/` there. Quick tiers
while iterating; full suites only before quoting numbers.

## The real path (mini-jp, 122 ms)

Never replace the Go server there (launchd `dev.osewa.server`, the
user's live sessions). Run the Rust server beside it:

    cargo build --release -p osewa
    scp target/release/osewa mini-jp:.local/bin/osewa-rs
    ssh mini-jp "sh -c 'OSEWA_RUN_DIR=\$HOME/.local/state/osewa/run-rs nohup \$HOME/.local/bin/osewa-rs server --listen :7323 > /tmp/osewa-rs.log 2>&1 < /dev/null &'"

Point a copy of `~/.config/osewa` (client.key, hosts.json with mini-jp's
`addr` and `quic_port` set to 7323) at it with `OSEWA_CONFIG_DIR`, and
run Go's `vsssh -quic mini-jp` alternately with and without it. Fresh
QUIC downloads: `OSEWA_CARRIER=quic osewa exec mini-jp -- sh -c 'head -c 50000000 /dev/zero'`,
six per server plus six ssh, all reported.

Cleanup, in this order: kill the Rust holders (`pgrep -fl osewa-rs`),
then the server, then remove `run-rs`, the log and the binary. Holders
outlive the server by design; deleting the binary first orphans them.
Go's vsssh never kills its sessions: each run leaves two `cat` holders
(80x24) on whichever server it used. Kill them with `osewa kill`.
ssh to mini-jp needs the user's 1Password agent; fish is the login
shell, so wrap commands in `sh -c '...'`.

## Open, by priority

1. **WebSocket echo under download** (biggest loss). Real path: Rust
   298-315 ms p50 vs Go 187-190 (and ssh 174-196); Docker slow link:
   151 vs 108 ms. QUIC is fine (tie). Rust's WS download "meanwhile"
   runs higher, so more bulk sits ahead of the keystroke.
   `TCP_NOTSENT_LOWAT` is set on macOS and Linux (crates/server/src/lowat.rs).
   Suspects, in order: buffering below the send queue on the TCP path
   (the carrier's out-buffer and rustls's 64 KiB send buffer via
   tokio-rustls both accept bytes the kernel refused; Go's coder/websocket
   writes straight through), so the SendQueue's priority no longer decides
   what goes next; then exec flight's typing detection and `tighten`
   (crates/server/src/flight.rs, channel.rs) against flight.go. Start
   with `OSEWA_VS_QUICK=1 OSEWA_VS_ONLY=slow` on both images, then
   OSEWA_TRACE / `ss -ti` sampling (e2e `TestEchoBreakdown`,
   `OSEWA_TRACE=1`, is the existing breakdown for W1).
2. **W2 echo-under-flood tail**: p95 553 ms-1.17 s vs Go 350-409
   (p50 close). The holder's catch-up path under the heaviest
   workload. A single-thread design difference already bit once (PTY
   reads starved keystrokes; fixed by yielding per read, 4e93bf5). Look
   at where the holder spends a flood: snapshot encode inline on the one
   thread, writer wakeups, timer lateness. Compare with traces from the
   Go holder.
3. **Client CPU**: typing 1.1% (must <= 1.0%), idle 0.1% (must <0.1%).
   The client runs a network thread plus an applier thread per
   connection (crates/client docs); profile the idle wakeups (keepalive
   timers, credit/ack timers) and per-keystroke work (mirror apply,
   prediction, render).
4. **Reattach +15-21 ms, keystroke-to-glyph p95 +0.3-1.2 ms** (bench-mosh).
   Re-measure over three runs before chasing; may be noise.
5. **Aims not met** (RUST.md): idle host RSS <= 10 MB (14.2), log-tail
   host CPU <= 250 ms/MB (368), flood <= 5 ms/MB (6), flood host RSS <=
   30 MB (36.9), typing host CPU <= mosh (0.9 vs 0.7%). Screen age vs
   mosh (~55 ms behind) needs a catch-up design change, not a port fix:
   propose it to the user before building it.
6. **Not measured yet**: `make bench` throughput floor (terminal write
   and snapshot encode within 5% of Go: port bench/'s Go benchmarks as
   criterion or a plain timing test), RSS per extra idle session (100
   sessions).
7. **Small differences**: env entries without `=` are dropped (std's
   Command can't pass them); osewa-bench usage text differs from Go's.

## How the port was run (worked well)

- One lead owns the plan, merges and every benchmark; subagents per
  piece in their own worktrees (`git worktree add ../osewa-rs-wt/<name>
  -b <name>`), each with a narrow brief: files to read, the gate, "no
  Docker or benchmarks".
- An independent reviewer per merge, given the code, PROTOCOL.md and
  the Go code, not the author's report. Every review found real bugs
  (holder signal dispositions, server relay end path, exec stdin, QUIC
  ordering); send findings back to the author to fix with tests.
- Interop tests against the real Go code (built from `../osewa` into a
  temp dir with `go build -o`, or a small Go helper module with a
  `replace` onto the checkout) catch what reading can't.
- The e2e rig catches what unit tests can't: TCP_NODELAY and the
  PTY-read starvation were both found there, not in review.
- Ask a subagent to make a flaky test deterministic before trusting the
  fix it guards: the first QUIC ordering fix "passed" by timing luck.
