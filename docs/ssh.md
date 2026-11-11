# SSH transport

An SSH port forward is the simplest transport: sshd is already on the server, the traffic
looks like an ordinary SSH session, and it is encrypted and authenticated. This page covers
the details behind the [quick start](../README.md#quick-start).

## Forward tunnel (`-L`), the usual case

The entry server connects to the exit server and forwards its local `127.0.0.1:5000` to the
relay server on the exit server:

```sh
ssh -N \
  -i /etc/wg-tcp-relay/id_ed25519 \
  -o UserKnownHostsFile=/etc/wg-tcp-relay/known_hosts -o StrictHostKeyChecking=yes \
  -o BatchMode=yes \
  -o ServerAliveInterval=10 -o ServerAliveCountMax=3 \
  -o ExitOnForwardFailure=yes \
  -o ConnectTimeout=15 \
  -o Compression=no \
  -o IPQoS=lowdelay \
  -L 127.0.0.1:5000:127.0.0.1:5000 \
  tunnel@exit.example.com
```

Run it under something that restarts it: the
[systemd unit](../contrib/systemd/wg-tcp-relay-tunnel.service) with `Restart=always`, or a
container with `restart: always`. `autossh` is not needed.

| Option | Why |
|---|---|
| `-N` | Opens no session or shell, only the forward. |
| `ServerAliveInterval=10`, `ServerAliveCountMax=3` | Detects a dead connection in about 30 s, instead of minutes or hours of TCP timeouts. |
| `ExitOnForwardFailure=yes` | If the forward cannot be set up (the local port is busy), exit and retry. Without it, ssh stays connected **without** the forward, and the tunnel looks up while forwarding nothing. |
| `ConnectTimeout=15` | On some networks a share of new connections hangs; this retries quickly instead of waiting about 2 minutes for the kernel timeout. |
| `BatchMode=yes`, pinned `known_hosts` | Never waits for a password or a host key prompt, and never silently trusts a new host key. |
| `Compression=no` | WireGuard traffic is encrypted and does not compress: compression only adds CPU and latency. |
| `IPQoS=lowdelay` | Marks packets as interactive (DSCP), which some networks prioritize. |
| `-L 127.0.0.1:...` | Binds the forward to loopback: only the local relay client needs it. |

The relay does its own liveness check too: in-band pings every 5 s, reconnect after 15 s of
silence. A tunnel that is up but blackholes traffic is still detected.

### Use a dedicated connection

Do not add other forwards (web panels, proxies) to the same ssh process. All forwards of one
SSH connection share a single TCP stream. A bulk download in another forward delays
WireGuard behind it (head-of-line blocking), and the relay cannot drop those packets. Run a
separate ssh for anything else.

### Restricted user on the exit server

```sh
useradd -m -s /usr/sbin/nologin tunnel
install -d -m 700 -o tunnel -g tunnel ~tunnel/.ssh
echo 'restrict,port-forwarding,permitopen="127.0.0.1:5000" ssh-ed25519 AAAA... entry' \
  > ~tunnel/.ssh/authorized_keys
chown tunnel:tunnel ~tunnel/.ssh/authorized_keys
```

`restrict` disables everything (pty, agent and X11 forwarding, user rc), `port-forwarding`
enables forwarding back, and `permitopen` limits it to the relay's port. A leaked key gives
access to the relay port and nothing else.

## Server side (sshd)

On the server that accepts the tunnel, sshd must also detect dead clients. Otherwise, after a
network outage, the old session lives on for hours (until TCP keepalive gives up). With a
reverse tunnel, the stale session also keeps holding the forwarded port, so every reconnect
fails.

```
# /etc/ssh/sshd_config.d/wg-tcp-relay.conf
ClientAliveInterval 10
ClientAliveCountMax 3
AllowTcpForwarding yes
```

Apply it with `sshd -t && systemctl reload ssh` (or `sshd`). A reload does not drop existing
sessions.

## Reverse tunnel (`-R`)

Use it when the entry server cannot reach the exit server, but the exit server can reach the
entry server (for example, the exit server is behind NAT, or only outgoing connections from
it get through). The exit server opens the connection and publishes the relay on the entry
server:

```sh
# on the exit server
ssh -N \
  -o ServerAliveInterval=10 -o ServerAliveCountMax=3 \
  -o ExitOnForwardFailure=yes -o ConnectTimeout=15 -o Compression=no \
  -R 127.0.0.1:5000:127.0.0.1:5000 \
  tunnel@entry.example.com
```

The relay client on the entry server still connects to `127.0.0.1:5000`. On the **entry**
server:

- the `tunnel` user's `authorized_keys` uses `restrict,port-forwarding,permitlisten="127.0.0.1:5000"`
  (`permitlisten` is for `-R`, `permitopen` is for `-L`);
- `ClientAliveInterval` in sshd is essential here: a stale session holds port 5000, and with
  `ExitOnForwardFailure=yes` the new one keeps failing until the old one is gone;
- `GatewayPorts` is only needed if you bind the forward to a non-loopback address.

## Kernel settings

On both servers, see [tuning](tuning.md#kernel-settings): BBR, `tcp_notsent_lowat`, UDP
buffers. In Docker, `net.ipv4.tcp_notsent_lowat` is per network namespace. Set it with
`sysctls:` on the container that runs ssh, unless it uses `network_mode: host`.
