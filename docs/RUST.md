# osewa in Rust: handoff

A brief for the agent (and its subagents) porting osewa from Go to Rust.
Written 2026-10-30 against `main` at `45628a7`. Read this first, then
[PROTOCOL.md](PROTOCOL.md), [QUIC.md](QUIC.md), [STATUS.md](STATUS.md) and
`bench/RESULTS.md`.

## Goal

The same product, the same wire protocol (v0), in Rust, measurably better
where the Go version is weak (host CPU per keystroke and per byte of
output, memory per session, QUIC congestion control) and no worse anywhere
else. The Go implementation stays the reference until the Rust one beats
it on the targets below; nothing is deleted before that.

Why Rust (what the Go version showed, not a guess):

- quic-go has no congestion-control settings, and its HyStart ends slow
  start early on jittery paths: 2 of 6 fresh connections to a host 122 ms
  away stalled at 0.9-3.2 MB/s against 9 MB/s (QUIC.md, "Known
  weakness"). quinn takes a pluggable congestion controller.
- The holder, one process per session, runs Go's GC every ~60 ms under a
  flood, and costs 2-4x mosh's CPU per keystroke and per byte at low
  output rates (targets below).
- A smaller client library for iOS/Android than a Go xcframework.

## Rules (from the user; not negotiable)

- **Never cheat on benchmarks.** Same links, same workloads, same harness
  for every side; report losses as plainly as wins. A Rust number counts
  only from the same e2e rig the Go number came from.
- **Fast loops while iterating.** Quick tiers (`make bench-vs-quick
  LINK=<substring>`, `OSEWA_RES_QUICK=1`, `-run <Test>`); full suites
  (~20 min) only before quoting numbers or committing results.
- No backwards-compatibility shims unless stored data depends on it (it
  does for `~/.config/osewa` and the state dir: keep their formats).
- libghostty is pinned to upstream Ghostty, not a fork (`ghostty/pin`).
- Commits end with the Co-Authored-By trailer the session gives you.
- `secrets/` in the monorepo is SOPS; never print decrypted values.

## The contract a Rust build must keep

The e2e suite is black-box: it builds a Docker image with `osewa` and
`osewa-bench` in `/usr/local/bin`, runs them, and talks to them over the
wire. Keep these and every e2e test and benchmark works unchanged:

1. **Wire protocol v0**, [PROTOCOL.md](PROTOCOL.md): frames, sessions,
   output log, attach/resume, catch-up, flow control and in-flight bounds,
   input, owner, events, channel streams, auth. Where the doc and the Go
   code disagree, the code is right and the doc gets fixed.
2. **Holder sockets** (PROTOCOL.md "Processes", "Holder sockets"): the
   server and holder are separate processes speaking the same frames over
   `<run>/<id>.sock`. This is the seam that lets the port go one process
   at a time.
3. **QUIC carrier**, [QUIC.md](QUIC.md): ALPN `osewa.v0+deflate` /
   `osewa.v0`, the main stream (length-prefixed frames, zero-length ping),
   lanes (one unidirectional stream per channel each way), WELCOME
   `carriers`, the race (WebSocket 300 ms behind QUIC), the 1200-byte
   initial packet.
4. **CLI surface** that e2e and users run: `osewa server|service|enroll|
   holder|login|ls|new|attach|kill|exec|update|forward|version` with the
   flags in `osewa` (no args) usage; `osewa-bench drive|gen|serve|bytes|
   resume`.
5. **On-disk state**: `~/.config/osewa/{client.key,hosts.json,devices...}`
   (auth/, client/hosts), the state dir (0700; internal/paths), launchd/
   systemd units (install/, cmd/osewa/service.go).
6. **libghostty-vt snapshots** (ghostty/): the snapshot format the
   holder sends and clients decode is libghostty's; call the same C API.

The Go client library is also imported by `app-hook` (4 files, in
`wt-osewa/app-hook`, branch `osewa-transport`; `internal/hostproxy`) and
built into the iOS framework (`mobile/`, `make ios`). Those keep using Go
until a Rust client exists with an FFI they can call; do not break them.

## Where the Go code is (non-test lines)

| Package | Lines | What |
| --- | ---: | --- |
| client (+mirror, login, hosts) | ~9.7k | connection, resume, channels, exec/forward, mirror screen, login/enroll |
| holder | ~6.5k* | PTY, child, terminal, output log, queue/catch-up, input, owner |
| server | ~5.5k* | TLS, device auth, holder spawn/relay, channels, flight control |
| cmd/osewa | ~5k* | CLI, attach UI, prediction, service, update |
| ghostty | 1.4k | cgo wrapper of libghostty-vt |
| auth, wire, internal/carrier, internal/execproc, install, paths | ~3.7k | keys, frames, carriers, exec, install |
| mobile | 1.1k | gomobile surface for iOS |

(*including tests in the count; whole repo ~17k non-test, ~22k test.)

## Port order

Each step ships behind the same e2e rig and must pass the step's gate
before the next starts. Mixed deployments are the test: every Rust piece
must interoperate with the Go pieces around it.

1. **Holder** (`osewa holder`). Highest payoff, smallest seam: a Rust
   holder spawned by the Go server. PTY (rustix/nix), libghostty-vt via
   bindgen against the same pinned build, output log, queue and catch-up,
   input, owner, snapshots. Gate: `go test ./holder/...` behaviors
   reproduced as Rust tests, the whole e2e suite green with the Go server
   spawning the Rust holder, `make bench-res` and `make bench-mosh`
   better than Go on the holder targets below.
2. **Server** (`osewa server`, `enroll`, `service`): rustls, tokio,
   WebSocket (tokio-tungstenite or fastwebsockets), flight control
   (server/flight.go: per-channel and per-connection windows, typing cap,
   jitter-tolerant base; read the comments, they record why), TCP
   `NOTSENT_LOWAT` 16 KiB, BBR on Linux when available. Gate: e2e green
   with Go clients against the Rust server; `make bench-vs` no worse.
3. **QUIC carrier** on quinn: lanes, deflate main stream, race. Choose
   the congestion controller deliberately (BBR or a Cubic without
   HyStart's early exit) and prove it on the real path (below). Gate:
   QUIC targets.
4. **Client library + CLI** (`attach`, `exec`, `forward`, `login`,
   `update`, prediction). Gate: e2e green with the Rust client against
   both servers.
5. **FFI** for app-hook (C ABI staticlib via cgo) and mobile (uniffi or
   C ABI). Only after 1-4; the user decides when app-hook switches.

## Working with subagents

Run one lead agent that owns the plan and the benchmarks, and subagents
per step in **separate git worktrees** (`isolation: "worktree"`), each
with a narrow brief: the step, the files to read, the gate. Good splits:

- holder core (PTY, log, queue) / libghostty bindings + snapshot tests /
  input + owner + events, merged by the lead;
- server relay + auth / flight control + socket tuning / QUIC carrier;
- a reviewer subagent that is given the code and PROTOCOL.md, not the
  author's conclusions, for each merge.

The lead alone runs full benchmarks (they need the whole machine; two at
once corrupt each other's numbers) and writes results.

## How to measure

Docker (OrbStack on the user's Mac):

    export DOCKER_HOST=unix:///Users/jyo/.orbstack/run/docker.sock
    export DOCKER_CONFIG=<scratch>/dockercfg   # with cli-plugins -> ~/.docker/cli-plugins

| Command | What | Time |
| --- | --- | --- |
| `make bench-res` (`OSEWA_RES_QUICK=1`, `OSEWA_RES_ONLY=`, `OSEWA_RES_TOOLS=`) | host/client CPU and RSS: idle, typing, log tail, flood | ~9 min (quick ~4) |
| `make bench-vs` / `make bench-vs-quick LINK=fiber` | osewa (WS, QUIC) vs ssh over 4 shaped links | ~20 min / ~1.5 |
| `make bench-mosh` | screen freshness, typing, recovery, reattach vs mosh | ~15 min |
| `OSEWA_E2E_MOSH=1 go test -tags e2e -run TestNetworkChangeRecovery ./e2e/` | recovery after a network change | ~5 min |
| `make bench` | terminal write throughput, snapshot encode/decode | ~1 min |
| `go run ./bench/cmd/vsssh [-quic] [-quick] mini-jp` | the real path, 122 ms to Japan | ~5 min |

To run e2e against a Rust build: build an image whose `/usr/local/bin`
has the Rust `osewa` (and, for a holder-only step, the Go `osewa` with the
Rust holder at the path the server spawns), then `OSEWA_E2E_IMAGE=<tag>`.
Keep a Go image beside it and run both in the same session for every
comparison.

Real host `mini-jp` (Mac mini in Japan, Tailscale 100.124.71.13, fish
login shell: run `sh -c '...'`). Its server runs from
`~/.local/bin/osewa` under launchd `dev.osewa.server`; `osewa update
mini-jp` replaces it over osewa itself. ssh to it needs the user's
1Password agent unlocked; ask when it fails. Never run `service install`
through an osewa exec on it: the bootout kills the exec before it can
re-bootstrap, and the server stays down until someone logs in.

## Lessons the Go version paid for (keep them)

- **Dial on the link you measure.** A connection established before
  netem shaping carries an unshaped minimum RTT; QUIC slow start then
  stops at once. The scoreboard now dials per link.
- **PTY reads are small** (1-4 KiB). Don't keep slices of a big read
  buffer: the Go holder pinned 32 KiB per chunk (75 MB live under a
  flood) until `ff0093f`.
- **Credit isn't latency.** Bound in-flight data to rate x (base RTT +
  40 ms) while someone types, 200 ms otherwise; base is the 10th
  percentile of 10 s of RTTs capped at min + 20 ms (jitter tolerance);
  the first keystroke tightens at once; only small DATA (<= 256 bytes)
  counts as typing; one budget per connection over all channels.
- **Echo shares a pipe with bulk.** Over TCP, keep the socket's unsent
  queue small (`TCP_NOTSENT_LOWAT` 16 KiB). Over QUIC, put bulk on lanes
  and keep the main stream for terminals; a writer lock shared by lanes
  and the main stream lets a blocked lane write delay an echo.
- **Credit returns every 8 KiB** of consumption (or after a short delay),
  or slow credit inflates RTT samples.
- **Variance on the real path is high.** Quote medians of several runs;
  fresh connections differ.

## Benchmark targets

Go baselines are the current numbers from the rigs above (2026-10-30,
after `ff0093f`). "Must" gates a step; "aim" is what makes the port worth
it. Lower is better unless noted. All from the same rig and session as a
Go run.

### Resource use (`make bench-res`; 10 Mbit/s, 100 ms RTT; one terminal)

| Workload | Measure | Go osewa | mosh | ssh | Rust must | Rust aim |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| idle | host CPU | <0.1% | <0.1% | <0.1% | <0.1% | <0.1% |
| idle | host RSS (server + holder) | 31 MB | 26 MB | 37 MB | <= 20 MB | <= 10 MB |
| idle | client CPU / RSS | 0.2% / 15 MB | <0.1% / 14 MB | <0.1% / 8 MB | <0.1% / <= 10 MB | <0.1% / <= 6 MB |
| typing, 10 keys/s | host CPU | 2.1% | 0.6% | <0.1% | <= 1.0% | <= mosh |
| typing | client CPU | 2.2% | 0.6% | 0.1% | <= 1.0% | <= mosh |
| log tail, 50 kB/s | host CPU per MB written | 830 ms | 514 ms | 92 ms | <= 514 ms | <= 250 ms |
| log tail | host RSS | 48 MB | 33 MB | 39 MB | <= 33 MB | <= 25 MB |
| flood (W2) | host CPU per MB written | 8 ms | 48 ms | 13 ms | <= 8 ms | <= 5 ms |
| flood | host RSS | 72 MB | 33 MB | 40 MB | <= 40 MB | <= 30 MB |
| flood | client RSS | 23 MB | 14 MB | 9 MB | <= 15 MB | <= 10 MB |

A flood's absolute CPU is ~one core for osewa and mosh by design (they
drain the PTY at once and send screens); compare per MB written. Add the
holder count dimension once the Rust holder exists: RSS per extra idle
session (100 sessions), which the Go version has not measured yet.

### Freshness and typing vs mosh (`make bench-mosh`)

| Measure (no loss / 1% loss) | Go osewa | mosh | Rust must | Rust aim |
| --- | ---: | ---: | ---: | ---: |
| keystroke to glyph p95 | 2.4 / 2.1 ms | 3.2 / 2.7 ms | <= Go | <= Go |
| screen age during W2 flood p50 | 133 / 163 ms | 78 / 78 ms | <= Go | <= mosh |
| screen age during W2 flood p95 | 206 / 312 ms | 101 / 101 ms | <= Go | <= 1.2x mosh |
| screen age, W1/W4 flood p50 | 131/129, 144/140 ms | 78 ms | <= Go | <= mosh |
| recovery after network change | 0.79 / 0.96 s (WS) | 1.09 / 1.13 s | <= Go | <= 0.6 s |
| reattach from a new process | 0.42 / 0.39 s | not possible | <= Go | <= 0.3 s |
| bytes/min, W2 at 50 kB/s | 1.27 MB | 1.12-1.18 MB | <= Go | <= mosh |

Screen age during a flood is the Go version's clearest loss to mosh;
it is a holder/catch-up behavior, so it belongs to step 1.

### osewa vs ssh (`make bench-vs`, four shaped links)

Must: no cell worse than the Go osewa of the same run beyond noise (10%,
or 5 ms for latencies); every echo-under-download cell still beats ssh.
The Go numbers are in `bench/RESULTS.md` "osewa vs ssh (Docker)"; the
run that replaces the pre-`45628a7` table (whose QUIC fiber download was
the dial-before-shaping artifact) is in progress at handoff. Highlights
of the last full run (WS / QUIC vs ssh):

| Link | Connect + run | Echo during download p50 | Download alone |
| --- | ---: | ---: | ---: |
| far fiber, 120 ms, 50 Mbit/s | 643 / 381 vs 1577 ms | 187 / 136 vs 332 ms | 5.34 / 5.94* vs 5.97 MB/s |
| cellular, 60 ms, deep buffer | 329 / 207 vs 946 ms | 131 / 135 vs 1714 ms | 1.18 / 1.19 vs 1.20 MB/s |
| busy Wi-Fi, 1% loss | 125 / 81 vs 471 ms | 68 / 56 vs 2235 ms | 0.94 / 1.44 vs 0.93 MB/s |
| slow 1 Mbit/s, deep buffer | 861 / 489 vs 1435 ms | 193 / 143 vs >15000 ms | tie |

(*quick run after the dial fix.) Aim: QUIC download >= 95% of ssh on
every link; Wi-Fi echo p95 under download within 2x idle (terminals on
their own lanes).

### Real path (`vsssh -quic mini-jp`, 122 ms)

| Measure | Go osewa WS / QUIC | ssh | Rust must | Rust aim |
| --- | ---: | ---: | ---: | ---: |
| connect + run p50 | 664 / 401 ms | 1614 ms | <= Go | QUIC <= 1.5 RTT + exec (~300 ms) |
| download alone | 10.05 / 1.33-11.6 MB/s | 10.25 MB/s | QUIC >= 90% of ssh in 6 of 6 fresh connections | >= ssh |
| echo under download p50 | 170 / 136 ms | 193 ms | <= Go | <= 1.1x idle |

The QUIC row is the one quinn's congestion control has to fix: run six
fresh connections and report all six, not the best.

### Throughput floor (`make bench`)

Terminal write throughput and snapshot encode/decode are libghostty's;
the Rust holder must stay within 5% of the Go numbers in
`bench/RESULTS.md` (W1 1346 MB/s, W2 298 MB/s at 200x60; R1 snapshot
encode 2.1 ms, READY 387 us), i.e. its own overhead around libghostty
must be negligible.

## Open decisions (ask the user)

- Whether to patch quic-go meanwhile (a fork with HyStart off) so the Go
  version's QUIC isn't flaky while the port runs.
- When app-hook and the iOS app switch to a Rust client (step 5).
- rjyo/osewa is private; the home page links it. Making it public, the
  `security@` contact in SECURITY.md, and removing the 18 MB binary from
  history (`8b192fa`) are still pending.
