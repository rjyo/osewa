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

`cargo test`: 607 tests, every Go test of these packages ported (none
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

## Benchmarks (2026-10-30/31, same rig and session, Go image vs Rust image)

### Resource use (`make bench-res`), WebSocket rows

| Workload | Measure | Go | Rust | Must | |
| --- | --- | ---: | ---: | ---: | --- |
| idle | host RSS | 29.7 MB | 14.2 MB | <= 20 | pass (aim <= 10 not met) |
| idle | client CPU / RSS | 0.1% / 14.5 MB | 0.1% / 6.8 MB | <0.1% / <= 10 | CPU fail (0.1%), RSS pass |
| typing | host CPU | 1.5% | 0.9% | <= 1.0% | pass (aim <= mosh 0.7% not met) |
| typing | client CPU | 1.6% | 1.1% | <= 1.0% | **fail** |
| log tail | host CPU per MB | 710 ms | 368 ms | <= 514 | pass (aim <= 250 not met) |
| log tail | host RSS | 48.1 MB | 26.7 MB | <= 33 | pass (aim <= 25 not met) |
| flood W2 | host CPU per MB | 8 ms | 6 ms | <= 8 | pass (aim <= 5 not met) |
| flood W2 | host RSS | 71.1 MB | 36.9 MB | <= 40 | pass (aim <= 30 not met) |
| flood W2 | client RSS | 22.7 MB | 8.6 MB | <= 15 | pass (aim met) |

### vs mosh (`make bench-mosh`), no loss / 1% loss

| Measure | Go | Rust | Must <= Go |
| --- | ---: | ---: | --- |
| keystroke to glyph p95 | 2.2 / 1.8 ms | 2.5 / 3.0 ms | **fail** (0.3 / 1.2 ms; mosh itself moved 1.9-3.4 ms run to run) |
| screen age W2 flood p50 | 133 / 187 ms | 138 / 146 ms | +5 ms / better |
| screen age W2 flood p95 | 209 / 380 ms | 219 / 284 ms | +10 ms / better |
| screen age W1, W4 p50 | 131 ms | 128-131 ms | pass |
| recovery after network change | 620 / 754 ms | 422 / 734 ms | pass (aim 0.6 s met without loss) |
| reattach from a new process | 308 / 298 ms | 323 / 319 ms | **fail** (+15-21 ms) |
| bytes/min, W2 at 50 kB/s | 1.26 MB | 1.17 MB | pass (ties mosh) |

Screen age is still ~55 ms behind mosh: the holder keeps Go's catch-up
design, so beating mosh needs a design change, not a port.

### vs ssh (`make bench-vs`), Go client against each server, WS / QUIC

Far fiber and cellular: every cell within noise of Go. Busy Wi-Fi:
download alone 0.80 / 0.97 vs Go 0.98 / 1.27 MB/s, but ssh moved
0.99 → 0.75 between the two runs (1% loss); echo under download 84 / 45
vs 82 / 60 ms. Slow link: connect + run 741 / 509 vs 1201 / 831 ms
(better); echo under download p50 **151 / 183 vs 108 / 162 ms (worse)**.
Every echo-under-download cell still beats ssh (>15 s on the slow link).
One run each.

### Echo under flood (e2e TestM2EchoUnderFlood, 1 Mbit/s, 100 ms, 1% loss)

W1, W4 p50 at or below Go (129-131 vs 131 ms). W2 p50 185-218 vs
166-198 ms; **W2 p95 553 ms-1.17 s vs 350-409 ms**: an open loss.

## Open

- W2 echo tail and slow-link echo under download: the holder's catch-up
  path under a heavy flood. Next step: OSEWA_TRACE breakdown
  (TestEchoBreakdown) on both images.
- Typing client CPU 1.1% (must 1.0%), idle client 0.1% (must <0.1%).
- Reattach +15-21 ms and keystroke p95: re-measure over several runs
  before chasing.
- Not yet measured: the real path (`vsssh -quic mini-jp`, six fresh
  connections, needs a Rust server on mini-jp and the user's ssh agent),
  `make bench` throughput floor, RSS per extra idle session.
- Env entries without `=` are dropped (std::process::Command can't
  pass them; Go passes them through).
