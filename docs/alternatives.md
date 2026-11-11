# Alternatives

There are many ways to get WireGuard through a network that blocks or degrades UDP. This page
compares `wg-tcp-relay` with the common ones, and says when another tool is the better choice.

What `wg-tcp-relay` is for: **many WireGuard peers, one real TCP connection that can go
through anything that forwards TCP (SSH, Cloak, a TCP proxy), and steady behavior under
load.**

| Tool | Carries UDP as | Through SSH / TCP proxies | Many peers | Load handling |
|---|---|---|---|---|
| **wg-tcp-relay** | real TCP, one connection for all peers | yes | yes | bounded aged queues, drops, handshake priority, per-peer fairness |
| wstunnel | WebSocket / HTTP2 over TCP, or WebTransport over QUIC | yes (WebSocket / HTTP2) | yes | backpressure: queues, can stall (see below) |
| udp-over-tcp (Mullvad) | real TCP, length-prefixed datagrams | yes | one peer per `udp2tcp` instance | none documented |
| udp2raw | fake TCP (raw sockets) | no | yes | none: "no congestion control or re-transmission" |
| phantun | fake TCP (TUN) | no | not stated | none |
| chisel | SSH over HTTP/WebSocket | yes | yes, up to `UDP_MAX_CONNS` flows | not documented |
| socat | TCP byte stream, no framing | yes | no | none, breaks datagram boundaries |

## wstunnel

[wstunnel](https://github.com/erebe/wstunnel) tunnels TCP and UDP over WebSocket, HTTP2 or
WebTransport. It looks like web traffic and supports TLS, HTTP proxies and many other
features. It is a good general-purpose tunnel.

With WireGuard under load, we ran into a stall in wstunnel 11.1.0: the VPN froze until the
user reconnected. The client had stopped reading its tunnel sockets, with megabytes of data
unread and no CPU use. Reading the source showed two contributing designs:

- all local UDP peers share one UDP socket, and the per-peer writers wait on it with
  `poll_send_to`, so when the socket buffer is full only one waiting writer is woken;
- the dispatch loop hands datagrams to per-peer streams one at a time and waits until each is
  read, so one stuck peer stops reception for everyone.

Both get worse when kernel UDP buffers are small. Its WebSocket ping (`--websocket-ping-frequency`)
recovers stuck flows by closing them, which shows up as a freeze for the length of the ping
timeout.

Beyond the stall, wstunnel uses backpressure: under overload, data queues in TCP and
WebSocket buffers instead of being dropped. Latency grows for everyone in the tunnel, and
WireGuard handshakes wait behind bulk data.

**wg-tcp-relay instead**

- never waits on UDP;
- drops data from bounded, aged queues instead of buffering it;
- lets WireGuard control messages skip the queue;
- shares the link round-robin between peers;
- uses one TCP connection for all peers.

**Choose wstunnel** if you need the traffic to look like HTTPS without extra tools, need HTTP
proxy support, or tunnel more than WireGuard.

## udp-over-tcp (Mullvad)

[udp-over-tcp](https://github.com/mullvad/udp-over-tcp) is a small, clean forwarder:
datagrams with a 16-bit length prefix over a TCP connection. `tcp2udp` on the server can
serve many clients, but "one `Udp2Tcp` instance can handle traffic from a single peer only".
It does not document any queue management, keepalive or reconnect: the forwarder runs "until
the TCP socket disconnects".

**wg-tcp-relay instead**

- serves many peers on one port and one connection;
- reconnects by itself, and keeps sessions (and WireGuard endpoints) across reconnects;
- detects dead connections with in-band pings;
- handles overload by dropping instead of queueing;
- prioritizes WireGuard handshakes.

**Choose udp-over-tcp** for a single client and the smallest possible code base.

## udp2raw and phantun

[udp2raw](https://github.com/wangyu-/udp2raw) and [phantun](https://github.com/dndx/phantun)
send UDP payloads in packets with **fake TCP headers**. On the wire they look like TCP, but
there is no real TCP connection: udp2raw's traffic is "essentially UDP", with "no congestion
control or re-transmission". This avoids the TCP-over-TCP problem entirely. When it works, it
gives the best throughput and latency of everything on this page.

Costs:

- they need raw sockets or a TUN interface, root or `CAP_NET_RAW` / `CAP_NET_ADMIN`, and
  iptables rules on both sides;
- they need direct IP reachability between the servers. phantun "will **not** be able to
  pass through L7 proxies", and neither tool can go through an SSH forward, Cloak or any other
  real TCP transport;
- stateful middleboxes that check TCP behavior closely can break them.

**wg-tcp-relay instead** works as an unprivileged process over any real TCP transport, and
needs no firewall tricks. It pays for that with TCP-over-TCP, and mitigates the cost with
bounded queues, dropping and pacing.

**Choose udp2raw or phantun** when the servers can exchange raw TCP-looking packets directly,
and only UDP is blocked.

## chisel

[chisel](https://github.com/jpillora/chisel) is "a fast TCP/UDP tunnel, transported over HTTP,
secured via SSH". It supports reverse tunnels, auto-reconnect and SOCKS. UDP flows are limited
by `UDP_MAX_CONNS` (100 by default). It is a general-purpose tunnel and documents no
WireGuard-specific behavior or overload handling.

**wg-tcp-relay instead** is built for WireGuard load: per-peer bounded, aged queues, drops
instead of buffering, handshake priority, sessions that survive reconnects, and detailed
link statistics.

**Choose chisel** for a single tool that tunnels many kinds of traffic over HTTP.

## socat, gost and other generic forwarders

`socat UDP-LISTEN:... TCP:...` does not work for WireGuard: TCP is a byte stream, and socat
writes datagrams to it without framing, so datagram boundaries are lost. Toolkits like
[gost](https://github.com/go-gost/gost) support many protocols and forwarding modes. None of
them is tuned for keeping a WireGuard tunnel responsive under load.

## When you do not need TCP at all

If UDP between the servers works but WireGuard is fingerprinted and blocked, an obfuscated UDP
transport is better than any TCP tunnel, because it avoids TCP-over-TCP:

- [AmneziaWG](https://github.com/amnezia-vpn/amneziawg-go): WireGuard with obfuscated
  handshakes and junk packets;
- [swgp-go](https://github.com/database64128/swgp-go): a proxy that obfuscates WireGuard
  packets.

Full proxy stacks (Xray/VLESS, Hysteria, TUIC, OpenVPN over TCP) replace WireGuard rather than
carry it. They are a different choice, made at a different level.
