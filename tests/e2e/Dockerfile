# Test bench: real WireGuard through wg-tcp-relay over an emulated slow, high-latency link.
ARG RELAY_IMAGE=wg-tcp-relay:dev
FROM ${RELAY_IMAGE} AS relay

FROM alpine:3
RUN apk add --no-cache bash wireguard-tools-wg iproute2 iproute2-tc iperf3 iputils jq coreutils kmod
COPY --from=relay /wg-tcp-relay /usr/local/bin/wg-tcp-relay
COPY run.sh /run.sh
ENTRYPOINT ["bash", "/run.sh"]
