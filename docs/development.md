# Development

## Building

```sh
cargo build --release              # target/release/wg-tcp-relay
docker build -t wg-tcp-relay:dev . # static musl binary in a distroless image (~10 MB)
```

The Docker build compiles on `rust:alpine`, which targets `*-linux-musl` and links statically,
and copies the binary into `gcr.io/distroless/static-debian12:nonroot`. That image has no
shell and no libc, and runs as uid 65532. To get a static binary without Docker:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Tests

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test        # unit tests and in-process relay tests (tests/relay.rs)
```

## End-to-end bench

`tests/e2e` runs real WireGuard through the relay over an emulated slow, high-latency link,
in network namespaces inside one privileged container:

```
[peer] wg0 ──udp──► [relayc] client ══ tcp: netem delay/loss + tbf rate/buffer ══ [relays] server ──udp──► [wgsrv] wg0
```

```sh
docker build -t wg-tcp-relay:dev .
docker build -t wg-tcp-relay-e2e tests/e2e
docker run --rm --privileged wg-tcp-relay-e2e
docker run --rm --privileged -e RATE=50mbit -e DELAY_MS=40 -e LOSS=1% wg-tcp-relay-e2e
```

It runs an iperf3 upload, a download, and 8 parallel download streams through WireGuard while
pinging through the tunnel. It fails if the pings show a gap longer than `MAX_GAP_SECS`
(3 s), or if there is more than one second without throughput.

| Variable | Default | Meaning |
|---|---|---|
| `DELAY_MS` | `90` | one-way delay (RTT = 2×) |
| `RATE` | `20mbit` | link rate |
| `LOSS` | `0%` | random loss |
| `BUFFER_MS` | `50` | bottleneck buffer |
| `CONGESTION` | `bbr` | relay congestion control; empty = system default |
| `MAX_RATE_MBIT` | `18` | relay send rate cap; 0 = none |
| `DURATION` | `20` | seconds per iperf3 run |

The host needs the `wireguard`, `sch_netem` and `sch_tbf` kernel modules, and `tcp_bbr` for
BBR. CI runs the bench on every push.

## Continuous integration

- `.github/workflows/ci.yml`: fmt, clippy, tests, then the end-to-end bench.
- `.github/workflows/docker.yml`:
  - builds `linux/amd64` and `linux/arm64` on native runners and publishes a multi-arch image
    to Docker Hub;
  - on `vX.Y.Z` tags, also creates a GitHub release with notes from `CHANGELOG.md` and static
    binaries;
  - keeps the Docker Hub description in sync with the README.

## Versions and releases

The project follows [Semantic Versioning](https://semver.org); changes are listed in
[CHANGELOG.md](../CHANGELOG.md). The wire protocol, command-line options and `WGTR_*`
variables are the public interface: breaking changes to them need a new major version.

Docker image tags:

| Tag | What |
|---|---|
| `X.Y.Z` | exact release |
| `X.Y` | latest patch release of `X.Y` |
| `X` | latest release of major version `X`, recommended for deployments |
| `latest` | latest release |
| `edge` | current `main`, not a release |
| `sha-<commit>` | a specific `main` commit |

Every [GitHub release](https://github.com/navrocky/wg-tcp-relay/releases) has static Linux
binaries (`x86_64`, `aarch64`) and `SHA256SUMS`.

To make a release:

1. Set `version` in `Cargo.toml` and run `cargo check`, which updates `Cargo.lock`.
2. In `CHANGELOG.md`, rename `[Unreleased]` to `[X.Y.Z] - YYYY-MM-DD`, add an empty
   `[Unreleased]` above it, and update the links at the bottom.
3. Commit, then run `git tag -a vX.Y.Z -m vX.Y.Z && git push origin main vX.Y.Z`.

CI refuses a tag that does not match `Cargo.toml` or has no section in `CHANGELOG.md`. A tag
like `v1.2.0-rc.1` becomes a pre-release.
