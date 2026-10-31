# osewa

**The terminal protocol that never drops.**

osewa runs your remote shell over a TCP/UDP hybrid: QUIC when UDP gets
through, TLS WebSocket when it doesn't. The session lives on the host, not
in the connection, so a dead link is just a reconnect, and your shell,
scrollback and running jobs never notice.

Like ssh, but the session survives the network. Like mosh, but it forwards
ports, runs commands and works where UDP is blocked.

The protocol is in [docs/PROTOCOL.md](docs/PROTOCOL.md) (QUIC in
[docs/QUIC.md](docs/QUIC.md)), benchmark results in
[bench/RESULTS.md](bench/RESULTS.md).

## Why now

ssh and mosh were built for networks we no longer have. Bandwidth is now
cheap almost everywhere: a Starlink dish, about the worst link most people
will meet, still carries tens of Mbit/s. What hurts now is latency, deep
buffers, switching networks and blocked UDP. And on the other end there is
often a coding agent, redrawing a full-screen TUI for hours while you check
in from a laptop or a phone.

| | ssh (1995) | mosh (2012) | osewa |
| --- | --- | --- | --- |
| Built for | One steady wired link | Scarce, lossy mobile bandwidth | Plenty of bandwidth, bad latency and queues |
| Session | Dies with the TCP connection | Survives roaming | Lives on the host: survives roaming, sleep and server restarts |
| Transport | TCP | UDP only | QUIC, falling back to TLS over TCP |
| Screen on the host | None, bytes are replayed | Its own emulator, no scrollback | libghostty-vt, Ghostty's terminal engine |
| Beyond the shell | Port forwards, exec | Terminal only | Port forwards and exec on the same connection |
| Kitty protocols (keyboard, graphics, clipboard) | Passed through | No | Keyboard today; graphics and clipboard next |
| OSC, including modern ones (hyperlinks, cwd, prompt marks, agent status OSC 7501) | Passed through | Few | Yes |

**mosh bet on saving bandwidth.** It sends small screen diffs from its own
terminal emulator, and when that emulator reads the output differently from
your terminal, you get the garbled text we have all seen. Whatever it
doesn't model, like hyperlinks, never reaches you. The bandwidth it saves no
longer matters; the cost remains: UDP only, no port forwarding, no
scrollback.

**There was no engine to keep the real screen.** In 2012 no terminal
emulator came as a library, so mosh wrote its own. libghostty-vt is
Ghostty's engine as a library, so the host can keep the same exact, modern
screen your terminal app draws, and send it whole when you reconnect.

**ssh is still the way in.** It is on every host, and osewa uses it once,
to install itself and enroll your device. But an ssh session is its TCP
connection: it dies when the network changes, and under load its queue
grows until each key waits behind a download (705 ms round trips in our
tests).

## Benchmarks: osewa vs ssh vs mosh

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

### Real world: Thailand to Tokyo

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

### Resource use

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

## Quick start

    osewa login --service me@devbox   # install on the host, enroll this device, run as a user service
    osewa attach devbox               # attach to (or create) a session
    osewa forward devbox 3000         # forward localhost:3000
    osewa exec devbox -- make test    # run a command, no terminal
    osewa update devbox               # upgrade the host over osewa, no ssh

Build from source (needs Rust and Zig for libghostty-vt):

    make libghostty   # pinned libghostty-vt into .cache/
    make check        # clippy + tests
    make build        # bin/osewa

## Roadmap

osewa is growing into two layers: a transport that any terminal app can
ride, and a multiplexer on top of it.

1. **Transport.** What ships today: QUIC with TLS WebSocket fallback,
   sessions held on the host, exactly-once input, keystrokes ahead of bulk.
   Next:
    - Screen diffs on catch-up: send what changed since the screen the
      client last acknowledged, not the whole screen, negotiated at
      connect. That takes mosh's last edge on a slow link, without the
      garbling: the diffs come from libghostty-vt's exact screen.
2. **Mux.** Tabs, splits and workspaces held on the host, like tmux, but
   built from osewa sessions instead of running inside one. Every pane is
   its own session with its own flow control, the layout lives on the host,
   and a terminal, a desktop app or a phone attaches to the same layout.
   With no tmux in between, there is one terminal engine between you and
   the program, and an agent's full-screen TUI redraws at the transport's
   speed.

The transport stays usable on its own: the mux is one client of the same
open protocol.

## Features

- **Hybrid carrier:** QUIC with automatic TLS WebSocket fallback, one frame
  format on both.
- **Persistent sessions:** survive sleep, IP changes, NAT rebinding and
  server restarts.
- **Exactly-once input:** sequenced and acked keystrokes, resent across
  reconnects.
- **Priority and flow control:** terminals before bulk, credit-based
  backpressure, a typing-aware in-flight cap.
- **No head-of-line blocking:** each port forward and exec gets its own
  QUIC stream.
- **Predictive local echo** with mosh-style epochs; never shows passwords.
- **Port forwarding and remote exec** (`-L` style forwards,
  stdin/stdout/stderr) on the same connection.
- **Device keys:** Ed25519 per device, TLS 1.3, pinned server key. No
  passwords, no trust on first use.
- **Self-hosting:** `osewa login` installs over ssh once; `osewa update`
  upgrades over osewa.
- **Open protocol and a client library,** built on Ghostty's
  terminal engine. Linux and macOS hosts.

## How it works

osewa separates the session from the transport, then makes the transport
hard to kill.

1. **TCP/UDP hybrid transport.** The same framed protocol runs on two
   carriers. QUIC over UDP: a 1-RTT handshake, connection IDs that survive
   address changes, and a separate stream per port forward, so a lost
   packet in a download never blocks your echo. TLS 1.3 WebSocket over TCP:
   it gets through proxies and UDP-hostile networks. The server advertises
   QUIC, the client caches it and races both carriers with a 300 ms head
   start for QUIC. Nothing to configure.
2. **The host keeps the real screen.** Each terminal runs in its own
   process on the host, which keeps the actual screen with libghostty-vt,
   Ghostty's terminal engine. Reconnecting sends that screen as one
   snapshot, not a replay of everything you missed. And when output
   outruns a slow link, you get the latest screen instead of a
   minutes-long backlog. Connections come and go; restarting the server
   doesn't touch a session.
3. **The link never gives up.** Pings detect a dead path in about a
   second. The client redials, racing attempts every 500 ms, and resumes
   every session on the new connection. Input carries sequence numbers and
   is resent until acked, so no keystroke is lost or doubled.
4. **Keystrokes first.** Frames go out by priority (control, then
   terminals, then exec, then forwards) under end-to-end credit flow
   control. Bulk streams are held to the path's bandwidth-delay product
   plus 40 ms of queue while you type, and 200 ms when you don't, so a
   download can't bury your echo. Over TCP the server also asks the kernel
   how much is already in flight, and while you type holds the queue to
   10 ms.
5. **Predictive local echo.** On slow links the client draws each key
   before the host echoes it, mosh-style, and checks it against the real
   screen. Nothing on a line is shown until the host confirms one key, so
   passwords never appear.

Auth is per-device Ed25519 keys over TLS 1.3, with the server key pinned.
You enroll once over ssh, and osewa installs itself on the host. After
that, even updates go over osewa.

## How we measured

osewa and OpenSSH ran against the same container, through the same
published ports, over netem-shaped links with realistic buffers: far fiber
(120 ms, 50 Mbit/s), cellular with a deep buffer (60 ms, 10 Mbit/s), Wi-Fi
with 1% loss (20 ms, 20 Mbit/s) and a slow link (100 ms, 1 Mbit/s, 1%
loss). Echo is the time from sending a key to seeing it, during a download
on the same connection. Recovery is measured by moving the client to a new
IP address after 5 s offline. Real-path checks ran against a host in Tokyo
over Tailscale. The rig is an e2e suite in Docker, run against an image of
this build (`docker build --target binary`); the results are in
[bench/RESULTS.md](bench/RESULTS.md).

## FAQ

**Does it replace ssh?** For interactive work, yes. You need ssh once, to
enroll; after that osewa runs on its own port (TCP and UDP 7322).

**What if UDP is blocked?** The client races QUIC against a TLS WebSocket
and takes whichever connects first. With UDP blocked, that costs 300 ms per
connect.

**How is it different from mosh?** mosh keeps sessions alive and predicts
echo too, but it is UDP-only and carries only the terminal. osewa adds TCP
fallback, port forwarding and exec, and keeps keystrokes ahead of bulk data
on the same connection.

**Does it work behind a VPN or tailnet?** Yes. QUIC starts with 1200-byte
packets to fit a WireGuard MTU, and all the numbers to a Tokyo host were
taken over Tailscale.

## License

Apache 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
