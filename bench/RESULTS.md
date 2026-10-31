# Benchmarks: osewa vs ssh vs mosh

osewa against ssh and mosh, the numbers [README.md](../README.md) quotes.

## Shaped links, in Docker

On a bad network, osewa echoes keys 8–100× faster than ssh under load, and
comes back from a network change faster than mosh.

| | osewa | mosh | ssh |
| --- | --- | --- | --- |
| Transport | QUIC (UDP) + TLS WebSocket (TCP), raced | UDP only (SSP) | TCP |
| Survives sleep / IP change | Yes, resumes in **0.4 s** | Yes, 1.1 s | No |
| Echo p50 during a download, cellular (60 ms, deep buffer) | **138 ms** | — | 1,714 ms |
| Echo p50 during a download, Wi-Fi (20 ms, 1% loss) | **44 ms** | — | 374 ms |
| Echo during a download, 1 Mbit/s (100 ms, 1% loss) | **154 ms** | — | none within 15 s |
| Connect + run a command, 120 ms RTT | **388 ms** | — | 1,630 ms |
| Local echo prediction | Yes | Yes | No |
| Port forwarding, exec | Yes | No | Yes |
| Works with UDP blocked | Yes, falls back to TCP | No | Yes |
| Bulk throughput | Equal to ssh; ahead on lossy Wi-Fi (0.99 vs 0.92 MB/s) | — | — |

osewa numbers are over QUIC, its default, from the full benchmark run of
2026-10-31. Over the TLS WebSocket, echo during a download was faster still
on cellular (73 ms) and the 1 Mbit/s link (125 ms). One caveat: in this test
rig QUIC reaches the container through Docker's UDP port forwarding, which
caps a 50 Mbit/s download near 1.4 MB/s.

## Real world: Thailand to Tokyo

From one Mac mini Pro in Thailand to another in Tokyo, over Tailscale (about
125 ms round trip), osewa connects four times faster than ssh, and over QUIC
it echoes keys during a download 48 ms sooner.

| Median of 5 runs | osewa QUIC | osewa WebSocket | ssh |
| --- | ---: | ---: | ---: |
| Connect + run a command | **387 ms** | 629 ms | 1,577 ms |
| Echo p50, idle | **126 ms** | **126 ms** | 131 ms |
| Echo p50 / p95 during a download | **139 / 176 ms** | 194 / 240 ms | 187 / 222 ms |
| Download alone | 8.96 MB/s | 9.11 MB/s | **10.97 MB/s** |
| Download while typing | **10.75 MB/s** | 9.53 MB/s | 10.48 MB/s |
| Fresh 50 MB download, connect included (6 runs) | **7.43 MB/s** | | 6.97 MB/s |

This path is fast and its buffers are shallow, so ssh's queue stays short:
the gap is far smaller than on the simulated cellular and slow links above.
Over the TLS WebSocket, osewa's echo during a download is no better than
ssh's. The path swings from run to run (ssh's own echo during a download
ranged 176–315 ms), so read differences under about 20 ms as noise.

## Resource use

An idle osewa session takes 14.3 MB on the host, half of ssh's and two
thirds of mosh's, and 6.9 MB on the client. The cost is host CPU on heavy
output: osewa keeps the real screen, so it parses everything the program
writes, where ssh just forwards bytes.

| | osewa | mosh | ssh |
| --- | ---: | ---: | ---: |
| Host memory, idle session | **14.3 MB** | 20.6 MB | 27.4 MB |
| Client memory, idle session | **6.9 MB** | 14.3 MB | 8.2 MB |
| Host / client CPU, typing 10 keys/s | 0.5% / 0.6% | 0.4% / 0.5% | <0.1% / 0.1% |
| Host CPU per MB, log tail at 50 kB/s | 302 ms | 434 ms | **66 ms** |
| Host CPU per MB, output flood | **6 ms** | 45 ms | 12 ms |
| Host memory, output flood | 35.6 MB | **20.2 MB** | 27.6 MB |

Same container and run as above, osewa over the TLS WebSocket; an idle
session uses under 0.1% CPU on both ends for all three. In a flood the
program wrote 173 MB/s under osewa, 22 MB/s under mosh and 1.2 MB/s under
ssh, which holds the program back to the link's speed.

*osewa won't fill the network's buffers to squeeze out bulk throughput,
which is what makes ssh's keys take seconds to echo under load. On lossy
links bulk speed swings run to run for both.*

## How we measured

osewa and OpenSSH ran against the same container, through the same
published ports, over netem-shaped links with realistic buffers: far fiber
(120 ms, 50 Mbit/s), cellular with a deep buffer (60 ms, 10 Mbit/s), Wi-Fi
with 1% loss (20 ms, 20 Mbit/s) and a slow link (100 ms, 1 Mbit/s, 1%
loss). Echo is the time from sending a key to seeing it, during a download
on the same connection. Recovery is measured by moving the client to a new
IP address after 5 s offline. Real-path checks ran against a host in Tokyo
over Tailscale. The rig is an e2e suite in Docker, run against an image of
this build (`docker build --target binary`).
