#!/usr/bin/env bash
# Topology (each box is a network namespace):
#
#  [peer] wg0 10.0.0.2 --udp--> [relayc] wg-tcp-relay client
#                                   |  tcp over "wan": netem delay + tbf rate limit
#                               [relays] wg-tcp-relay server --udp--> [wgsrv] wg0 10.0.0.1
#
# Env: DELAY_MS (one way, default 90), RATE (20mbit), LOSS (0%), BUFFER_MS (bottleneck buffer, 50),
#      CONGESTION (relay TCP congestion control, bbr; empty = system default),
#      MAX_RATE_MBIT (relay send rate cap, 0 = none; default 18 = 90% of RATE), DURATION (20)
set -euo pipefail
DELAY_MS=${DELAY_MS:-90} RATE=${RATE:-20mbit} LOSS=${LOSS:-0%} DURATION=${DURATION:-20}
BUFFER_MS=${BUFFER_MS:-50} CONGESTION=${CONGESTION-bbr} MAX_RATE_MBIT=${MAX_RATE_MBIT:-18}
MAX_GAP_SECS=${MAX_GAP_SECS:-3}

ns() { ip netns exec "$@"; }
link() { # link nsA ifA ipA nsB ifB ipB
  ip link add "$2" netns "$1" type veth peer name "$5" netns "$4"
  ns "$1" ip addr add "$3/24" dev "$2"; ns "$1" ip link set "$2" up
  ns "$4" ip addr add "$6/24" dev "$5"; ns "$4" ip link set "$5" up
}
shape() { # ns dev: netem delays (and loses), tbf below it is the bottleneck with a
  # router-like buffer of BUFFER_MS (a bufferbloated link would have seconds here).
  ns "$1" tc qdisc add dev "$2" root handle 1: netem delay "${DELAY_MS}ms" loss "$LOSS" limit 10000
  ns "$1" tc qdisc add dev "$2" parent 1:1 handle 10: tbf rate "$RATE" burst 32kbit latency "${BUFFER_MS}ms"
}

for n in peer relayc relays wgsrv; do ip netns add $n; ns $n ip link set lo up; done
link peer   eth0 10.1.0.2  relayc lan  10.1.0.1
link relayc wan  10.2.0.1  relays wan  10.2.0.2
link relays lan  10.3.0.1  wgsrv  eth0 10.3.0.2
shape relayc wan; shape relays wan

wg genkey > /tmp/k1; wg pubkey < /tmp/k1 > /tmp/p1
wg genkey > /tmp/k2; wg pubkey < /tmp/k2 > /tmp/p2
ns wgsrv ip link add wg0 type wireguard
ns wgsrv wg set wg0 private-key /tmp/k2 listen-port 51820 peer "$(cat /tmp/p1)" allowed-ips 10.0.0.2/32
ns wgsrv ip addr add 10.0.0.1/24 dev wg0; ns wgsrv ip link set wg0 mtu 1420 up
ns peer ip link add wg0 type wireguard
ns peer wg set wg0 private-key /tmp/k1 peer "$(cat /tmp/p2)" allowed-ips 10.0.0.0/24 endpoint 10.1.0.1:51820 persistent-keepalive 25
ns peer ip addr add 10.0.0.2/24 dev wg0; ns peer ip link set wg0 mtu 1420 up

modprobe "tcp_${CONGESTION:-cubic}" 2>/dev/null || true
export RUST_LOG=${RUST_LOG:-info} WGTR_STATS_SECS=5 WGTR_CONGESTION="$CONGESTION" WGTR_MAX_RATE_MBIT="$MAX_RATE_MBIT"
ns relays wg-tcp-relay server --listen 10.2.0.2:5000 --target 10.3.0.2:51820 >/tmp/server.log 2>&1 &
ns relayc wg-tcp-relay client --listen 10.1.0.1:51820 --connect 10.2.0.2:5000 >/tmp/client.log 2>&1 &
ns wgsrv iperf3 -s -D

fail() { echo "FAIL: $*"; echo "--- client log"; tail -20 /tmp/client.log; echo "--- server log"; tail -20 /tmp/server.log; exit 1; }

echo "== link: rtt $((2*DELAY_MS))ms, rate $RATE, loss $LOSS, bottleneck buffer ${BUFFER_MS}ms, relay congestion control: ${CONGESTION:-system default}, relay rate cap: ${MAX_RATE_MBIT} Mbit/s"
ns peer ping -c 3 -W 3 10.0.0.1 >/dev/null || ns peer ping -c 3 -W 3 10.0.0.1 >/dev/null || fail "no connectivity through tunnel"
echo "== idle ping: $(ns peer ping -c 5 -i 0.2 -q 10.0.0.1 | tail -1)"

# Max gap between consecutive ping replies (a stall shows up as a long gap).
max_gap() { awk -F'[ =]' '/bytes from/ { t=$1; gsub(/[\[\]]/,"",t); if (p) { g=t-p; if (g>m) m=g } p=t } END { printf "%.2f", m }' "$1"; }

run_load() { # name iperf-args...
  local name=$1; shift
  ns peer ping -D -i 0.2 -W 1 -w "$((DURATION + 1))" 10.0.0.1 >/tmp/ping-$name.log 2>&1 & local pp=$!
  ns peer iperf3 -c 10.0.0.1 -t "$DURATION" -J "$@" >/tmp/iperf-$name.json || true
  wait $pp || true  # ping exits by its own deadline (-w)
  local mbps zero gap loss rtt
  mbps=$(jq '.end.sum_received.bits_per_second // 0 | ./1e6 | floor' /tmp/iperf-$name.json)
  zero=$(jq '[.intervals[]?.sum.bits_per_second | select(. < 1000)] | length' /tmp/iperf-$name.json)
  gap=$(max_gap /tmp/ping-$name.log)
  loss=$(grep -oE '[0-9.]+% packet loss' /tmp/ping-$name.log || echo "?")
  rtt=$(grep -oE 'rtt [^=]+= [0-9./]+' /tmp/ping-$name.log | awk '{print $NF}' || echo "?")
  local p95; p95=$(grep -oE 'time=[0-9.]+' /tmp/ping-$name.log | cut -d= -f2 | sort -n | awk '{a[NR]=$1} END { if (NR) printf "%.0f", a[int(NR*0.95)>0?int(NR*0.95):1]; else print "?" }')
  echo "== $name: ${mbps} Mbit/s, zero-throughput seconds: $zero, ping under load: loss $loss, p95 ${p95} ms, min/avg/max/mdev $rtt ms, max gap ${gap}s"
  awk -v g="$gap" -v m="$MAX_GAP_SECS" 'BEGIN { exit !(g <= m) }' || fail "$name: tunnel stalled for ${gap}s"
  [ "$zero" -le 1 ] || fail "$name: $zero seconds without throughput"
}

run_load upload
run_load download -R
run_load parallel-8 -P 8 -R

ns peer ping -c 3 -W 2 10.0.0.1 >/dev/null || fail "tunnel dead after load"
echo "== relay stats (client):"; grep stats /tmp/client.log | tail -3
echo "== relay stats (server):"; grep stats /tmp/server.log | tail -3
echo "PASS"
