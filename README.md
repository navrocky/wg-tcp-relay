# wg-tcp-relay

WireGuard over TCP that does not stall.

`wg-tcp-relay` carries the UDP traffic of many WireGuard peers over **one TCP stream**, typically
an SSH port forward or a [Cloak](https://github.com/cbeuw/Cloak) tunnel, for networks where
plain UDP between your servers is blocked or throttled. Unlike generic UDP-over-TCP tools, it
understands WireGuard and is built around one rule: **under load, drop packets; never queue
them for seconds and never block.**

```
 WireGuard peers                                                           WireGuard server
 (phones, routers)                                                         (wg-easy, wg-quick)
      │ UDP                                                                        ▲ UDP
      ▼                                                                            │
┌──────────────────┐        one TCP stream: SSH -R / -L, Cloak, ...       ┌──────────────────┐
│ wg-tcp-relay     │ ═══════════════════════════════════════════════════► │ wg-tcp-relay     │
│ client           │ ◄═══════════════════════════════════════════════════ │ server           │
│ :51820/udp       │                                                       │ :5000/tcp        │
└──────────────────┘                                                       └──────────────────┘
   entry server                                                               exit server
```

## Why

Tunneling WireGuard over TCP has two classic failure modes, and generic tools hit both:

1. **Stalls.** A relay that applies backpressure (waits when a socket or stream is busy) can
   get stuck: one slow peer blocks everyone, a missed wakeup freezes a flow forever. Users
   see "the VPN hangs until I reconnect".
2. **Bufferbloat.** TCP never loses packets, it queues them. WireGuard's inner TCP flows (a
   speedtest, a download) never see loss, keep accelerating, and fill every buffer on the
   way. Latency for *everything* in the tunnel grows to seconds, handshakes time out, and
   the tunnel "freezes for 10-15 seconds" under load.

What `wg-tcp-relay` does about it:

- **Never blocks on UDP.** A full UDP socket means the datagram is dropped, like a router
  would do. One peer cannot stall another.
- **Bounded, aged queues.** Each peer's queue toward TCP is capped in bytes, and a packet
  that waited longer than `--max-delay-ms` (150 ms by default) is dropped instead of sent.
  This turns congestion into loss, which the inner TCP flows back off from.
- **WireGuard-aware priority.** Handshakes, cookie replies and keepalives skip the data
  queue, so sessions stay up even when the link is saturated.
- **Fairness.** Data is served round-robin between peers: one peer's speedtest does not
  starve the others.
- **Optional rate cap** (`--max-rate-mbit`). Pacing the relay's own TCP slightly below the
  link capacity means the outer TCP never overflows the link. There are no outer
  retransmissions, and each of those would stall *all* tunneled traffic for at least one RTT.
- **Filters non-WireGuard traffic.** Only well-formed WireGuard messages are relayed.
- **Survives reconnects.** A session outlives the TCP connection. After a reconnect the
  server reuses the same per-peer UDP sockets, so WireGuard endpoints stay stable.
- **Fast failure detection.** In-band ping/pong (5 s) drops a dead connection after 15 s and
  reconnects with backoff.
- **Observability.** Periodic stats with drop reasons, tunnel RTT measured with in-band
  pings (including queueing), kernel `TCP_INFO` (rtt, cwnd, retransmits, unsent bytes),
  queue depth. See [Troubleshooting](#troubleshooting).
- **Small.** A static musl binary in a distroless image (~10 MB). No shell, runs as non-root.

## Quick start

Exit server (where the WireGuard server runs; here wg-easy in the same Docker network):

```yaml
# exit server: docker-compose.yml
services:
  relay:
    image: navrocky/wg-tcp-relay:latest
    command: server --listen 0.0.0.0:5000 --target wg-easy:51820
    restart: unless-stopped
    sysctls:
      net.ipv4.tcp_notsent_lowat: 131072
    networks: [proxy]
networks:
  proxy:
    external: true
```

Entry server (the address your WireGuard peers connect to):

```yaml
# entry server: docker-compose.yml
services:
  relay:
    image: navrocky/wg-tcp-relay:latest
    command: client --listen 0.0.0.0:51820 --connect 127.0.0.1:5000
    network_mode: host
    restart: unless-stopped
```

Then bring TCP port 5000 of the exit server to `127.0.0.1:5000` on the entry server: see
[Transport: SSH](#transport-ssh) or [Transport: Cloak](#transport-cloak). Point the peers'
`Endpoint` at the entry server's `:51820`.

The relay is transparent to WireGuard: keys, configs and peers stay the same.

## Transport: SSH

A reverse SSH forward works when the exit server can connect to the entry server, but not the
other way round. In that case the exit server opens the tunnel:

```sh
# on the exit server (e.g. in a small alpine+openssh container)
ssh -N \
  -o ServerAliveInterval=10 -o ServerAliveCountMax=3 \
  -o ExitOnForwardFailure=yes \
  -o ConnectTimeout=15 \
  -o Compression=no \
  -o IPQoS=lowdelay \
  -R 127.0.0.1:5000:relay:5000 \
  tunnel@entry.example.com
```

Run it under a supervisor that restarts it, for example a Docker `restart: always` policy.
This is enough, `autossh` is not needed. What each piece is for:

| Setting | Why |
|---|---|
| **A dedicated SSH connection** | Do not share it with other forwards (proxies, web panels). All channels of one SSH connection share one TCP stream: a bulk download in another channel delays WireGuard (head-of-line blocking). |
| `ServerAliveInterval=10`, `ServerAliveCountMax=3` | A dead connection is noticed in about 30 s instead of minutes or hours of TCP timeouts. |
| `ExitOnForwardFailure=yes` | If the remote port is still held by a stale session, exit and retry instead of staying connected **without** the forward. Without it you get a "connected" tunnel that forwards nothing. |
| `-N` | No remote command. Avoid `sleep 365d`-style keepers, which restart the tunnel periodically. |
| `Compression=no` | WireGuard traffic is encrypted and does not compress, so compression only adds CPU and latency. |
| Bind the forward to `127.0.0.1` | Only the local relay client needs it. |
| Pass options on the command line | When `~/.ssh/config` is baked into an image, it silently goes stale unless the image is rebuilt. |

On the **entry server**, `sshd` must notice dead clients too. Otherwise a dead session keeps
holding port 5000, and every reconnect fails on the forward:

```
# /etc/ssh/sshd_config.d/tunnel.conf
ClientAliveInterval 10
ClientAliveCountMax 3
AllowTcpForwarding yes
# Only if you bind the forward to a non-loopback address:
# GatewayPorts clientspecified
```

Kernel settings, on both servers (`/etc/sysctl.d/90-tunnel.conf`):

```
# Keep unsent data out of kernel socket buffers (less queueing inside the tunnel)
net.ipv4.tcp_notsent_lowat = 131072
# Allow large UDP buffers on the entry server (the relay asks for 8 MiB)
net.core.rmem_max = 67108864
net.core.wmem_max = 67108864
```

In Docker, `tcp_notsent_lowat` is per network namespace. Set it with `sysctls:` on the
containers that own the TCP sockets (the relay, the ssh container). `wg-tcp-relay` also sets
`TCP_NOTSENT_LOWAT` on its own sockets.

A forward SSH tunnel (`-L`) works the same way, with the entry server opening the connection.

## Transport: Cloak

[Cloak](https://github.com/cbeuw/Cloak) disguises a TCP stream as ordinary TLS traffic to a
real website. Unauthenticated visitors and active probes get the real website, which makes
the tunnel hard to detect and block with DPI. Unlike the SSH setup above, the **entry**
server connects to the exit server, so the exit server's port 443 must be reachable.

Exit server: `ck-server` forwards the `wg` proxy method to the relay server.

```jsonc
// ck-server.json (exit server)
{
  "ProxyBook": { "wg": ["tcp", "127.0.0.1:5000"] },
  "BindAddr": [":443"],
  "RedirAddr": "www.example.com",      // real site served to everyone else
  "PrivateKey": "<ck-server -key>",
  "BypassUID": ["<ck-server -uid>"]
}
```

Entry server: `ck-client` exposes the tunnel on a local port, and the relay client connects
to it.

```jsonc
// ck-client.json (entry server)
{
  "Transport": "direct",
  "ProxyMethod": "wg",
  "EncryptionMethod": "plain",         // WireGuard is already encrypted
  "UID": "<uid>",
  "PublicKey": "<public key>",
  "ServerName": "www.example.com",
  "BrowserSig": "chrome",
  "NumConn": 4,
  "StreamTimeout": 300
}
```

```sh
ck-client -c ck-client.json -s exit.example.com -p 443 -l 5000
wg-tcp-relay client --listen 0.0.0.0:51820 --connect 127.0.0.1:5000
```

`EncryptionMethod: plain` avoids encrypting twice. `NumConn` controls how many TLS connections
Cloak spreads the stream over. Check the Cloak documentation for the current option set.

## Configuration

All options can also be set with environment variables.

| Option | Env | Default | Description |
|---|---|---|---|
| `client --listen` | `WGTR_LISTEN` | `0.0.0.0:51820` | UDP address WireGuard peers connect to |
| `client --connect` | `WGTR_CONNECT` | (required) | relay server TCP address |
| `server --listen` | `WGTR_LISTEN` | `0.0.0.0:5000` | TCP address for relay clients |
| `server --target` | `WGTR_TARGET` | (required) | WireGuard server UDP address, resolved per new peer |
| `--max-delay-ms` | `WGTR_MAX_DELAY_MS` | `150` | drop data that waited longer in the queue |
| `--peer-queue-kb` | `WGTR_PEER_QUEUE_KB` | `256` | queue limit per peer |
| `--max-rate-mbit` | `WGTR_MAX_RATE_MBIT` | `0` (off) | cap the relay's TCP send rate (see [Tuning](#tuning)) |
| `--congestion` | `WGTR_CONGESTION` | `bbr` | TCP congestion control for the relay connection; empty = system default |
| `--keepalive-secs` | `WGTR_KEEPALIVE_SECS` | `5` | in-band ping interval |
| `--dead-after-secs` | `WGTR_DEAD_AFTER_SECS` | `15` | reconnect when nothing was received for this long |
| `--peer-timeout-secs` | `WGTR_PEER_TIMEOUT_SECS` | `180` | forget idle peers |
| `--stats-secs` | `WGTR_STATS_SECS` | `60` | stats log interval, 0 = off |
| | `RUST_LOG` | `info` | log level: `info`, `debug`, `trace`, or per module, e.g. `wg_tcp_relay=debug` |

The tuning options apply to the side that sends. Set them on both sides: the client sends
uploads, the server sends downloads.

## Tuning

**`--max-rate-mbit` is the most effective setting.** Measure the real throughput between
the two servers, for example `iperf3` through the same transport, and set the cap about
10% below it on **both** sides. Without a cap, the outer TCP probes for bandwidth by
overflowing the bottleneck, and every lost segment stalls the whole tunnel for at least one
RTT. With a cap, the excess waits in the relay queue, where it is aged out and dropped.

**`--congestion`.** BBR is the default and is the better choice on lossy long-haul links (see
the benchmarks). It needs the `tcp_bbr` kernel module (`modprobe tcp_bbr`); without it the
relay warns and uses the system default. If the transport adds its own TCP hop (SSH, Cloak),
the setting applies to the relay's hop only, so set the host default too
(`net.ipv4.tcp_congestion_control = bbr`, `net.core.default_qdisc = fq`).

**`--max-delay-ms`.** Lower values mean lower latency under load and more drops. Keep it
around the link RTT.

### Benchmarks

WireGuard through the relay on the e2e bench: a link with 180 ms RTT, 20 Mbit/s and a 50 ms
bottleneck buffer, iperf3 for 15 s, pings through the tunnel at the same time (180 ms idle).
The pings show what other users of the tunnel experience during a speedtest.

| Relay settings | Random loss | Throughput | Ping p95 under load | Stalls |
|---|---|---|---|---|
| cubic, no cap | 0 | 12-15 Mbit/s | 323-361 ms | none |
| cubic, `--max-rate-mbit 18` | 0 | 15-16 Mbit/s | 311-320 ms | none |
| bbr, no cap | 0 | 10-12 Mbit/s | 521-641 ms | none |
| **bbr, `--max-rate-mbit 18`** | 0 | 14-16 Mbit/s | 306-323 ms | none |
| cubic, `--max-rate-mbit 18` | 0.5% | 2-3 Mbit/s | 481-545 ms | 2 s without throughput |
| **bbr, `--max-rate-mbit 18`** | 0.5% | 7-9 Mbit/s | 401-470 ms | none |

Takeaways:

- No configuration stalls: the tunnel keeps answering pings throughout.
- The rate cap helps in every case. BBR without a cap overshoots the link, which causes
  outer retransmissions and higher latency.
- With random loss, which long-haul links usually have, cubic collapses and BBR holds up.
  Hence the recommended setup is **BBR (the default) plus `--max-rate-mbit` about 10% below
  the link capacity**.

Your link differs: run the bench with your RTT, rate and loss (`DELAY_MS`, `RATE`, `LOSS`),
and watch `ping_rtt_ms` and `retrans` in production logs.

## Troubleshooting

At `info` level the relay logs connections, peers, and every `--stats-secs`:

```
stats peers=3 udp_in=48210 udp_out=51877 drop_queue_full=120 drop_aged=310 drop_udp_full=0
      drop_not_wg=0 drop_unknown_peer=0 queued_frames=2 queued_bytes=2904 queue_oldest_ms=12
link stats link=127.0.0.1:5000 uptime_s=3600 ping_rtt_ms=192 ping_rtt_max_ms=640
      tcp_rtt_ms=181.5 retrans=0 sent_mb=812 received_mb=5310
```

| Field | Meaning |
|---|---|
| `drop_queue_full`, `drop_aged` | Overload: the link is slower than the offered traffic. A few under load is the relay doing its job. Many all the time: the link is too slow, or lower `--max-rate-mbit`. |
| `drop_udp_full` | The local UDP socket buffer was full. Raise `net.core.wmem_max` / `rmem_max`. |
| `drop_not_wg` | Non-WireGuard datagrams arrived (scanners, a misconfigured peer). |
| `drop_unknown_peer` | Data for a peer that already expired (normal right after a long idle). |
| `ping_rtt_ms` vs `tcp_rtt_ms` | In-band RTT, including the relay queues and outer retransmissions, compared with the kernel's RTT estimate. A large gap means data waits *behind* something: queueing or retransmissions. |
| `retrans` | Outer TCP retransmissions in the interval. Each one stalls the tunnel for at least one RTT. Use `--max-rate-mbit` to avoid them. |

`RUST_LOG=wg_tcp_relay=debug` adds:

- a `link` line every keepalive with kernel `TCP_INFO`: cwnd, unacked, `notsent_bytes`,
  retransmits, delivery rate, and queue depth by kind;
- every WireGuard handshake, cookie and keepalive in both directions, which tells you
  whether a peer that "does not connect" reaches the relay at all;
- connection setup timings, reconnect backoff, socket buffer sizes the kernel actually
  granted, TCP writes blocked for more than 1 s.

`RUST_LOG=wg_tcp_relay=trace` logs every packet and every drop with its reason. This is very
verbose; use it on a test setup only.

Typical patterns:

- **`connection failed: relay handshake timed out`**: something is listening on the TCP port,
  but it is not a `wg-tcp-relay` server. The forward points to the wrong place.
- **The tunnel connects, but there are no `wireguard control from server` lines.** The relay
  server cannot reach `--target`. Check `WireGuard server unreachable` warnings.
- **Reconnects every ~15 s with `nothing received`.** The transport is blackholing traffic.
  Check the SSH/Cloak logs.

## Building and testing

```sh
cargo test                        # unit tests + in-process relay tests
docker build -t wg-tcp-relay:dev .
docker build -t wg-tcp-relay-e2e tests/e2e
docker run --rm --privileged wg-tcp-relay-e2e    # real WireGuard over an emulated slow link
```

The end-to-end bench (`tests/e2e/run.sh`) builds a peer, the two relay halves and a
WireGuard server in separate network namespaces. It emulates the link between the relays
with `netem` (delay, loss) and `tbf` (rate, bottleneck buffer), runs `iperf3` uploads,
downloads and 8 parallel streams through WireGuard, and fails if pings through the tunnel
show a gap longer than 3 s. Parameters: `DELAY_MS`, `RATE`, `LOSS`, `BUFFER_MS`,
`CONGESTION`, `MAX_RATE_MBIT`, `DURATION`. It needs the `wireguard`, `sch_netem` and
`sch_tbf` kernel modules on the host.

## Versions and releases

The project follows [Semantic Versioning](https://semver.org); changes are listed in
[CHANGELOG.md](CHANGELOG.md). Before 1.0, a minor version bump may change the wire protocol
or options. Run the same version on both sides.

Docker images (`linux/amd64`, `linux/arm64`) on
[Docker Hub](https://hub.docker.com/r/navrocky/wg-tcp-relay):

| Tag | What |
|---|---|
| `X.Y.Z` | exact release, recommended for deployments |
| `X.Y` | latest patch release of `X.Y` |
| `X` | latest release of major version `X` (from 1.0 on) |
| `latest` | latest release |
| `edge` | current `main`, not a release |
| `sha-<commit>` | a specific `main` commit |

Every release on [GitHub](https://github.com/navrocky/wg-tcp-relay/releases) also has static
Linux binaries (`x86_64`, `aarch64`) and `SHA256SUMS`.

To make a release:

1. Set `version` in `Cargo.toml` and run `cargo check` (this updates `Cargo.lock`).
2. In `CHANGELOG.md`, rename `[Unreleased]` to `[X.Y.Z] - YYYY-MM-DD`, add an empty
   `[Unreleased]` section above it, and update the links at the bottom.
3. Commit, then `git tag vX.Y.Z && git push origin main vX.Y.Z`.

CI refuses a tag that does not match `Cargo.toml` or has no CHANGELOG section. Tags like
`v1.2.0-rc.1` become pre-releases.

## License

MIT
