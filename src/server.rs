//! Server side: accepts relay clients over TCP and talks to the WireGuard server over UDP,
//! one UDP socket per relayed peer (so WireGuard sees each peer as a separate endpoint).
//!
//! A session (identified by the client's random id) outlives TCP connections: after a
//! reconnect the same per-peer sockets are reused, so WireGuard endpoints stay stable.

use crate::common::{self, Link, Snapshot, Stats, Tuning, idle_for, inc, now_ms};
use crate::proto::{self, Frame};
use crate::queue::Shared;
use crate::{net, wg};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::AbortHandle;
use tokio::time::timeout;
use tracing::{Instrument, debug, info, info_span, trace, warn};

pub struct Config {
    pub listen: SocketAddr,
    pub target: String,
    pub tune: Tuning,
}

struct PeerSock {
    sock: Arc<UdpSocket>,
    last_ms: Arc<AtomicU64>,
    task: AbortHandle,
    since: Instant,
}

impl Drop for PeerSock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Session {
    id: u64,
    q: Arc<Shared>,
    peers: Mutex<HashMap<u32, PeerSock>>,
    conn: Mutex<Option<AbortHandle>>,
    last_ms: AtomicU64,
    connections: AtomicU64,
}

impl Session {
    fn connected(&self) -> bool {
        self.conn
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    fn tag(&self) -> String {
        format!("{:016x}", self.id)
    }
}

struct Ctx {
    cfg: Config,
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    stats: Arc<Stats>,
}

pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(cfg.listen).await?;
    info!(listen = %cfg.listen, target = %cfg.target, tuning = ?cfg.tune, "server started");
    match net::resolve(&cfg.target).await {
        Ok(a) => info!(target = %cfg.target, resolved = %a, "target resolves"),
        Err(e) => {
            warn!(target = %cfg.target, "target does not resolve yet (will retry per peer): {e}")
        }
    }
    let ctx = Arc::new(Ctx {
        cfg,
        sessions: Mutex::default(),
        stats: Arc::default(),
    });
    let c = ctx.clone();
    tokio::spawn(async move { reaper(&c).await });
    let c = ctx.clone();
    tokio::spawn(async move { stats(&c).await });
    loop {
        let (s, from) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        debug!(%from, "tcp connection accepted");
        let ctx = ctx.clone();
        tokio::spawn(
            async move {
                if let Err(e) = handle(ctx, s, from).await {
                    warn!("connection failed: {e:#}");
                }
            }
            .instrument(info_span!("conn", %from)),
        );
    }
}

async fn handle(ctx: Arc<Ctx>, mut s: TcpStream, from: SocketAddr) -> anyhow::Result<()> {
    net::setup_tcp(
        &s,
        ctx.cfg.tune.congestion.as_deref(),
        ctx.cfg.tune.max_rate_mbit,
    );
    let t = Instant::now();
    let id = timeout(Duration::from_secs(10), proto::server_handshake(&mut s))
        .await
        .map_err(|_| anyhow::anyhow!("relay handshake timed out after 10s"))?
        .map_err(|e| anyhow::anyhow!("relay handshake: {e}"))?;
    let (sess, resumed) = {
        let mut sessions = ctx.sessions.lock().unwrap();
        let resumed = sessions.contains_key(&id);
        let sess = sessions
            .entry(id)
            .or_insert_with(|| {
                Arc::new(Session {
                    id,
                    q: Arc::new(Shared::new(ctx.cfg.tune.limits)),
                    peers: Mutex::default(),
                    conn: Mutex::default(),
                    last_ms: AtomicU64::new(now_ms()),
                    connections: AtomicU64::new(0),
                })
            })
            .clone();
        (sess, resumed)
    };
    let n = sess.connections.fetch_add(1, Ordering::Relaxed) + 1;
    info!(
        session = sess.tag(),
        resumed,
        connection_no = n,
        peers = sess.peers.lock().unwrap().len(),
        handshake_ms = t.elapsed().as_millis() as u64,
        "client connected"
    );
    let link = Link::new(format!("{from}"), &s);
    let task = tokio::spawn(serve(ctx.clone(), sess.clone(), s, link).in_current_span());
    // A new connection of the same session wins: the old one is likely dead already.
    if let Some(old) = sess.conn.lock().unwrap().replace(task.abort_handle())
        && !old.is_finished()
    {
        info!(
            session = sess.tag(),
            "replacing previous connection of this session"
        );
        old.abort();
    }
    let r = match task.await {
        Ok(r) => r,
        Err(e) if e.is_cancelled() => {
            info!(session = sess.tag(), "connection replaced by a newer one");
            Ok(())
        }
        Err(e) => Err(e.into()),
    };
    sess.last_ms.store(now_ms(), Ordering::Relaxed);
    info!(session = sess.tag(), "client disconnected");
    r
}

async fn serve(ctx: Arc<Ctx>, sess: Arc<Session>, s: TcpStream, link: Link) -> anyhow::Result<()> {
    if let Some(i) = link.tcp() {
        debug!(?i, "tcp state after handshake");
    }
    let (rd, wr) = s.into_split();
    let r = tokio::select! {
        r = common::writer(wr, &sess.q, &link) => r.map_err(|e| e.context("tcp write")),
        r = reader(&ctx, &sess, rd, &link) => r.map_err(|e| e.context("tcp read")),
        r = common::keepalive(&sess.q, &link, &ctx.cfg.tune) => r,
    };
    info!(uptime_s = link.uptime().as_secs(), "connection ended");
    r
}

async fn reader(
    ctx: &Ctx,
    sess: &Arc<Session>,
    rd: tokio::net::tcp::OwnedReadHalf,
    link: &Link,
) -> anyhow::Result<()> {
    let mut rd = BufReader::with_capacity(256 * 1024, rd);
    let mut buf = Vec::with_capacity(65536);
    loop {
        let f = proto::read_frame(&mut rd, &mut buf).await?;
        link.on_rx();
        match f {
            Frame::Data { peer, payload } => {
                let Some(kind) = wg::classify(payload) else {
                    inc(&ctx.stats.not_wg);
                    debug!(
                        peer,
                        len = payload.len(),
                        "drop: not a WireGuard message from client"
                    );
                    continue;
                };
                inc(&ctx.stats.udp_in);
                let Some((sock, last)) = peer_sock(ctx, sess, peer).await else {
                    continue;
                };
                last.store(now_ms(), Ordering::Relaxed);
                trace!(peer, len = payload.len(), ?kind, "tcp -> udp");
                if kind == wg::Kind::Control {
                    debug!(
                        peer,
                        msg = wg::describe(payload),
                        "wireguard control to server"
                    );
                }
                match net::send_nowait(&sock, payload, None) {
                    Ok(_) => inc(&ctx.stats.udp_out),
                    Err(e) => {
                        inc(&ctx.stats.udp_full);
                        trace!(peer, "drop: udp send: {e}");
                    }
                }
            }
            Frame::Ping(n) => sess.q.push_session(proto::encode_pong(n)),
            Frame::Pong(sent) => link.on_pong(sent),
            Frame::Close(peer) => {
                if let Some(p) = sess.peers.lock().unwrap().remove(&peer) {
                    info!(
                        peer,
                        lived_s = p.since.elapsed().as_secs(),
                        "peer closed by client"
                    );
                }
                sess.q.remove_peer(peer);
            }
        }
    }
}

/// Returns the UDP socket for `peer`, creating it on first use.
async fn peer_sock(
    ctx: &Ctx,
    sess: &Arc<Session>,
    peer: u32,
) -> Option<(Arc<UdpSocket>, Arc<AtomicU64>)> {
    if let Some(p) = sess.peers.lock().unwrap().get(&peer) {
        return Some((p.sock.clone(), p.last_ms.clone()));
    }
    let t = Instant::now();
    let (sock, target) = match open_peer_socket(&ctx.cfg.target).await {
        Ok(s) => s,
        Err(e) => {
            warn!(peer, target = %ctx.cfg.target, "drop: cannot open UDP socket to target: {e}");
            return None;
        }
    };
    let sock = Arc::new(sock);
    let last = Arc::new(AtomicU64::new(now_ms()));
    let span = info_span!("peer", session = sess.tag(), peer);
    let task = tokio::spawn(
        peer_rx(
            sock.clone(),
            last.clone(),
            sess.q.clone(),
            ctx.stats.clone(),
            peer,
        )
        .instrument(span),
    );
    let mut peers = sess.peers.lock().unwrap();
    // Another frame of the same peer may have raced us; keep the first socket.
    let total = peers.len() + 1;
    let p = peers.entry(peer).or_insert_with(|| {
        info!(
            peer,
            local = ?sock.local_addr().ok(),
            %target,
            setup_ms = t.elapsed().as_millis() as u64,
            total,
            "new peer"
        );
        PeerSock {
            sock,
            last_ms: last,
            task: task.abort_handle(),
            since: Instant::now(),
        }
    });
    Some((p.sock.clone(), p.last_ms.clone()))
}

async fn open_peer_socket(target: &str) -> std::io::Result<(UdpSocket, SocketAddr)> {
    let addr = net::resolve(target).await?;
    let s = net::udp_socket(net::wildcard_for(addr))?;
    s.connect(addr).await?;
    Ok((s, addr))
}

/// WireGuard server -> relay client.
async fn peer_rx(
    sock: Arc<UdpSocket>,
    last: Arc<AtomicU64>,
    q: Arc<Shared>,
    stats: Arc<Stats>,
    peer: u32,
) {
    let mut buf = vec![0u8; 65536];
    let mut refused = 0u64;
    loop {
        match sock.recv(&mut buf).await {
            Ok(n) => {
                if refused > 0 {
                    info!(after_errors = refused, "WireGuard server reachable again");
                    refused = 0;
                }
                let p = &buf[..n];
                let Some(kind) = wg::classify(p) else {
                    inc(&stats.not_wg);
                    debug!(len = n, "drop: not a WireGuard message from server");
                    continue;
                };
                last.store(now_ms(), Ordering::Relaxed);
                trace!(len = n, ?kind, "udp -> tcp");
                if kind == wg::Kind::Control {
                    debug!(msg = wg::describe(p), "wireguard control from server");
                }
                q.push_wg(peer, kind, proto::encode_data(peer, p));
            }
            // ECONNREFUSED: WireGuard server is down/restarting; keep the socket.
            Err(e) => {
                refused += 1;
                if refused == 1 {
                    warn!("WireGuard server unreachable: {e}");
                } else {
                    debug!(count = refused, "udp recv: {e}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn reaper(ctx: &Ctx) {
    let idle = ctx.cfg.tune.peer_timeout;
    let mut iv = tokio::time::interval(Duration::from_secs(10));
    loop {
        iv.tick().await;
        let mut sessions = ctx.sessions.lock().unwrap();
        sessions.retain(|_, s| {
            let mut peers = s.peers.lock().unwrap();
            peers.retain(|peer, p| {
                let keep = idle_for(&p.last_ms) <= idle;
                if !keep {
                    info!(
                        session = s.tag(),
                        peer,
                        lived_s = p.since.elapsed().as_secs(),
                        "peer expired"
                    );
                    s.q.remove_peer(*peer);
                }
                keep
            });
            let keep = s.connected() || idle_for(&s.last_ms) <= idle;
            if !keep {
                info!(
                    session = s.tag(),
                    peers = peers.len(),
                    "session expired (client gone)"
                );
            }
            keep
        });
        debug!(sessions = sessions.len(), "reaper pass");
    }
}

async fn stats(ctx: &Ctx) {
    let every = ctx.cfg.tune.stats_every;
    if every.is_zero() {
        return;
    }
    let queues = || -> Vec<Arc<Shared>> {
        ctx.sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| s.q.clone())
            .collect()
    };
    // Queue drop counters live in sessions, which come and go; deltas are clamped at zero.
    let qs = queues();
    let mut prev = Snapshot::take(
        &ctx.stats,
        &qs.iter().map(|q| q.as_ref()).collect::<Vec<_>>(),
    );
    let mut iv = tokio::time::interval(every);
    iv.tick().await;
    loop {
        iv.tick().await;
        let qs = queues();
        let refs: Vec<&Shared> = qs.iter().map(|q| q.as_ref()).collect();
        let cur = Snapshot::take(&ctx.stats, &refs);
        let (sessions, peers) = {
            let s = ctx.sessions.lock().unwrap();
            (
                s.len(),
                s.values()
                    .map(|s| s.peers.lock().unwrap().len())
                    .sum::<usize>(),
            )
        };
        debug!(sessions, "sessions");
        cur.log_delta(&prev, peers, &refs);
        prev = cur;
    }
}
