# QUIC carrier

The wire contract stays [PROTOCOL.md](PROTOCOL.md): the same frames,
sessions, resume, flow control and auth. This note is the second
**carrier** for those frames, QUIC beside the TLS WebSocket, and how
`osewa_carrier::quic` implements it (quinn 0.11, rustls with ring as in
osewa-auth).

In short: the server serves QUIC on UDP beside TCP (`--no-quic` to turn it
off) and advertises it in WELCOME; a client that knows a host's QUIC port
races QUIC against the WebSocket. The frames travel on one main QUIC
stream, length-prefixed, with a zero-length frame as ping, and each
forward or exec stream's data on lanes of its own. Dials start a new
attempt every 500 ms, so one started while the network was down doesn't
wait out its retransmission backoff.

## Why

The WebSocket carrier is one TCP connection, so:
- **Head-of-line blocking:** one lost segment stalls every terminal, every
  channel and the control frames until it is retransmitted.
- **Network changes cost a reconnect:** a new address means a new TCP and
  TLS handshake, then resume.
- **No control over packet size:** TCP over a tailnet works, but we can't
  keep packets under the WireGuard MTU without the kernel's help.

QUIC gives independent streams over UDP, connection IDs that survive address
changes, and TLS 1.3 built in, and its handshake is a round trip shorter
than TCP + TLS + WebSocket upgrade. On a lossy 1 Mbit/s link, echo during a
download stays at its idle value over QUIC, because the download's lost
packets hold up only its lane.

## Discovery: WELCOME `carriers`

The first connection to a host is always WebSocket. Enrollment only gives
the TCP port, and a client can't know UDP works until it tries. The server
advertises QUIC in WELCOME, using the `wire.Carrier` type:

```json
{"version":0, "...":"...", "carriers":[{"carrier":"quic","port":7322,"alpn":["osewa.v0"]}]}
```

- **Port:** UDP on the same port number as the TCP listener by default
  (`--quic-listen` to change it, `--no-quic` to turn it off). The host
  is the one the client already dialed. A carrier entry never carries a
  host, so a compromised WELCOME can't steer a client to another machine.
  It wouldn't matter anyway, since the pin is checked again.
- **Cache:** the client keeps the port per host in `hosts.json`
  (`quic_port`) and uses it from the next connection on. An empty or
  missing `carriers` removes it.
- **ALPN:** `osewa.v0`, the WebSocket subprotocol string, so one constant
  names the protocol version on both carriers, and `osewa.v0+deflate` for
  compression (below). A server refuses other ALPNs in the handshake. A
  future `osewa.v1` would be listed next to them.

## Connecting: the race

A connection to a host with a known QUIC port races QUIC against the
WebSocket, the WebSocket starting 300 ms in (at once if QUIC fails). The
first carrier to deliver WELCOME wins; the other is closed. With UDP dead a
connection costs those 300 ms and lands on the WebSocket, whose WELCOME
re-saves the port. `OSEWA_CARRIER=quic` or `ws` (or `Config.carrier`)
forces one.

A dial gives up at once when the server answers no (a refused handshake, a
closed connection); on a silent path it starts a new attempt every 500 ms,
at most four in flight. A dial its caller drops (a timeout, a race the
WebSocket won) aborts its attempts at once, so a lost race stops sending.
The address dialed is the first IPv4 one unless the host is an IPv6
literal.

## Auth

The same TLS configs as the WebSocket's (`osewa_auth::server_config` and
`client_config`) with the ALPN list set, so the same checks apply:
- the device-key check on the server, on every handshake (the server never
  resumes sessions, so a revoked device can't come back on a ticket);
- the server-key pin on the client.

**No 0-RTT.** 0-RTT data can be replayed by the network, and the first
flight carries INPUT on reconnect.

quinn holds a handshake until the server takes the connection, so the
server accepts at once and bounds the wait for the main stream to 10 s.

## Streams

Frames keep their format. On a QUIC stream, as on a holder socket, each
frame is length-prefixed (`len u32`, then type, stream id, payload),
because a QUIC stream is a byte stream. A zero length, which no frame has,
is a ping, answered with one; only a client's pings are answered.

| QUIC stream | Opened by | Carries |
| --- | --- | --- |
| **Main** (the first bidirectional stream) | client | Everything but channel data: control, requests and responses, sessions (INPUT, OUTPUT, snapshots, events, acks), CREDIT and GRANT |
| **Lane** (a unidirectional stream per channel stream and direction) | its sender | One channel stream's data one way: DATA and EOF from the client; CHANNEL_DATA, CHANNEL_EOF and then STREAM_END from the server |

A lost packet of a download holds up only that download's lane: keystrokes
and their echo on the main stream go on.

The osewa stream id stays in every frame, so the server's relay and the
holder see exactly what they see over the WebSocket. Connections end with a
WebSocket status code sent as QUIC's application error code, and the
reason; unlike a WebSocket close it doesn't wait, so what is still unsent is
dropped.

**Order across streams.** Frames on different streams have no order on the
wire. The one place the protocol used to rely on one: a channel's
STREAM_END (on its lane, after its data) is written before the stream.close
RESPONSE that follows it (main stream). Over QUIC the RESPONSE may arrive
first, so a client keeps the channel's data until its STREAM_END
(PROTOCOL.md, Channel streams); over the WebSocket the order stays as
written. Nothing cheap can promise it: quinn applies a priority change to a
stream only once it next sends from it, so raising an ending lane above the
main stream doesn't reorder data already queued; holding the main stream
until the lane is acknowledged would stall the server's writes for a round
trip at every channel end. And a receiver that merges streams (a task per
stream feeding one queue) reorders frames that arrive close together
anyway. What the unsent bound (below) does keep: the RESPONSE overtakes at
most LOWAT of the lane (`response_overtakes_at_most_the_unsent_tail`;
without the bound it overtook 159 of 160 KB).

**Datagrams (RFC 9221)** are not used. Frames can't be lost, and the
latency-sensitive frames (INPUT, INPUT_ACK) are already small and at the
front of the main stream.

## Compression

permessage-deflate is a WebSocket extension and doesn't exist on QUIC.
Losing it would cost the 6-8x that output and snapshots compress by. So it
is negotiated in the handshake: a client offers ALPN `osewa.v0+deflate`
before `osewa.v0`, and a server that compresses (no `--no-compress`) takes
it (rustls picks by the server's order). The server's half of the main
stream is then one raw deflate stream, sync-flushed after each frame (the
`00 00 ff ff` marker kept), so each frame decodes as it arrives; the stream
never ends, and a final block is an error. Lanes and the client's half stay
plain, as on the WebSocket (see SECURITY.md).

## Flow control

CREDIT is end-to-end: the *holder* paces each stream by what the *client*
has received and buffered; the server only relays. QUIC flow control is
hop-by-hop. So **CREDIT stays exactly as it is and QUIC flow control never
binds**: receive windows start above credit's largest window
(`wire::CHANNEL_WINDOW`, 4 MiB) plus a frame. The holder's window then
always closes first, and the queue stays in the holder, where catch-up can
drop it and send a fresh screen, never in a QUIC send buffer that can't be
dropped. Credit's RTT and rate estimates keep working, since they are
measured at the application end to end.

Translating QUIC's window into CREDIT was rejected: it gives up "received
and buffered by the client" for "accepted by the client's stack".

**What quinn holds unsent.** A write to quinn returns once its bytes are
in quinn's buffer, which by default may hold megabytes nobody has sent
yet. Whatever sits there is out of reach of the server's send queue,
whose whole point is that control frames (INPUT_ACK, RESPONSE, GRANT) go
ahead of output and that catch-up can drop output that hasn't gone. Over
TCP, `TCP_NOTSENT_LOWAT` 16 KiB keeps that backlog in the queue. quinn
exposes neither a stream's unsent bytes nor bytes in flight; it bounds what
is written but unacknowledged by its send window. So before every write to
quinn the carrier sets that window to cwnd + 16 KiB: in flight is at most
cwnd, so at most 16 KiB more than cwnd lets out at once waits unsent,
across all streams (`frames::bound_unsent`). The window follows cwnd as
acks come, so it doesn't cap a download: 66 MB over a 120 ms relay took
2.5 s either way (`far_link_throughput`, ignored; 26 MB/s against 26-28
unbounded).

The main stream has a higher send priority than lanes, so an echo goes
ahead of the download data quinn holds unsent (at most the 16 KiB above,
130 ms at 1 Mbit/s). Each lane has its own write lock, so the carrier never
makes a write to one stream wait for another. The server doesn't use that:
its write loop (`client.rs` `write_loop`) awaits each frame in turn, so a
write blocked on a lane (quinn's send window, the peer's flow control)
holds up everything queued behind it, the main stream's frames included,
as a blocked socket does on the WebSocket. Credit keeps lanes from blocking
on flow control in practice.

## Packet size: 1200 bytes, for Tailscale

Tailscale's tun MTU is 1280. Over IPv6 that leaves 1280 - 40 - 8 = 1232
bytes of UDP payload, and QUIC's floor is 1200. A first packet of 1280
would be too big for a tailnet path: packets would be dropped until path
MTU discovery recovers, and the handshake's own packets are padded to the
full size.

- First packets are 1200 bytes on both sides. Every handshake packet then
  fits any path that can carry QUIC at all.
- Path MTU discovery stays on. On a normal path it grows packets toward
  1452 or so. On a tailnet its probes above 1232 fail and it stays put.
- No osewa frame has to fit in one packet, since streams fragment.

## Congestion control: Cubic without HyStart, by default

quinn's Cubic (RFC 9438) leaves slow start only on a loss or an ECN mark,
never on a rise in delay. A slow start that ends on delay (HyStart) misfires
on real paths: jitter reads as a queue, slow start ends at the first round
whose fastest round trips are 4-16 ms above the minimum, without a loss,
and Cubic then grows about a packet per round trip for seconds. Measured
with such a sender, of 6 fresh connections to a host 122 ms away, 2 left
slow start at a 106 and a 372 KB window and downloaded at 0.9 and 3.2 MB/s,
the others at 9 MB/s. Acknowledgments of app-limited sends don't grow the
window. The rest stays as ssh's TCP: the same algorithm family, a first
window of 32 packets, a first RTT guess of 100 ms.

Why not BBR by default: quinn marks its BBR experimental; it is BBRv1
(ProbeRTT drops the window to 4 packets for 200 ms every 10 s, loss doesn't
slow it, so it overruns a shallow buffer and crowds out TCP beside it). The
risk of Cubic without HyStart is the opposite one: slow start on a
deep-buffer link fills the buffer before its first loss. The server's
flight control (credit bounded to rate x (base RTT + 40/200 ms)) bounds what
osewa offers, which bounds that.

The knob: `OSEWA_QUIC_CC=cubic|bbr|newreno` in a process's environment
picks its sender's controller (an unknown value says so on stderr and uses
cubic). The server's governs downloads, so for an A/B on the real path set
it in the server's environment (the launchd/systemd unit, or the container)
and dial fresh connections. Pacing is always on: quinn paces every
controller's sends over the RTT, with no setting.

## Transport parameters

Both ends use the same (`quic::transport`):

| | |
| --- | --- |
| initial packet size | 1200 (`initial_mtu`, `min_mtu`), PMTUD on up to 1452 |
| idle timeout / keepalive | 30 s / 15 s, as the WebSocket's pings: two missed keepalives mean the connection is dead |
| stream receive window | 8 MiB (quinn doesn't auto-tune, so it starts where an auto-tuned one would end) |
| connection receive window | 64 MiB |
| incoming uni / bidi streams | 4096 / 100 |
| datagrams | off |
| initial RTT | 100 ms (quinn's default is 333) |
| first congestion window | 32 x 1200 bytes (quinn's default is 10 packets) |
| written but unsent | up to 16 KiB past what cwnd lets out, per connection (above) |
| UDP socket buffers | 7 MiB asked for (and forced on Linux where allowed): a burst at 10 MB/s and 120 ms doesn't fit Linux's default 208 KiB |

## Network changes

QUIC connections are named by connection IDs, not addresses, so the server
follows a client whose address changes (NAT rebinding) with no work from
us. When the path itself is gone (Wi-Fi to cellular, a new network), the
client does what it does on the WebSocket: a new connection and a resume
from the same epoch and position. That path is fast and tested. Active
migration (probe a new path, move the connection without a handshake) is
not used yet.

## TODO (WebSocket, not this carrier): deflating incompressible channel data

The WebSocket compresses every message over its threshold, CHANNEL_DATA
included; on random or already-compressed data that is CPU for nothing. The
loopback smoke test (`server/src/tests/quic.rs`, 32 MB of exec output from
/dev/urandom, in-process server, release build, 3 runs) downloads at
63-67 MB/s over the WebSocket against 206-211 MB/s over QUIC, whose lanes
are never deflated; echo p50 134-138 against 79-87 us. Worth trying: leave
channel data uncompressed on the WebSocket too (as lanes are), or give up on
a stream whose frames don't shrink.

## Open questions

1. **Datagrams for INPUT_ACK or input.** Are they worth it? With credit
   keeping data queues short, INPUT_ACK behind output should cost
   microseconds. Measure before adding a lossy path.
2. **Same UDP port as TCP?** Simple for users and firewall rules, but some
   networks treat UDP 7322 differently from UDP 443. Should the server also
   offer 443, or is a port per carrier entry enough?
3. **Remembering UDP failures per network.** Many networks block or
   throttle UDP. The race costs 300 ms there on every connect; a per-network
   cache (a salted hash of interface type, gateway and subnet, so a leaked
   file doesn't fingerprint the user) could skip QUIC for a while after a
   failure.
4. **Active migration.** Worth it over resume, which always works? It
   would save a handshake and a resume on a network change.
5. **One connection per host?** One QUIC connection per (client, host)
   carries every terminal. Should a client with many busy terminals open
   more connections for congestion fairness, or is one enough?
6. **Server memory.** Each connection has quinn's send buffers plus our
   windows. What per-connection and total caps do we enforce, like the
   WebSocket send queue's 16 MiB hard cap?
