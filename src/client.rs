//! Client side: listens for WireGuard peers on UDP and carries them over one TCP stream.

use crate::common::{self, Link, Snapshot, Stats, Tuning, inc, now_ms};
use crate::proto::{self, Frame};
use crate::queue::Shared;
use crate::{net, wg};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

pub struct Config {
    pub listen: SocketAddr,
    pub connect: String,
    pub tune: Tuning,
}

struct Peer {
    addr: SocketAddr,
    last_ms: u64,
    since: Instant,
}

#[derive(Default)]
struct Peers {
    by_addr: HashMap<SocketAddr, u32>,
    by_id: HashMap<u32, Peer>,
    next: u32,
}

impl Peers {
    fn id_for(&mut self, addr: SocketAddr) -> u32 {
        let now = now_ms();
        if let Some(&id) = self.by_addr.get(&addr) {
            self.by_id.get_mut(&id).unwrap().last_ms = now;
            return id;
        }
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.by_addr.insert(addr, id);
        self.by_id.insert(
            id,
            Peer {
                addr,
                last_ms: now,
                since: Instant::now(),
            },
        );
        info!(peer = id, %addr, total = self.by_id.len(), "new peer");
        id
    }

    fn addr_of(&mut self, id: u32) -> Option<SocketAddr> {
        let p = self.by_id.get_mut(&id)?;
        p.last_ms = now_ms();
        Some(p.addr)
    }

    fn expire(&mut self, idle: Duration) -> Vec<u32> {
        let now = now_ms();
        let dead: Vec<u32> = self
            .by_id
            .iter()
            .filter(|(_, p)| now.saturating_sub(p.last_ms) > idle.as_millis() as u64)
            .map(|(id, _)| *id)
            .collect();
        for id in &dead {
            if let Some(p) = self.by_id.remove(id) {
                self.by_addr.remove(&p.addr);
                info!(
                    peer = id,
                    addr = %p.addr,
                    lived_s = p.since.elapsed().as_secs(),
                    remaining = self.by_id.len(),
                    "peer expired"
                );
            }
        }
        dead
    }
}

struct Ctx {
    cfg: Config,
    udp: UdpSocket,
    q: Shared,
    peers: Mutex<Peers>,
    stats: Stats,
    session: u64,
}

pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let udp = net::udp_socket(cfg.listen)?;
    let session = common::random_u64();
    log_socket_setup(&udp);
    info!(
        listen = %cfg.listen,
        connect = %cfg.connect,
        session = format!("{session:016x}"),
        tuning = ?cfg.tune,
        "client started"
    );
    let ctx = Arc::new(Ctx {
        q: Shared::new(cfg.tune.limits),
        cfg,
        udp,
        peers: Mutex::default(),
        stats: Stats::default(),
        session,
    });
    tokio::select! {
        r = udp_rx(&ctx) => r,
        _ = reaper(&ctx) => Ok(()),
        _ = stats(&ctx) => Ok(()),
        _ = connection_loop(&ctx) => Ok(()),
    }
}

fn log_socket_setup(udp: &UdpSocket) {
    let s = socket2::SockRef::from(udp);
    debug!(
        rcvbuf = s.recv_buffer_size().ok(),
        sndbuf = s.send_buffer_size().ok(),
        wanted = net::UDP_BUFFER,
        "udp socket buffers (kernel caps them at net.core.rmem_max/wmem_max, x2)"
    );
}

async fn udp_rx(ctx: &Ctx) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = match ctx.udp.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(e) => {
                warn!("udp recv: {e}");
                continue;
            }
        };
        let p = &buf[..n];
        let Some(kind) = wg::classify(p) else {
            inc(&ctx.stats.not_wg);
            debug!(%from, len = n, first_byte = p.first().copied(), "drop: not a WireGuard message");
            continue;
        };
        inc(&ctx.stats.udp_in);
        let id = ctx.peers.lock().unwrap().id_for(from);
        trace!(peer = id, %from, len = n, ?kind, "udp -> tcp");
        if kind == wg::Kind::Control {
            debug!(peer = id, %from, msg = wg::describe(p), "wireguard control from peer");
        }
        ctx.q.push_wg(id, kind, proto::encode_data(id, p));
    }
}

async fn reaper(ctx: &Ctx) {
    let mut iv = tokio::time::interval(Duration::from_secs(10));
    loop {
        iv.tick().await;
        let dead = ctx.peers.lock().unwrap().expire(ctx.cfg.tune.peer_timeout);
        for id in dead {
            ctx.q.remove_peer(id);
            ctx.q.push_session(proto::encode_close(id));
        }
    }
}

async fn stats(ctx: &Ctx) {
    let every = ctx.cfg.tune.stats_every;
    if every.is_zero() {
        return std::future::pending().await;
    }
    let mut prev = Snapshot::take(&ctx.stats, &[&ctx.q]);
    let mut iv = tokio::time::interval(every);
    iv.tick().await;
    loop {
        iv.tick().await;
        let cur = Snapshot::take(&ctx.stats, &[&ctx.q]);
        let peers = ctx.peers.lock().unwrap().by_id.len();
        cur.log_delta(&prev, peers, &[&ctx.q]);
        prev = cur;
    }
}

async fn connection_loop(ctx: &Ctx) {
    let mut backoff = Duration::from_millis(500);
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        let started = Instant::now();
        match connect_once(ctx, attempt).await {
            Ok(()) => info!(server = %ctx.cfg.connect, "connection closed"),
            Err(e) => warn!(
                server = %ctx.cfg.connect,
                attempt,
                lived_ms = started.elapsed().as_millis() as u64,
                "connection failed: {e:#}"
            ),
        }
        // Reset backoff after a connection that lived for a while.
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_millis(500);
        }
        debug!(
            backoff_ms = backoff.as_millis() as u64,
            "reconnecting after backoff"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn connect_once(ctx: &Ctx, attempt: u64) -> anyhow::Result<()> {
    debug!(server = %ctx.cfg.connect, attempt, "connecting");
    let t = Instant::now();
    let mut s = timeout(
        Duration::from_secs(10),
        TcpStream::connect(&ctx.cfg.connect),
    )
    .await
    .map_err(|_| anyhow::anyhow!("tcp connect timed out after 10s"))?
    .map_err(|e| anyhow::anyhow!("tcp connect: {e}"))?;
    let connect_ms = t.elapsed().as_millis() as u64;
    net::setup_tcp(
        &s,
        ctx.cfg.tune.congestion.as_deref(),
        ctx.cfg.tune.max_rate_mbit,
    );
    let local = s.local_addr().ok();
    let peer = s.peer_addr().ok();
    let t = Instant::now();
    timeout(
        Duration::from_secs(10),
        proto::client_handshake(&mut s, ctx.session),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "relay handshake timed out after 10s (is a wg-tcp-relay server listening there?)"
        )
    })?
    .map_err(|e| anyhow::anyhow!("relay handshake: {e}"))?;
    info!(
        server = %ctx.cfg.connect,
        ?local,
        ?peer,
        attempt,
        connect_ms,
        handshake_ms = t.elapsed().as_millis() as u64,
        "connected"
    );
    let link = Link::new(ctx.cfg.connect.clone(), &s);
    if let Some(i) = link.tcp() {
        debug!(?i, "tcp state after connect");
    }
    // While attached, the queue accepts traffic; detached (and cleared) when this ends.
    let _attached = ctx.q.attach();
    let (rd, wr) = s.into_split();
    let r = tokio::select! {
        r = common::writer(wr, &ctx.q, &link) => r.map_err(|e| e.context("tcp write")),
        r = reader(ctx, rd, &link) => r.map_err(|e| e.context("tcp read")),
        r = common::keepalive(&ctx.q, &link, &ctx.cfg.tune) => r,
    };
    info!(uptime_s = link.uptime().as_secs(), "connection ended");
    r
}

async fn reader(ctx: &Ctx, rd: tokio::net::tcp::OwnedReadHalf, link: &Link) -> anyhow::Result<()> {
    let mut rd = BufReader::with_capacity(256 * 1024, rd);
    let mut buf = Vec::with_capacity(65536);
    loop {
        let f = proto::read_frame(&mut rd, &mut buf).await?;
        link.on_rx();
        match f {
            Frame::Data { peer, payload } => {
                let Some(addr) = ctx.peers.lock().unwrap().addr_of(peer) else {
                    inc(&ctx.stats.unknown_peer);
                    debug!(
                        peer,
                        len = payload.len(),
                        "drop: data for unknown (expired) peer"
                    );
                    continue;
                };
                trace!(peer, %addr, len = payload.len(), "tcp -> udp");
                if payload.len() < 200 && wg::classify(payload) == Some(wg::Kind::Control) {
                    debug!(peer, %addr, msg = wg::describe(payload), "wireguard control to peer");
                }
                // Never wait for the UDP socket: a full socket means drop, like any router would.
                match net::send_nowait(&ctx.udp, payload, Some(addr)) {
                    Ok(_) => inc(&ctx.stats.udp_out),
                    Err(e) => {
                        inc(&ctx.stats.udp_full);
                        trace!(peer, %addr, "drop: udp send: {e}");
                    }
                }
            }
            Frame::Ping(n) => ctx.q.push_session(proto::encode_pong(n)),
            Frame::Pong(sent) => link.on_pong(sent),
            Frame::Close(peer) => debug!(peer, "unexpected CLOSE from server, ignored"),
        }
    }
}
