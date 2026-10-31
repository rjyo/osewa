# Rust port: status

As of 2026-10-31. The Go osewa (`../osewa`) stays the reference until
this beats it on the targets in [RUST.md](RUST.md).

## What exists

Every Go component has a Rust port, steps 1-4 of RUST.md's port order:

| Crate | Port of | Interop evidence |
| --- | --- | --- |
| wire | wire/ | Go-identical JSON encoding (HTML escaping) and Go-lenient decoding |
| ghostty | ghostty/ | same pinned libghostty, same snapshot bytes |
| holder | holder/ | Go e2e suite with the Go server spawning Rust holders |
| auth | auth/ | Go↔Rust TLS handshakes both ways; Go-written key and devices files |
| carrier | internal/carrier | WebSocket vs coder/websocket both ways; QUIC (quinn) vs quic-go both ways |
| server, execproc | server/, internal/execproc | Go CLI and Go client library against the Rust server, WS and QUIC |
| client, mirror | client/, client/mirror, hosts, login | Rust client against the Go and the Rust server, WS, QUIC and the race |
| osewa | cmd/osewa | same usage, ls columns, unit files and exit codes as Go's |
| bench | bench/cmd/osewa-bench | byte-identical workloads and transfer pattern vs Go's |

`cargo test`: 627 tests, every Go test of these packages ported (none
ignored). Each step got an independent review against PROTOCOL.md and
the Go code; the findings are fixed (see the merge commits).

Not ported: step 5 (FFI for app-hook and the iOS framework; the user
decides when), `mobile/`.

## The e2e rig

Images (Dockerfile): `mixed` (Go server, Rust holders), `rust` (Rust
osewa, Go osewa-bench: for benchmarks, same harness as Go), `rust-all`
(also the Rust osewa-bench). The Go server needs `OSEWA_HOLDER` for
`mixed`, on branch `rust-holder-seam` of the Go repo.

Go e2e suite (`make e2e`), quiet machine: 22/22 on `mixed`; on `rust`
and `rust-all` everything passes except `TestM2EchoUnderFlood`'s W2
tail, which sometimes misses its bound (below), and Go's own
`TestLinuxUnits` (`server.TestTrace`), which failed once under load.

## Benchmarks (2026-10-31 night, same rig and session, Go image vs Rust image)

Main at 814be1a. Every suite run Go image then Rust image back to back on
a quiet machine (no builds, no subagents). Raw outputs:
`bench/2026-10-31-night/`. One run each unless noted; the slow-link and
M2 cells move 20-30 ms p50 between identical runs, so the sweeps behind
the commits (several runs each) are quoted in the commit messages.

### Resource use (`make bench-res`), WebSocket rows

| Workload | Measure | Go | Rust | Must | |
| --- | --- | ---: | ---: | ---: | --- |
| idle | host RSS | 29.9 MB | 14.3 MB | <= 20 | pass (aim <= 10 not met) |
| idle | client CPU / RSS | 0.1% / 14.4 MB | <0.1% / 6.9 MB | <0.1% / <= 10 | **pass** (was 0.1%); RSS aim <= 6 not met |
| typing | host CPU | 1.5% | 0.5% | <= 1.0% | pass; aim <= mosh (0.4% this run) not met by 0.1 |
| typing | client CPU | 1.5% | 0.6% | <= 1.0% | **pass** (was 1.1%); aim <= mosh (0.5%) not met by 0.1 |
| log tail | host CPU per MB | 710 ms | 302 ms | <= 514 | pass (aim <= 250 not met; was 368) |
| log tail | host RSS | 46.5 MB | 25.7 MB | <= 33 | pass (aim <= 25 missed by 0.7) |
| flood W2 | host CPU per MB | 8 ms | 6 ms | <= 8 | pass (aim <= 5 not met) |
| flood W2 | host RSS | 70.7 MB | 35.6 MB | <= 40 | pass (aim <= 30 not met) |
| flood W2 | client RSS | 22.6 MB | 9.2 MB | <= 15 | pass (aim met) |

### vs mosh (`make bench-mosh`), no loss / 1% loss

| Measure | Go | Rust | mosh | Must <= Go |
| --- | ---: | ---: | ---: | --- |
| keystroke to glyph p95 | 2.8 / 4.6 ms | 3.7 / 3.5 ms | 1.0 / 0.9 ms | fail without loss (+0.9), pass with |
| screen age W2 flood p50 | 132 / 152 ms | 133 / 138 ms | 78 ms | tie / pass |
| screen age W2 flood p95 | 207 / 330 ms | 205 / 229 ms | 101 ms | pass |
| screen age W1, W4 p50 | 132-140 ms | 128-134 ms | 76-78 ms | pass |
| recovery after network change | 639 / 694 ms | 396 / 423 ms | 1.09 s | pass (aim <= 0.6 s met) |
| reattach from a new process | 306 / 312 ms | 323 / 317 ms | not possible | **fail** (+17 / +5 ms) |
| bytes/min, W2 at 50 kB/s | 1.26 MB | 1.06 MB | 1.12 MB | pass (aim <= mosh **met**) |

Screen age is still ~55 ms behind mosh: catch-up sends whole screens,
mosh sends diffs (see Open).

### vs ssh (`make bench-vs`), Go client against each server, WS / QUIC

| Link | Echo under download p50/p95, WS | Go WS | Download meanwhile WS | Go |
| --- | ---: | ---: | ---: | ---: |
| far fiber, 120 ms | **131 / 163** | 185 / 240 | 5.57 | 5.92 |
| cellular, 60 ms | **73 / 90** | 132 / 188 | 1.07 | 1.18 |
| busy Wi-Fi, 1% loss | **59 / 122** | 67 / 151 | 0.92 | 1.03 |
| slow 1 Mbit/s | **125 / 191** | 163 / 251 | **0.06** | 0.10 |

QUIC echo under download within noise of Go everywhere (fiber 188 vs
184, cellular 138 vs 136, Wi-Fi 44 vs 55, slow 154 vs 205). Losses:
download meanwhile on the slow link (0.06 vs 0.10, the price of the
typing-time path bound, below); busy Wi-Fi download alone 0.87 / 0.99 vs
Go 1.05 / 1.50 MB/s (ssh 0.92 vs 0.93: the link was the same; the QUIC
gap is the known quinn-under-loss one, the WS one is new or noise,
unexplained). Connect + run within noise except the slow link's WS cell
(1147 vs 832 ms, one run; 540-625 in five earlier runs tonight). Every
echo-under-download cell beats ssh (>15 s on the slow link).

### Real path (mini-jp, 122 ms, 2026-10-31 day, main 3a814b3)

Same rig as below (Go server on :7322, Rust beside it on :7323, Go
vsssh -quic for both). Five Go passes, five Rust passes at HEAD, two of
the pre-tonight Rust build (317b014) alternated with HEAD; raw lines in
bench/2026-10-31-real/. p50 per pass, WS / QUIC:

| | Go | Rust HEAD | Rust 317b014 | ssh (all passes) |
| --- | --- | --- | --- | --- |
| WS echo under download | 183 174 217 203 179 (median 183) | 211 175 194 263 138 (median 194) | 167 305 (last night 298 315) | 176-356 |
| QUIC echo under download | 133 130 131 130 128 | 139 179 176 134 128 | 131 129 | |
| WS / QUIC download meanwhile MB/s, median | 8.76 / 3.82 | 9.53 / 10.75 | 9.08 / 6.45 | |
| connect + run ms | 629-644 / 382-392 | 628-649 / 382-387 | 619-630 / 382-383 | 1547-1598 |

Six fresh 50 MB QUIC downloads each, alternating (MB/s): Go 7.52 7.36
2.65 1.61 5.08 2.88 (median 3.98), Rust 7.89 7.86 7.88 6.80 7.00 6.76
(median 7.43, 6 of 6 >= 90% of ssh), ssh 7.05 7.58 6.89 5.22 7.43 5.19
(median 6.97).

The path swings run to run (ssh's own echo p50 176-356 ms). WS echo
under download is no longer a clear loss: median 194 vs Go 183 ms
(+11, within the spread; the target <= Go is not shown). The pre-tonight
build's last-night 298-315 recurred once (305) but also gave 167, so the
fix is likely but not proven by these runs. The QUIC 176-179 ms passes
were noise: alternated with the old build, HEAD gave 134 and 128. Rust
moves more bulk meanwhile on both carriers, and QUIC downloads are
steady where quic-go's dropped to 1.6-2.9 MB/s tonight.

### Real path (mini-jp, 122 ms, 2026-10-31, before tonight's changes)

Go server (launchd, :7322) and the Rust server (beside it on :7323,
same keys), Go vsssh as the client for both, alternating, same session.
Two vsssh passes each, WS / QUIC vs ssh:

| | Go server | Rust server | ssh |
| --- | ---: | ---: | ---: |
| connect + run p50 | 638-639 / 387-392 ms | 633-643 / 377-382 ms | ~1590 ms |
| QUIC download alone | 10.58, **5.08** MB/s | 9.96, 9.58 MB/s | 9.9-11.5 |
| QUIC download during echo | 8.73, **4.41** MB/s | 10.34, 10.30 MB/s | ~10-11.5 |
| QUIC echo under download p50 | 127, 131 ms | 133, 129 ms | 174-196 ms |
| **WS echo under download p50** | 187, 190 ms | **298, 315 ms** | 174-196 ms |

Six fresh QUIC connections each (50 MB exec download, Go client, wall
clock including connect), and ssh the same way:

| | runs (MB/s) | median | >= 90% of ssh's median (6.9) |
| --- | --- | ---: | --- |
| Go (quic-go) | 7.82 6.01 8.75 5.47 7.54 8.20 | 7.68 | 4 of 6 |
| Rust (quinn, Cubic) | 6.64 7.75 7.54 7.60 8.00 7.68 | 7.64 | **6 of 6** |
| ssh | 6.38 6.41 7.42 6.43 7.43 7.53 | 6.9 | |

Counting connect favors osewa (ssh's handshake is ~1 s slower); without
it every osewa/ssh ratio drops ~10-15%, Rust vs Go unchanged. The QUIC
gate passes; the aim (>= ssh) is not shown. The WebSocket echo under
download is a clear loss on the real path (~110 ms, now worse than ssh),
the same as the Docker slow link.

### Echo under flood (e2e TestM2EchoUnderFlood, 1 Mbit/s, 100 ms, 1% loss)

W1 and W4 at Go (129-137 ms p50). W2, p50/p95, three alternating pairs
tonight: Rust 149/664, 150/529, 152/688 ms; Go 158/738, 255/744, 220/501.
The W2 p50 gap (Rust 176 vs Go 152 before) was the WebSocket deflate
level (3.55x vs Go's 5.46x on W2 screens; fixed in 3c69299). The tail is
screen size, not the holder: a trace (TestEchoBreakdown with W2) shows
every echo's screen cut and READY written 20 ms after the input, and the
slow keystrokes are the ones whose screen is 70-134 KB before deflate.

## Open

- **Screen age and W2 tail vs mosh.** Catch-up sends whole libghostty
  screens (W2: 12-134 KB raw, ~5.5x deflated); mosh sends diffs against
  the state the client acknowledged, one frame per interval. Beating it
  needs a protocol extension (a feature in HELLO/WELCOME: the holder
  sends VT that turns the client's acknowledged screen into the current
  one, falling back to a screen). Needs the user's decision and a Go
  client change for the e2e rig to measure it.
- **Download meanwhile on the slow link** (0.06 vs Go 0.10 MB/s): the
  typing-time path bound keeps ~9 packets in flight at 1 Mbit/s, so a
  1% loss often has too few later packets for fast retransmit. Knob:
  path::TYPING_QUEUE (10 ms); a 25 ms run didn't do better.
- **Real path WS echo under download**: re-measured (above), median
  194 vs Go 183 ms, within the path's run-to-run spread; more passes or
  a quieter hour would settle whether it beats Go.
- Reattach +5-17 ms, keystroke p95 noise-level, busy Wi-Fi download
  alone (above).
- Aims not met: idle host RSS 14.3 (<= 10), log tail host CPU 302 ms/MB
  (<= 250), flood 6 ms/MB (<= 5), flood RSS 35.6 (<= 30), typing host and
  client CPU 0.1 above mosh.
- Not yet measured: `make bench` throughput floor, RSS per extra idle
  session.
- Go's vsssh leaves its sessions behind (two `cat` holders per run).
- Env entries without `=` are dropped (std::process::Command can't
  pass them; Go passes them through).
