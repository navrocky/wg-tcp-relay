# Troubleshooting

## Log lines

At the default `info` level the relay logs connections, peers, and two summary lines every
`--stats-secs` (60 s):

```
stats peers=3 udp_in=48210 udp_out=51877 drop_queue_full=120 drop_aged=310 drop_no_conn=0
      drop_udp_full=0 drop_not_wg=0 drop_unknown_peer=0 queued_frames=2 queued_bytes=2904
      queue_oldest_ms=12
link stats link=127.0.0.1:5000 uptime_s=3600 ping_rtt_ms=192 ping_rtt_max_ms=640
      tcp_rtt_ms=181.5 retrans=0 sent_mb=812 received_mb=5310
```

Counters are per interval.

| Field | Meaning |
|---|---|
| `udp_in`, `udp_out` | WireGuard datagrams received from / sent to the local side. |
| `drop_queue_full`, `drop_aged` | Overload: more traffic than the link carries. Some drops under load are the relay doing its job. Many drops all the time mean the link is too slow, or try a lower [`--max-rate-mbit`](tuning.md#--max-rate-mbit). |
| `drop_no_conn` | Traffic arrived while no TCP connection was up. Normal during a reconnect, since WireGuard retries handshakes by itself. If it keeps growing, the relay cannot connect: look for `connection failed` warnings. |
| `drop_udp_full` | The local UDP socket buffer was full. Raise `net.core.rmem_max` and `wmem_max` ([tuning](tuning.md#kernel-settings)). |
| `drop_not_wg` | Datagrams that are not WireGuard messages (scanners, a misconfigured peer). |
| `drop_unknown_peer` | Data for a peer that already expired. Normal right after a long idle period. |
| `queued_*`, `queue_oldest_ms` | Current backlog. It stays small, because data older than `--max-delay-ms` is dropped. |
| `ping_rtt_ms`, `ping_rtt_max_ms` | RTT of the in-band ping through the whole path, including queues and the transport. This is the latency users feel. |
| `tcp_rtt_ms` | The kernel's RTT estimate for the relay's own TCP connection. Behind SSH or Cloak this is only the local hop. |
| `retrans` | Retransmissions on the relay's TCP connection in the interval. |

## Debug logging

Set `RUST_LOG=wg_tcp_relay=debug`: in Docker in `environment:`, with systemd in the env file.
It adds:

- a `link` line every keepalive with the kernel's `TCP_INFO` (cwnd, unacked, `notsent_bytes`,
  retransmits, delivery rate) and the queue depth by kind;
- every WireGuard handshake, cookie and keepalive in both directions, so you can see whether
  a peer that "does not connect" reaches the relay at all, and whether the server answers;
- connection setup timings, reconnect backoff, the UDP buffer sizes the kernel actually
  granted, and TCP writes blocked for more than 1 s.

`RUST_LOG=wg_tcp_relay=trace` logs every packet and every drop with its reason. It is very
verbose; use it on a test setup.

## Typical problems

**`connection failed: tcp connect: Connection refused`**

Nothing listens on `--connect`. Check that the tunnel is up (`systemctl status
  wg-tcp-relay-tunnel`, `ss -tlnp | grep 5000`).

**`relay handshake timed out`**

Something listens on the port, but it is not a `wg-tcp-relay` server, or the transport
  forwards it to the wrong place.

**Connected, but peers do not get a handshake**

With `debug`, look for `wireguard control to server` (the relay forwards the peer's
  handshake) and `wireguard control from server` (WireGuard answers). If there is no answer,
  the server cannot reach `--target`: look for `WireGuard server unreachable`, and check the
  address and port that WireGuard listens on.

**Reconnects every ~15 s with `nothing received`**

The transport is up but does not deliver data. Check the SSH or Cloak logs and the
  transport's own keepalive.

**`ssh: ... remote port forwarding failed` or the tunnel is up without the forward**

A stale session holds the port. Use `ExitOnForwardFailure=yes` and set
  `ClientAliveInterval` in sshd. See [SSH](ssh.md#server-side-sshd).

**Short `ping_rtt_max_ms` spikes of seconds, with an empty queue**

Latency comes from below the relay: retransmissions on the long-haul TCP connection of the
  transport. Try [`--max-rate-mbit`](tuning.md#--max-rate-mbit) and check that BBR is the
  system default.

**`cannot set tcp congestion control`**

The `tcp_bbr` module is not loaded, or an unprivileged process may not select it. See
  [tuning](tuning.md#--congestion).
