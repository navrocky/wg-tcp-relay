# wg-tcp-relay

WireGuard over TCP that does not stall.

`wg-tcp-relay` carries the traffic of many WireGuard peers over **one TCP connection**, such as
an SSH port forward or a [Cloak](docs/cloak.md) tunnel. It is meant for networks where UDP
between your servers is blocked or throttled. It understands WireGuard, and it follows one
rule: **under load, drop packets; never queue them for seconds and never block.**

```
 WireGuard peers ──UDP──► [ entry server ]  ═══ TCP (SSH, Cloak, ...) ═══►  [ exit server ] ──UDP──► WireGuard
 (phones, routers)       wg-tcp-relay client                               wg-tcp-relay server       (on the host)
                         :51820/udp                                        127.0.0.1:5000/tcp        :51820/udp
```

Peers keep their WireGuard configs: only the `Endpoint` changes, from the exit server to the
entry server.

## Why

Generic UDP-over-TCP tunnels tend to fail with WireGuard in two ways:

- **Stalls.** One slow flow blocks the others, and "the VPN hangs until I reconnect".
- **Bufferbloat.** TCP queues packets instead of losing them, so a single speedtest adds
  seconds of latency for every user of the tunnel.

`wg-tcp-relay` avoids both:

- it never waits on a UDP socket;
- each peer gets a small queue, and packets that wait too long are dropped;
- WireGuard handshakes and keepalives skip the queue;
- peers get the link in turns (round-robin);
- sessions survive reconnects of the TCP connection.

[How it compares to wstunnel, udp2raw, udp-over-tcp and others](docs/alternatives.md).

## Quick start

The example uses an SSH port forward as the transport and assumes WireGuard runs on the exit
server's host, listening on `51820/udp`.

**1. Exit server** (where WireGuard runs). Start the relay server on localhost:

```yaml
# docker-compose.yml
services:
  wg-tcp-relay:
    image: navrocky/wg-tcp-relay:0.1
    command: server --listen 127.0.0.1:5000 --target 127.0.0.1:51820
    network_mode: host
    restart: unless-stopped
```

Then create a user that may only forward to the relay:

```sh
useradd -m -s /usr/sbin/nologin tunnel
install -d -m 700 -o tunnel -g tunnel ~tunnel/.ssh
echo 'restrict,port-forwarding,permitopen="127.0.0.1:5000" ssh-ed25519 AAAA... entry' \
  > ~tunnel/.ssh/authorized_keys
chown tunnel:tunnel ~tunnel/.ssh/authorized_keys
```

**2. Entry server** (the address peers connect to). Open the SSH tunnel as a systemd service:

```ini
# /etc/systemd/system/wg-tcp-relay-tunnel.service
[Unit]
Description=SSH tunnel for wg-tcp-relay
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/bin/ssh -N -i /etc/wg-tcp-relay/id_ed25519 \
  -o ServerAliveInterval=10 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
  -o ConnectTimeout=15 -o UserKnownHostsFile=/etc/wg-tcp-relay/known_hosts \
  -L 127.0.0.1:5000:127.0.0.1:5000 tunnel@exit.example.com
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

```sh
mkdir -p /etc/wg-tcp-relay
ssh-keygen -t ed25519 -N '' -C entry -f /etc/wg-tcp-relay/id_ed25519   # this public key goes to step 1
ssh-keyscan exit.example.com > /etc/wg-tcp-relay/known_hosts            # check the fingerprint
systemctl enable --now wg-tcp-relay-tunnel
```

Then start the relay client:

```yaml
# docker-compose.yml
services:
  wg-tcp-relay:
    image: navrocky/wg-tcp-relay:0.1
    command: client --listen 0.0.0.0:51820 --connect 127.0.0.1:5000
    network_mode: host
    restart: unless-stopped
```

**3. Peers.** Set `Endpoint = entry.example.com:51820` and allow `51820/udp` in the entry
server's firewall. Nothing else changes.

Without Docker, see [installing with systemd](docs/systemd.md). Static binaries for x86_64 and
aarch64 are attached to every [release](https://github.com/navrocky/wg-tcp-relay/releases).

## Documentation

- [Installing with systemd](docs/systemd.md): binary, units, sysctls, no Docker
- [SSH transport](docs/ssh.md): what each option does, sshd settings, reverse (`-R`) setup
- [Cloak transport](docs/cloak.md): TCP disguised as TLS, for networks with DPI
- [Configuration](docs/configuration.md): all options and environment variables
- [Tuning and benchmarks](docs/tuning.md): `--max-rate-mbit`, congestion control, kernel settings
- [Troubleshooting](docs/troubleshooting.md): reading the logs, typical problems
- [Alternatives](docs/alternatives.md): wstunnel, udp2raw, udp-over-tcp, Cloak, and others
- [Development](docs/development.md): building, tests, the end-to-end bench, releases
- [Changelog](CHANGELOG.md)

## License

MIT
