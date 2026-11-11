# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0.0,
minor versions may contain breaking changes to the wire protocol or command-line options;
such changes are always listed under **Changed** or **Removed**.

## [Unreleased]

## [0.1.1] - 2026-11-11

### Fixed

- The server kept queuing replies for a session whose client had gone (for example after
  the client restarted with a new session). Nobody sent them, so they only took memory until
  the session expired, and the stats showed a growing queue (`queued_frames`,
  `queue_oldest_ms` of minutes). A queue now accepts traffic only while a TCP connection is
  draining it, and is cleared when the connection ends, on both client and server.

### Added

- `drop_no_conn` stats counter: traffic dropped because no connection was up.

## [0.1.0] - 2026-11-11

### Added

- `client` and `server` modes: many WireGuard peers carried over one TCP stream (SSH port
  forward, Cloak, or any TCP transport).
- Wire protocol v1: handshake with a session id, framed datagrams with peer ids, in-band
  ping/pong, peer close.
- Sessions survive TCP reconnects: the server keeps per-peer UDP sockets, so WireGuard
  endpoints stay stable.
- Non-blocking design: UDP sends never wait (full socket = drop), bounded per-peer queues,
  data older than `--max-delay-ms` is dropped, round-robin fairness between peers.
- WireGuard awareness: handshakes, cookie replies and keepalives bypass the data queue;
  datagrams that are not well-formed WireGuard messages are dropped.
- `--max-rate-mbit`: pacing cap on the relay's TCP sending rate, to avoid outer
  retransmissions.
- `--congestion` (default `bbr`): TCP congestion control for the relay connection.
- `TCP_NOTSENT_LOWAT`, keepalive and large UDP buffers on relay sockets.
- Logs: periodic traffic and drop stats, link stats with in-band RTT and retransmissions;
  `debug` adds kernel `TCP_INFO`, queue depth and WireGuard handshake events; `trace` logs
  every packet.
- Every option can be set from `WGTR_*` environment variables; graceful shutdown on SIGTERM.
- Static musl binary, distroless multi-arch Docker image (`linux/amd64`, `linux/arm64`).
- End-to-end bench with real WireGuard over an emulated slow, high-latency link (`tests/e2e`).

[Unreleased]: https://github.com/navrocky/wg-tcp-relay/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/navrocky/wg-tcp-relay/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/navrocky/wg-tcp-relay/releases/tag/v0.1.0
