# Configuration

```
wg-tcp-relay client --listen 0.0.0.0:51820 --connect 127.0.0.1:5000 [OPTIONS]
wg-tcp-relay server --listen 127.0.0.1:5000 --target 127.0.0.1:51820 [OPTIONS]
```

- `client` runs on the entry server. It receives UDP from WireGuard peers and sends it over
  TCP.
- `server` runs on the exit server. It accepts the TCP connection and talks UDP to the
  WireGuard server, with one UDP socket per peer, so WireGuard sees each peer as a separate
  endpoint.

Every option can also be set with an environment variable. That is handy in Docker
(`environment:`) and in systemd env files.

## Addresses

| Option | Env | Default | Description |
|---|---|---|---|
| `client --listen` | `WGTR_LISTEN` | `0.0.0.0:51820` | UDP address WireGuard peers connect to |
| `client --connect` | `WGTR_CONNECT` | (required) | relay server TCP address, `host:port` |
| `server --listen` | `WGTR_LISTEN` | `0.0.0.0:5000` | TCP address for relay clients; use `127.0.0.1:5000` behind SSH or Cloak |
| `server --target` | `WGTR_TARGET` | (required) | WireGuard server UDP address, `host:port`; resolved for every new peer, so a Docker service name works |

## Tuning

These options apply to the side that **sends**. Set them on both sides: the client sends the
peers' uploads, the server sends their downloads. [Tuning](tuning.md) explains when to change
them.

| Option | Env | Default | Description |
|---|---|---|---|
| `--max-rate-mbit` | `WGTR_MAX_RATE_MBIT` | `0` (off) | cap on the relay's TCP sending rate, Mbit/s |
| `--congestion` | `WGTR_CONGESTION` | `bbr` | TCP congestion control for the relay's connection; empty = system default |
| `--max-delay-ms` | `WGTR_MAX_DELAY_MS` | `150` | drop data that waited longer than this in the queue |
| `--peer-queue-kb` | `WGTR_PEER_QUEUE_KB` | `256` | queue limit per peer, KiB |

## Liveness

| Option | Env | Default | Description |
|---|---|---|---|
| `--keepalive-secs` | `WGTR_KEEPALIVE_SECS` | `5` | in-band ping interval |
| `--dead-after-secs` | `WGTR_DEAD_AFTER_SECS` | `15` | reconnect when nothing was received for this long |
| `--peer-timeout-secs` | `WGTR_PEER_TIMEOUT_SECS` | `180` | forget peers that were idle this long |

## Logging

| Option | Env | Default | Description |
|---|---|---|---|
| `--stats-secs` | `WGTR_STATS_SECS` | `60` | interval of `stats` and `link stats` lines, 0 = off |
| | `RUST_LOG` | `info` | `info`, `debug`, `trace`, or per module, e.g. `wg_tcp_relay=debug` |

[Troubleshooting](troubleshooting.md) explains what the log lines mean.

## Compatibility

Both sides must speak the same wire protocol. Within a major version (1.x) the protocol and
options stay compatible, so a 1.2 client works with a 1.0 server. A new major version may
change them; the [changelog](../CHANGELOG.md) always says so.
