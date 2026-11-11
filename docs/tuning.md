# Tuning and benchmarks

The defaults work. This page is for getting the most out of a particular link.

## `--max-rate-mbit`

Without a cap, the TCP connection between the relays probes for bandwidth by overflowing the
link. Every lost segment is retransmitted, and until it arrives **all** WireGuard traffic
behind it waits: at least one round trip, and seconds when losses come in bursts. With a cap
slightly below the link's capacity, the relay's TCP never overflows the link. The excess load
waits in the relay's own queue instead, where it is aged out and dropped, and the flows
inside WireGuard (a speedtest, a download) back off.

How to choose:

1. Measure the throughput between the two servers over the same transport, in both
   directions. For SSH, timing `ssh exit 'head -c 50000000 /dev/zero' | wc -c` and the
   reverse is enough.
2. Set the cap about 10% below the measured value: on the client for the upload direction
   (entry → exit), on the server for the download direction (exit → entry).

The cap is a trade-off: it lowers peak throughput a little in exchange for steadier latency
under load. If speeds without a cap are close to the link's capacity and the logs show
neither many `retrans` nor `ping_rtt_ms` spikes, running without a cap is fine.

When the transport is a local SSH or Cloak process, the cap applies to the hop into that
process, and that is enough. The transport forwards at most what it receives, so its own TCP
connection does not overflow the link either.

## `--congestion`

`bbr` (the default) controls the congestion algorithm of the relay's own TCP connection.

- **BBR** estimates the bandwidth and the minimum RTT, and keeps queues short. It holds up
  well with random loss, which long-haul links usually have.
- **cubic** (the usual Linux default) backs off on every loss and fills buffers until it
  sees one.

BBR needs the `tcp_bbr` kernel module. Without privileges, a process can select it only if it
is the system default or listed in `net.ipv4.tcp_allowed_congestion_control`; otherwise the
relay logs a warning and uses the system default.

If the long-haul TCP connection belongs to a transport (SSH, Cloak), that connection uses the
**system** setting, so set BBR system-wide on both servers (below).

## `--max-delay-ms`

This is the age limit for queued data (150 ms by default). Lower values give lower latency
under load and more drops; higher values give more throughput on bursty links and more
latency. Keep it around the link's RTT.

## Kernel settings

On both servers, in `/etc/sysctl.d/90-wg-tcp-relay.conf`
([file](../contrib/systemd/90-wg-tcp-relay.conf)):

```
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
net.ipv4.tcp_notsent_lowat = 131072
net.core.rmem_max = 67108864
net.core.wmem_max = 67108864
```

| Setting | Why |
|---|---|
| `tcp_congestion_control = bbr`, `default_qdisc = fq` | BBR for every TCP connection, including the transport's (SSH, Cloak); `fq` paces it. |
| `tcp_notsent_lowat` | Limits unsent data in socket buffers, so data waits where it can be dropped rather than in the kernel. The relay sets it on its own sockets too; this sysctl covers the transport. |
| `rmem_max`, `wmem_max` | Let the relay get the 8 MiB UDP buffers it asks for. The default limit (~200 KiB) overflows within milliseconds under load. |

In Docker, `net.ipv4.*` settings are per network namespace. A container that is not on
`network_mode: host` needs them in `sysctls:`. `net.core.*` settings are global and are set on
the host.

## Benchmarks

The end-to-end bench ([development](development.md#end-to-end-bench)) runs WireGuard through
the relay over a link with 180 ms RTT, 20 Mbit/s and a 50 ms bottleneck buffer. iperf3 runs
for 15 s while pings go through the tunnel (180 ms when idle). The pings show what other users
of the tunnel experience during a speedtest.

| Relay settings | Random loss | Throughput | Ping p95 under load | Stalls |
|---|---|---|---|---|
| cubic, no cap | 0 | 12-15 Mbit/s | 323-361 ms | none |
| cubic, `--max-rate-mbit 18` | 0 | 15-16 Mbit/s | 311-320 ms | none |
| bbr, no cap | 0 | 10-12 Mbit/s | 521-641 ms | none |
| **bbr, `--max-rate-mbit 18`** | 0 | 14-16 Mbit/s | 306-323 ms | none |
| cubic, `--max-rate-mbit 18` | 0.5% | 2-3 Mbit/s | 481-545 ms | 2 s without throughput |
| **bbr, `--max-rate-mbit 18`** | 0.5% | 7-9 Mbit/s | 401-470 ms | none |

What the table shows:

- No configuration stalls: the tunnel keeps answering pings throughout.
- The cap helps in every case. Without it, BBR overshoots the link, which causes outer
  retransmissions.
- With random loss, cubic collapses and BBR holds up.

On a real link (SSH between Russia and the US, about 180 ms RTT, measured capacity about
40 Mbit/s down and 32 Mbit/s up), a speedtest through the relay with no cap reached
36/31 Mbit/s with no stalls.

Your link is different: run the bench with your RTT, rate and loss (`DELAY_MS`, `RATE`, `LOSS`),
and check `ping_rtt_ms` and `retrans` in production logs.
