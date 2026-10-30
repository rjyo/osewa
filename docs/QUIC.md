# QUIC carrier (M3 design note)

Status: prototype (stage A). `internal/carrier` carries the frames on one
QUIC stream, length-prefixed, with a zero-length frame as ping; the server
serves it on UDP beside TCP (`--no-quic`), and a client uses it only when
asked (`Config.Carrier` or `OSEWA_CARRIER=quic`): no WELCOME carriers, race
or failure cache yet. Dials race a new attempt every 500 ms, so one started
while the network was down doesn't wait out its retransmission backoff.
Measured: connect + run 1.6x faster than WebSocket (397 against 652 ms to
a host 122 ms away; 4x faster than ssh), network-change recovery 0.71 s
(WebSocket 0.74 s, mosh 1.1-1.2 s), bulk download on par once slow start
has run its course.

Known weakness: **slow start ends early in some connections.** quic-go
(v0.64.1, no setting for it) ends slow start at the first round whose
fastest round trips are 4-16 ms above the connection's minimum (HyStart),
without a loss; its Cubic then grows about a packet per round trip for
seconds. Jitter on a real path does that sometimes: of 6 fresh
connections to a host 122 ms away, 2 left slow start at a 106 and a
372 KB window and downloaded at 0.9 and 3.2 MB/s, the others at 9 MB/s.
A connection whose handshake ran on a faster path is stuck the same way:
that, not Docker's UDP forwarding, was the collapse the scoreboard first
showed (its connections were dialed before the link was shaped; they are
now dialed after).

Stage B, first part: **lanes.** Each forward or exec stream's data goes on
a unidirectional QUIC stream of its own each way (DATA and EOF from the
client; CHANNEL_DATA, CHANNEL_EOF and STREAM_END from the server); all
else stays on the main stream. A lost packet of a download no longer
holds up a keystroke's echo: on the 1 Mbit/s lossy link, echo during a
download is p50 106 ms, its idle value (WebSocket 139, ssh never). Next:
terminals on lanes of their own, and migration proper.

Stage C, first part: **discovery and the race.** The server advertises
QUIC in WELCOME `carriers` once it serves it; `hosts.json` keeps the port
(`quic_port`), and a connection to a host with one races QUIC against the
WebSocket, the WebSocket starting 300 ms in (at once if QUIC fails). With
UDP dead a connection costs those 300 ms and lands on the WebSocket, whose
WELCOME re-saves the port. No per-network failure cache yet.

**Compression** is negotiated in the handshake: a client offers ALPN
`osewa.v0+deflate` before `osewa.v0`, and a server that compresses (no
`--no-compress`) takes it. The server's half of the main stream is then a
deflate stream, flushed after each frame (output, snapshots); lanes and the
client's half stay plain, as on the WebSocket.

Originally: design only, no code. The wire contract stays
[PROTOCOL.md](PROTOCOL.md). This note proposes a second **carrier** for the
same frames, and lists what has to be decided before it lands.

## Why

The WebSocket carrier is one TCP connection, so:
- **Head-of-line blocking across terminals:** one lost segment stalls every
  terminal and the control channel until it is retransmitted. That is part
  of the flood screen-age tail in `bench/MOSH.md` at 10 Mbit/s, 100 ms
  RTT and 1% loss: p95 382 ms against mosh's 101 ms, worst 802 ms against
  153 ms. Part of the gap is that osewa streams output where mosh sends only
  the newest screen, so QUIC alone won't close it.
- **Network changes cost a reconnect:** a new address means a new TCP and
  TLS handshake, then resume. `bench/MOSH.md` has recovery at 1.41 s
  against mosh's 515 ms without loss (904 ms against 1.21 s with 1% loss).
  Migration can skip the reconnect.
- **No control over packet size:** TCP over a tailnet works, but we can't
  keep packets under the WireGuard MTU without the kernel's help.

QUIC gives independent streams over UDP, connection IDs that survive address
changes, and TLS 1.3 built in. It keeps everything else in the protocol:
frames, sessions, resume, flow control, auth.

## Discovery: WELCOME `carriers`

The first connection to a host is always WebSocket. Enrollment only gives
the TCP port, and a client can't know UDP works until it tries. The server
advertises QUIC in WELCOME, using the existing `wire.Carrier` type:

```json
{"version":0, "...":"...", "carriers":[{"carrier":"quic","port":7322,"alpn":["osewa.v0"]}]}
```

- **Port:** UDP on the same port number as the TCP listener by default
  (`--quic-listen` to change it, `--no-quic` to turn it off). The host
  is the one the client already dialed. A carrier entry never carries a
  host, so a compromised WELCOME can't steer a client to another machine.
  It wouldn't matter anyway, since the pin is checked again.
- **Cache:** the client stores the carriers per host in `hosts.json` and
  uses them from the next connection on. An empty or missing `carriers`
  removes the cached entry.
- **ALPN:** `osewa.v0`, the WebSocket subprotocol string, so one constant
  names the protocol version on both carriers. A server refuses other ALPNs
  in the handshake. A future `osewa.v1` would be listed next to it.

## Auth

The same `tls.Config`s: `auth.ServerTLS` and `auth.ClientTLS` with
`NextProtos` set to the ALPN list. quic-go drives Go's `crypto/tls` through
its QUIC API, so the existing `VerifyConnection` checks apply unchanged:
- the device-key check on the server, on every handshake including
  resumption;
- the server-key pin on the client.

**No 0-RTT.** 0-RTT data can be replayed by the network, and the first
flight carries INPUT on reconnect. Session resumption (1-RTT) is fine; its
revocation check is the one above. The QUIC handshake is already one round
trip shorter than TCP + TLS + WebSocket upgrade.

## Streams

Frames keep their format. On a QUIC stream, as on a holder socket, each
frame is length-prefixed (`len u32`, then type, stream id, payload),
because a QUIC stream is a byte stream.

| QUIC stream | Opened by | Carries |
| --- | --- | --- |
| **Control** (the first bidirectional stream) | client | HELLO, WELCOME, and REQUEST/RESPONSE for `session.list`, `session.create`, `session.kill`. Connection-level frames (stream 0) |
| **Terminal** (one bidirectional stream per osewa stream) | client | Client → server: the terminal's `session.attach`, `detach`, `own` and `resize` REQUESTs, INPUT, ACK, CREDIT. Server → client: their RESPONSEs, INPUT_ACK, SNAPSHOT_*, OUTPUT, EVENT, STREAM_END |

Why the per-terminal requests travel on the terminal's own stream:
PROTOCOL.md relies on ordering between a stream's responses and its data.
- The attach RESPONSE (with `head` and `epoch`) comes before the first
  SNAPSHOT_BEGIN.
- STREAM_END `detached` comes before the detach RESPONSE.
- INPUT_ACK comes right after the attach response.

Across QUIC streams there is no order, so these must share one. A client
therefore opens a terminal stream with the attach REQUEST as its first
frame. The osewa stream id stays in every frame, so the server's relay and
the holder see exactly what they see today.

What this buys:
- A lost packet on one terminal's data stalls only that terminal.
  Keystrokes to it still travel on their own direction of the stream, and
  the control stream is untouched.
- The server needs no shared per-client send queue: each terminal has its
  own QUIC stream, so one slow terminal no longer holds back another. The
  4 MiB-per-stream bound toward holders stays.

**Datagrams (RFC 9221)** are not used in this design. Frames can't be lost,
and the latency-sensitive frames (INPUT, INPUT_ACK) are already small and
at the front of their direction. See open question 3.

## Flow control

Today CREDIT is end-to-end: the *holder* paces each stream by what the
*client* has received and buffered. The server only relays. QUIC flow
control is hop-by-hop: the client's stack opens the window as the client
reads, and only the server sees it. The holder is behind a Unix socket.

Proposal: **keep CREDIT exactly as it is and make QUIC flow control never
bind.**
- Per-stream receive windows on the client are set above the holder's
  largest window (`maxWindow` 4 MiB) plus a frame: `InitialStreamReceiveWindow`
  of 512 KiB, auto-tuned up to `MaxStreamReceiveWindow` of 8 MiB.
- The connection window is sized for a few busy terminals:
  `MaxConnectionReceiveWindow` of 32 MiB.
- The holder's window then always closes first. The queue stays in the
  holder, where catch-up can drop it and send a fresh screen. It never
  sits in a QUIC send buffer that can't be dropped.
- Credit's RTT and rate estimates keep working, since they are measured
  at the application end to end.

The alternative was for the server to translate QUIC's window into CREDIT,
crediting what the peer's stack has accepted. That gives up "received and
buffered by the client" for "accepted by the client's stack". quic-go also
doesn't expose the peer's MAX_STREAM_DATA. Rejected.

Client to server, the server's receive window per terminal stream is the
existing 4 MiB input bound: a client that overruns it is blocked by QUIC
instead of having its stream ended.

## Packet size: 1200 bytes, for Tailscale

Tailscale's tun MTU is 1280. Over IPv6 that leaves 1280 - 40 - 8 = 1232
bytes of UDP payload, and QUIC's floor is 1200. quic-go's default first
packet size, 1280, is too big for a tailnet path. Packets would be dropped
until path MTU discovery recovers, and the handshake's own packets are
padded to the full size.

- Set `InitialPacketSize: 1200` on both sides. Every handshake packet then
  fits any path that can carry QUIC at all.
- Keep path MTU discovery on (DPLPMTUD, which quic-go implements). On a
  normal path it grows packets toward 1452 or so. On a tailnet its probes
  above 1232 fail and it stays put.
- No osewa frame has to fit in one packet, since streams fragment. Only
  datagrams would need to fit, and we don't use them.

## Connecting: QUIC first, WebSocket after 1.5 s

For a host whose cached carriers include QUIC:
1. Start the QUIC handshake, and send HELLO plus any attach in the first
   flight after it, as on WebSocket.
2. If QUIC has not reached WELCOME within **1.5 s**, start the WebSocket
   dial in parallel. Start it at once if QUIC fails outright (ICMP
   unreachable, or a handshake error).
3. The first carrier to deliver WELCOME wins. The other is closed, and
   if it was QUIC, it is cancelled before its HELLO is answered.
4. If WebSocket wins while QUIC was still trying, keep the session on
   WebSocket and record the failure (next section). Don't migrate a live
   session from WebSocket to QUIC; the next reconnect will try QUIC again
   according to the cache.

1.5 s is far above a QUIC handshake on any sane path: one round trip, and
our self-signed Ed25519 certificate is a few hundred bytes, one packet. It
is also well under the M1 cold-resume target. It wants a measurement on the tc fixture before it is fixed.

## Remembering UDP failures per network

Many networks block or throttle UDP: hotels, corporate Wi-Fi, some mobile
carriers. Paying 1.5 s on every connect there is the wrong default. The
client keeps a small cache, `quic-failures.json` next to `hosts.json`:

```json
{"<network key>": {"until": "2026-10-29T10:00:00Z"}}
```

- The **network key** identifies "where I am" without anything sensitive
  in clear: a hash of the local interface type, the default gateway's
  address, and the local subnet. On iOS it would come from
  `NWPathMonitor` (interface type, plus the SSID if the app has that
  entitlement). The network key is open question 5.
- **After a failure** on a network, that network goes straight to
  WebSocket for 24 h. The client can still try QUIC in the background once
  in that time, without the user waiting on it, and clear the entry if it
  works.
- **A QUIC success** clears the network's entry.
- Failures caused by the host itself are not network failures: a refused
  handshake, or no carrier in WELCOME. They aren't cached against the
  network.

## Network changes: migration

QUIC connections are named by connection IDs, not addresses.
- **On the server:** quic-go follows a client whose address changes
  (NAT rebinding) with no work from us.
- **On the client** (Wi-Fi to cellular, a new Wi-Fi): an active migration
  probes the new path and moves the connection without a new handshake.
  The session keeps its stream ids, credit and resend queue. quic-go has
  an API for this in recent releases (add a path on a new local socket,
  probe it, switch). It is newer than the rest of the library, so we should
  pin a version and test it in the e2e fixture (route flip under tc).
- **If migration isn't possible or fails** (the server's address family
  isn't reachable from the new network, or the probe times out), fall back
  to what WebSocket does today: a new connection and a resume from the
  same epoch and position. That path is already fast and tested.

Idle and liveness:
- `MaxIdleTimeout` 30 s and `KeepAlivePeriod` 15 s, matching the WebSocket
  ping logic: two missed keepalives mean the connection is dead.
- The client's own detection of a network change (a path monitor) triggers
  a migration attempt at once, instead of waiting out the idle timeout.

## Compression

permessage-deflate is a WebSocket extension and doesn't exist on QUIC.
Losing it would cost the 6-8× that output and snapshots compress by. So the
QUIC carrier needs its own:
- the server → client half of each terminal stream is a deflate stream
  with a sync flush after each frame;
- the control stream likewise, or uncompressed, since it is small;
- client → server stays uncompressed, as on WebSocket (see SECURITY.md),
  and `--no-compress` turns it off the same way.

Whether this is on is negotiated in the ALPN or in HELLO; see open
question 2.

## What quic-go provides, and what we build

**quic-go (`github.com/quic-go/quic-go`) provides:**
- QUIC v1 (RFC 9000) and v2, with TLS 1.3 through Go's `crypto/tls`, so our
  certificates and verify callbacks work as they are;
- bidirectional and unidirectional streams, with per-stream and
  connection flow control and receive-window auto-tuning;
- loss recovery and congestion control (NewReno and Cubic);
- path MTU discovery and a configurable initial packet size;
- connection IDs, NAT rebinding, and a client path-migration API;
- keepalives, idle timeout, Retry and address validation against spoofing,
  datagrams (RFC 9221) if we ever want them, and GSO/ECN on Linux.

**We build:**
- the server's UDP listener and accept loop;
- a carrier interface under `client` and `server`, so the relay and the
  session code don't care which carrier they run on;
- length-prefixed framing on streams, and the stream layout above;
- the per-stream compression;
- WELCOME `carriers`, the `hosts.json` cache, the 1.5 s race, the
  per-network failure cache, and migration triggers from a path monitor;
- e2e coverage: QUIC against WebSocket under loss, UDP blocked (fallback
  and caching), a route flip (migration), and a 1280 MTU link (tailnet).

**Not quic-go's job:** the iOS and Android clients. iOS has QUIC in
Network.framework (`NWProtocolQUIC`, with stream multiplexing through
`NWMultiplexGroup`), so it doesn't need a Go library. Android has Cronet,
or a Rust or Java stack. Their stream layout and TLS pinning must match
this note exactly.

## Rust: quinn (osewa_carrier::quic)

The Rust carrier is quinn 0.11 (rustls with ring, as osewa-auth), wire
compatible with the Go one: the same ALPNs, main stream, lanes, ping and
deflate, WebSocket status codes as QUIC application error codes. It is
tested both ways against quic-go (`crates/osewa/tests/go_quic.rs`).

**Congestion control: Cubic without HyStart, by default.** quinn's Cubic
(RFC 9438) leaves slow start only on a loss or an ECN mark, never on a
rise in delay, so the early exit above (jitter read as a queue, a window
stuck at 106-372 KB) can't happen; acknowledgments of app-limited sends
don't grow the window. Everything else stays as on the Go side and as
ssh's TCP: the same algorithm family, quic-go's first window (32 packets),
its first RTT guess (100 ms). A comparison against Go then measures the
HyStart change and nothing else. Why not BBR by default: quinn marks its
BBR experimental; it is BBRv1 (ProbeRTT drops the window to 4 packets for
200 ms every 10 s, loss doesn't slow it, so it overruns a shallow buffer
and crowds out TCP beside it). The risk of Cubic without HyStart is the
opposite one: slow start on a deep-buffer link fills the buffer before
its first loss. The server's flight control (credit bounded to rate x
(base RTT + 40/200 ms)) bounds what osewa offers, which bounds that.

The knob: `OSEWA_QUIC_CC=cubic|bbr|newreno` in a process's environment
picks its sender's controller (an unknown value says so on stderr and
uses cubic). The server's governs downloads, so for an A/B on the real
path set it in the server's environment (the launchd/systemd unit, or the
container) and dial fresh connections. Pacing is always on: quinn paces
every controller's sends over the RTT, with no setting.

**Transport parameters**, against Go's `QUICConfig`:

| | Go (quic-go) | Rust (quinn) |
| --- | --- | --- |
| initial packet size | 1200 | 1200 (`initial_mtu`, `min_mtu`), PMTUD on up to 1452 |
| idle timeout / keepalive | 30 s / 15 s | 30 s / 15 s |
| stream receive window | 6 MiB, auto-tuned to 8 MiB | 8 MiB (quinn doesn't auto-tune) |
| connection receive window | 32 MiB, auto-tuned to 64 MiB | 64 MiB |
| incoming uni / bidi streams | 4096 / 100 | 4096 / 100 |
| datagrams | off | off |
| initial RTT | 100 ms | 100 ms (quinn's default is 333) |
| first congestion window | 32 packets | 32 x 1200 bytes (quinn's default is 10) |
| written but unsent | about a packet per stream (a write returns once the rest fits in one) | up to 16 KiB past what cwnd lets out, per connection (below) |
| UDP socket buffers | 7 MiB asked for | 7 MiB asked for (and forced on Linux where allowed) |
| dial address | first IPv4 unless an IPv6 literal (`net.ResolveUDPAddr`) | the same |

**What quinn holds unsent.** A write to quinn returns once its bytes are
in quinn's buffer, which by default may hold megabytes nobody has sent
yet. Whatever sits there is out of reach of the server's send queue,
whose whole point is that control frames (INPUT_ACK, RESPONSE, GRANT) go
ahead of output and that catch-up can drop output that hasn't gone. Over
TCP, `TCP_NOTSENT_LOWAT` 16 KiB keeps that backlog in the queue; quic-go's
writes return only with about a packet unsent. quinn exposes neither a
stream's unsent bytes nor bytes in flight; it bounds what is written but
unacknowledged by its send window. So before every write to quinn the
carrier sets that window to cwnd + 16 KiB: in flight is at most cwnd, so
at most 16 KiB more than cwnd lets out at once waits unsent, across all
streams (`frames::bound_unsent`). The window follows cwnd as acks come,
so it doesn't cap a download: 66 MB over a 120 ms relay took 2.5 s either
way (`far_link_throughput`, ignored; 26 MB/s against 26-28 unbounded).

**Order across streams.** Frames on different streams have no order on
the wire. The one place the protocol used to rely on one: a channel's
STREAM_END (on its lane, after its data) is written before the
stream.close RESPONSE that follows it (main stream). Over QUIC the
RESPONSE may arrive first, so a client keeps the channel's data until its
STREAM_END (PROTOCOL.md, Channel streams); over the WebSocket the order
stays as written. Nothing cheap can promise it: quic-go round-robins
streams with about a packet unsent on each; quinn applies a priority
change to a stream only once it next sends from it, so raising an ending
lane above the main stream doesn't reorder data already queued; holding
the main stream until the lane is acknowledged would stall the server's
writes for a round trip at every channel end. And a receiver that merges
streams (ours and Go's: a task or goroutine per stream feeding one queue)
reorders frames that arrive close together anyway. What the unsent bound
does keep: the RESPONSE overtakes at most LOWAT of the lane
(`response_overtakes_at_most_the_unsent_tail`; without the bound it
overtook 159 of 160 KB).

**Where it differs from Go, on purpose:**
- The main stream has a higher send priority than lanes, so an echo goes
  ahead of the download data quinn holds unsent (at most the 16 KiB
  above, 130 ms at 1 Mbit/s); quic-go round-robins.
- Each lane has its own write lock, so the carrier never makes a write to
  one stream wait for another. The server doesn't use that: its write
  loop (`client.rs` `write_loop`) awaits each frame in turn, so a write
  blocked on a lane (quinn's send window, the peer's flow control) holds
  up everything queued behind it, the main stream's frames included, as a
  blocked socket does on the WebSocket. Credit keeps lanes from blocking
  on flow control in practice.
- A dial gives up at once when the server answers no (a refused handshake,
  a closed connection), where Go keeps starting attempts until its
  deadline; on a silent path both start a new attempt every 500 ms, at
  most four in flight. A dial its caller drops (a timeout, a race the
  WebSocket won) aborts its attempts at once, as Go's deferred cancel does.
- quinn holds a handshake until the server takes the connection, so the
  server accepts at once and bounds the wait for the main stream to 10 s,
  as Go does.

**TODO (WebSocket, not this carrier): deflating incompressible channel
data.** The WebSocket compresses every message over its threshold,
CHANNEL_DATA included, as Go's coder/websocket does; on random or
already-compressed data that is CPU for nothing. The loopback smoke test
(`server/src/tests/quic.rs`, 32 MB of exec output from /dev/urandom,
in-process server, release build, 3 runs) downloads at 63-67 MB/s over the
WebSocket against 206-211 MB/s over QUIC, whose lanes are never deflated;
echo p50 134-138 against 79-87 us. Worth trying: leave channel data
uncompressed on the WebSocket too (as lanes are), or give up on a stream
whose frames don't shrink.

## Open questions

1. **Dependency.** quic-go is a new dependency, with more under it
   (`golang.org/x/net`, `x/sys`, `x/crypto`). CLAUDE.md says to ask before
   adding one. Is the M3 gain worth it, against keeping WebSocket only and
   improving TCP behaviour instead?
2. **Compression negotiation.** Per-stream deflate on by default, decided
   by the server's `--no-compress`? Signalled by a second ALPN
   (`osewa.v0+deflate`), or by a WELCOME field? Is a flush per frame or per
   write batch the right granularity for latency?
3. **Datagrams for INPUT_ACK or input.** Are they worth it? With credit
   keeping data queues short, INPUT_ACK behind output should cost
   microseconds. Measure before adding a lossy path.
4. **Same UDP port as TCP?** Simple for users and firewall rules, but some
   networks treat UDP 7322 differently from UDP 443. Should the server also
   offer 443, or is a port per carrier entry enough?
5. **Network key.** What identifies a network well enough to cache UDP
   failures, but without fingerprinting the user if `quic-failures.json`
   leaks? A salted hash per device is the current proposal.
6. **Migration maturity.** Which quic-go version do we pin, and what's the
   fallback if its migration API changes? Resume always works, so
   migration is an optimization, not a dependency.
7. **Fallback timing.** Is 1.5 s right for phones on poor cellular, where
   a QUIC handshake can legitimately take longer than a TCP one that some
   middlebox accelerates? Should it adapt per host from past handshake
   times?
8. **One connection per host?** One QUIC connection per (client, host)
   carries every terminal. Should a client with many busy terminals open
   more connections for congestion fairness, or is one enough?
9. **Server memory.** Each connection has quic-go send buffers plus our
   windows. What per-connection and total caps do we enforce, like the
   WebSocket send queue's 16 MiB hard cap today?
10. **Holder-side impact.** None is expected: the holder sees the same
    frames on its Unix socket, and credit stays end-to-end. Confirm with
    the holder agent that nothing in catch-up assumes one TCP connection
    per client.
