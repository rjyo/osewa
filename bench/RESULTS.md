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

## Real world: Thailand to Tokyo, over Tailscale

From one Mac mini Pro in Thailand to another in Tokyo, over Tailscale (about
125 ms round trip), osewa connects four times faster than ssh and six times
faster than mosh. During a download it echoes keys in 129 ms, 3 ms over
idle, against ssh's 178 and mosh's 198.

| Median of 5 runs | osewa QUIC | osewa WebSocket | ssh | mosh |
| --- | ---: | ---: | ---: | ---: |
| Connect + run a command | **387 ms** | 640 ms | 1,627 ms | 2,366 ms |
| Echo p50, idle | **126 ms** | 127 ms | 136 ms | 142 ms |
| Echo p50 / p95 during a download | **129 / 156 ms** | 164 / 276 ms | 178 / 208 ms | 198 / 228 ms |
| Download alone | 9.86 MB/s | **9.97 MB/s** | 9.82 MB/s | — |
| Download while typing | 9.82 MB/s | 9.57 MB/s | 10.87 MB/s | **11.07 MB/s** |

Echo is timed with prediction off, so it is the real round trip of a key.
mosh carries only the terminal, so its download ran over a separate ssh
connection on the same path, as a mosh user's would. Alone, every download
ran near 10 MB/s, close to what the line allows (below). While typing,
ssh's downloads filled the line to 96–98% and kept about 1 MB/s more than
osewa's, and their keys waited 50–70 ms longer, behind the queue that
takes. osewa's download left about a tenth of the line free, and its keys
did not queue. Runs of 2026-11-01 with `osewa-bench vs` on the current
build (raw output in [real-world-2026-11-01.txt](real-world-2026-11-01.txt)).

**The network.** The Tokyo Mac mini sits on 5 GHz Wi-Fi (802.11ac, 80 MHz)
behind an apartment cable line (coax, not fiber) whose upload is capped at
100 Mbit/s. That cap is the bottleneck for every download here: after
Tailscale's and Ethernet's headers it leaves about 11.3 MB/s of payload.
Tailscale ran direct between the two NATs (WireGuard over UDP, no relay),
and the benchmark checked that before and after the runs. The path is
jittery, as a cable upload's request-and-grant scheduling makes it: ping
before the runs measured 120 ms at best, 167 ms on average, with a 36 ms
standard deviation, and 116 / 156 / 33 ms after. Run to run it swings too
(ssh's echo during a download ranged 175–359 ms across the five runs, osewa
QUIC's 127–160), so read differences under about 20 ms as noise. It also
changes from day to day, since the building's neighbours share the cable
upload: on 2026-10-31 the same path gave ssh 5.36 MB/s alone, against 9.82
here, so compare columns within one table, not across days. That evening
the Thai ISP's path would not go direct at all, and Tailscale fell back to
its Tokyo relay, which caps everything near 2.8 MB/s and pushes ssh's echo
under load to about 700 ms. Those runs are not used here.

## Real world: Thailand to a Tokyo VPS

From the same Mac in Thailand to a 1-vCPU, 1 GB Linode in Tokyo, over the
public internet (about 100 ms round trip). With a faster path, osewa over
QUIC downloads at three times ssh's speed, and during the download keys
echo in 102 ms, as fast as idle; ssh's take 145.

| Median of 5 runs | osewa QUIC | osewa WebSocket | ssh | mosh |
| --- | ---: | ---: | ---: | ---: |
| Connect + run a command | **311 ms** | 511 ms | 1,337 ms | 1,993 ms |
| Echo p50, idle | **99 ms** | 101 ms | 110 ms | 114 ms |
| Echo p50 / p95 during a download | **102 / 105 ms** | 104 / 143 ms | 145 / 150 ms | 113 / 119 ms |
| Download alone | **39.67 MB/s** | 27.26 MB/s | 11.96 MB/s | — |
| Download while typing | **30.55 MB/s** | 27.28 MB/s | 11.55 MB/s | 15.45 MB/s |

QUIC's download while typing ranged 15.8–39.9 MB/s across the five runs;
the WebSocket's held at 27 in every one. Runs of 2026-10-31 with
`osewa-bench vs` (raw output in
[real-world-vps-2026-10-31.txt](real-world-vps-2026-10-31.txt)).

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

## Real world: ET, Thailand to a Tokyo VPS

Eternal Terminal 7.1.1 on a fresh 1-vCPU, 1 GB Linode in Tokyo, from a Mac in
Thailand over the public internet, about 104 ms round trip. Every tool ran in
the same session on the same host. ET's telemetry was off.

| Median of 5 runs | osewa QUIC | osewa WebSocket | ET |
| --- | ---: | ---: | ---: |
| Connect + run a command | **316 ms** | 520 ms | 2,145 ms |
| Echo p50, idle | 104 ms | 103 ms | **102 ms** |
| Echo p50 / p95 during a download | **106 / 111 ms** | 109 / 160 ms | 132 / 158 ms |
| Download alone | **38.99 MB/s** | 26.38 MB/s | 18.52 MB/s |
| Download while typing | **37.89 MB/s** | 26.50 MB/s | 18.76 MB/s |

ET's download runs through a forward on its terminal's connection, to a port
on the host serving random bytes. It swung from run to run, 12.8–24.1 MB/s
alone. Raw output in [real-world-et-2026-11-06.txt](real-world-et-2026-11-06.txt).

A separate test cut the link both ways for 15 s from the host
([outage.py](outage.py)), 2 rounds each. Times are seconds after the link
came back, good to a few tens of ms.

| | osewa | ET |
| --- | ---: | ---: |
| First new output | 0.88, 0.89 | 2.18, 1.76 |
| Last line of a command that wrote 22 MB in the outage | 1.04, 1.00 | 3.52, 3.34 |
| Bytes the client read for that | 2 kB | 22.5 MB |
| Last line of a command that wrote 240 MB | 36.0, 36.3 | 50.0, 52.2 |
| Bytes the client read for that | 1.1 MB | 241 MB |

The reader drained its terminal as fast as it could, so a real terminal
drawing ET's replay would take longer.
