# Installing with systemd

This guide installs `wg-tcp-relay` as a plain binary managed by systemd, without Docker. It
uses the same layout as the [quick start](../README.md#quick-start):

- **exit server**: WireGuard on the host (`51820/udp`), and the relay server on
  `127.0.0.1:5000`;
- **entry server**: the relay client on `51820/udp` for the peers, and an SSH tunnel from
  `127.0.0.1:5000` to the exit server.

Files used below are in [`contrib/systemd`](../contrib/systemd).

## 1. Install the binary (both servers)

Releases have static binaries with no dependencies, for `x86_64` and `aarch64`:

```sh
V=0.1.1                      # see https://github.com/navrocky/wg-tcp-relay/releases
ARCH=$(uname -m)             # x86_64 or aarch64
BASE=https://github.com/navrocky/wg-tcp-relay/releases/download/v$V
cd /tmp
curl -fsSLO "$BASE/wg-tcp-relay-$V-linux-$ARCH"
curl -fsSLO "$BASE/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
install -m 755 "wg-tcp-relay-$V-linux-$ARCH" /usr/local/bin/wg-tcp-relay
wg-tcp-relay --version
```

To upgrade later, repeat this step with the new version and restart the services.

Kernel settings, recommended on both servers:

```sh
modprobe tcp_bbr && echo tcp_bbr > /etc/modules-load.d/bbr.conf
curl -fsSL -o /etc/sysctl.d/90-wg-tcp-relay.conf \
  https://raw.githubusercontent.com/navrocky/wg-tcp-relay/main/contrib/systemd/90-wg-tcp-relay.conf
sysctl --system
```

[Tuning](tuning.md) explains these settings.

## 2. Exit server

Install and start the relay server:

```sh
RAW=https://raw.githubusercontent.com/navrocky/wg-tcp-relay/main/contrib/systemd
curl -fsSL -o /etc/systemd/system/wg-tcp-relay-server.service "$RAW/wg-tcp-relay-server.service"
systemctl daemon-reload
systemctl enable --now wg-tcp-relay-server
journalctl -u wg-tcp-relay-server -f      # expect "server started" and "target resolves"
```

The defaults are `--listen 127.0.0.1:5000 --target 127.0.0.1:51820`. If yours differ, change
them in `/etc/wg-tcp-relay/server.env`, not in the unit:

```sh
mkdir -p /etc/wg-tcp-relay
cat > /etc/wg-tcp-relay/server.env <<'CONF'
WGTR_TARGET=127.0.0.1:51820
# WGTR_MAX_RATE_MBIT=0
CONF
systemctl restart wg-tcp-relay-server
```

Then create the SSH user for the tunnel. It may only forward to the relay, and cannot get a
shell:

```sh
useradd -m -s /usr/sbin/nologin tunnel
install -d -m 700 -o tunnel -g tunnel ~tunnel/.ssh
# The key is generated on the entry server in step 3.
echo 'restrict,port-forwarding,permitopen="127.0.0.1:5000" ssh-ed25519 AAAA... entry' \
  > ~tunnel/.ssh/authorized_keys
chown tunnel:tunnel ~tunnel/.ssh/authorized_keys
```

Make sure sshd detects dead clients. See [SSH: server side](ssh.md#server-side-sshd).

## 3. Entry server

Create the tunnel key and pin the exit server's host key:

```sh
mkdir -p /etc/wg-tcp-relay && chmod 700 /etc/wg-tcp-relay
ssh-keygen -t ed25519 -N '' -C entry -f /etc/wg-tcp-relay/id_ed25519
cat /etc/wg-tcp-relay/id_ed25519.pub     # put this into ~tunnel/.ssh/authorized_keys on the exit server
ssh-keyscan -p 22 exit.example.com > /etc/wg-tcp-relay/known_hosts
ssh-keygen -lf /etc/wg-tcp-relay/known_hosts   # compare with the exit server's fingerprint
```

Install the tunnel and the relay client:

```sh
RAW=https://raw.githubusercontent.com/navrocky/wg-tcp-relay/main/contrib/systemd
curl -fsSL -o /etc/systemd/system/wg-tcp-relay-tunnel.service "$RAW/wg-tcp-relay-tunnel.service"
curl -fsSL -o /etc/systemd/system/wg-tcp-relay-client.service "$RAW/wg-tcp-relay-client.service"
sed -i 's/exit.example.com/YOUR.EXIT.HOST/' /etc/systemd/system/wg-tcp-relay-tunnel.service
systemctl daemon-reload
systemctl enable --now wg-tcp-relay-tunnel wg-tcp-relay-client
```

If sshd on the exit server is not on port 22, add `-p PORT` to `ExecStart` in the tunnel
unit, and use the same port with `ssh-keyscan`.

Open the peers' port, for example `ufw allow 51820/udp`.

## 4. Check

```sh
# entry server
systemctl status wg-tcp-relay-tunnel wg-tcp-relay-client
journalctl -u wg-tcp-relay-client -f    # expect "connected", then "new peer" as peers come in
```

Point a peer's `Endpoint` at `entry.example.com:51820`. On the exit server, `wg show` should
show a fresh `latest handshake` for it.

After a minute both sides log `stats` and `link stats` lines.
[Troubleshooting](troubleshooting.md) explains them.

## Notes

- The relay units run as a transient unprivileged user (`DynamicUser=yes`) with no
  capabilities and a read-only filesystem. They only need network access.
- `--congestion bbr` (the default) is applied without privileges only when BBR is the
  system default or is listed in `net.ipv4.tcp_allowed_congestion_control`. The sysctl file
  above takes care of that. Otherwise the relay logs a warning and uses the system default.
- The client unit starts after the tunnel unit, but they do not depend on each other: the
  relay keeps reconnecting until the tunnel is up.
- Logs go to the journal. For more detail, add `RUST_LOG=wg_tcp_relay=debug` to the
  service's env file.
