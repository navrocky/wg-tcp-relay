//! Pieces shared by client and server.

use crate::net;
use crate::proto;
use crate::queue::{Limits, Shared};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, trace};

#[derive(Debug, Clone)]
pub struct Tuning {
    pub limits: Limits,
    /// Ping interval.
    pub keepalive: Duration,
    /// Connection is considered dead when nothing was received for this long.
    pub dead_after: Duration,
    /// Peers idle for this long are forgotten.
    pub peer_timeout: Duration,
    /// Stats log interval (zero disables).
    pub stats_every: Duration,
    /// TCP congestion control for the relay connection (None: system default).
    pub congestion: Option<String>,
    /// Cap on the relay's TCP sending rate, Mbit/s (0: no cap).
    pub max_rate_mbit: f64,
}

/// Max bytes written to TCP per syscall.
pub const WRITE_BATCH: usize = 64 * 1024;

/// A TCP write that takes longer than this is reported: the link cannot keep up.
const SLOW_WRITE: Duration = Duration::from_secs(1);

/// Milliseconds since process start; cheap monotonic timestamp for atomics.
pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn idle_for(last_ms: &AtomicU64) -> Duration {
    Duration::from_millis(now_ms().saturating_sub(last_ms.load(Ordering::Relaxed)))
}

#[derive(Default, Debug)]
pub struct Stats {
    pub udp_in: AtomicU64,
    pub udp_out: AtomicU64,
    /// Local UDP socket was full: datagram dropped (never block on UDP).
    pub udp_full: AtomicU64,
    /// Not a WireGuard message: dropped.
    pub not_wg: AtomicU64,
    /// DATA frame for a peer we do not know (expired or never seen).
    pub unknown_peer: AtomicU64,
}

pub fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Counter snapshot, for "per interval" deltas.
#[derive(Default, Clone, Copy)]
pub struct Snapshot {
    udp_in: u64,
    udp_out: u64,
    udp_full: u64,
    not_wg: u64,
    unknown_peer: u64,
    queue_full: u64,
    aged: u64,
}

impl Snapshot {
    pub fn take(stats: &Stats, queues: &[&Shared]) -> Self {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut s = Snapshot {
            udp_in: l(&stats.udp_in),
            udp_out: l(&stats.udp_out),
            udp_full: l(&stats.udp_full),
            not_wg: l(&stats.not_wg),
            unknown_peer: l(&stats.unknown_peer),
            ..Default::default()
        };
        for q in queues {
            s.queue_full += l(&q.drops.overflow);
            s.aged += l(&q.drops.aged);
        }
        s
    }

    /// Logs `self - prev` at info level.
    pub fn log_delta(&self, prev: &Snapshot, peers: usize, queues: &[&Shared]) {
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let (mut frames, mut bytes, mut oldest) = (0, 0, 0);
        for q in queues {
            let x = q.depth();
            frames += x.frames;
            bytes += x.bytes;
            oldest = oldest.max(x.oldest_ms);
        }
        info!(
            peers,
            udp_in = d(self.udp_in, prev.udp_in),
            udp_out = d(self.udp_out, prev.udp_out),
            drop_queue_full = d(self.queue_full, prev.queue_full),
            drop_aged = d(self.aged, prev.aged),
            drop_udp_full = d(self.udp_full, prev.udp_full),
            drop_not_wg = d(self.not_wg, prev.not_wg),
            drop_unknown_peer = d(self.unknown_peer, prev.unknown_peer),
            queued_frames = frames,
            queued_bytes = bytes,
            queue_oldest_ms = oldest,
            "stats"
        );
    }
}

/// Liveness and latency of one TCP connection.
pub struct Link {
    pub label: String,
    fd: i32,
    last_rx: AtomicU64,
    rtt_ms: AtomicU64,
    rtt_max_ms: AtomicU64,
    started: Instant,
}

impl Link {
    pub fn new(label: String, s: &tokio::net::TcpStream) -> Self {
        #[cfg(unix)]
        let fd = std::os::fd::AsRawFd::as_raw_fd(s);
        #[cfg(not(unix))]
        let fd = -1;
        Self {
            label,
            fd,
            last_rx: AtomicU64::new(now_ms()),
            rtt_ms: AtomicU64::new(0),
            rtt_max_ms: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    pub fn on_rx(&self) {
        self.last_rx.store(now_ms(), Ordering::Relaxed);
    }

    /// Ping carries the send time, so the pong gives the full tunnel RTT including queueing.
    pub fn on_pong(&self, sent_ms: u64) {
        let rtt = now_ms().saturating_sub(sent_ms);
        self.rtt_ms.store(rtt, Ordering::Relaxed);
        self.rtt_max_ms.fetch_max(rtt, Ordering::Relaxed);
        trace!(link = %self.label, rtt_ms = rtt, "pong");
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Kernel TCP state. Only valid while the connection's socket is alive, which holds
    /// because a `Link` lives exactly as long as its connection.
    pub fn tcp(&self) -> Option<net::TcpInfo> {
        if self.fd < 0 {
            None
        } else {
            net::tcp_info(self.fd)
        }
    }
}

/// Drains the queue into the TCP stream forever.
pub async fn writer<W: AsyncWrite + Unpin>(
    mut w: W,
    q: &Shared,
    link: &Link,
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(WRITE_BATCH + 2 * u16::MAX as usize);
    loop {
        buf.clear();
        q.pop_batch(&mut buf, WRITE_BATCH).await;
        let t = Instant::now();
        w.write_all(&buf).await?;
        let took = t.elapsed();
        trace!(link = %link.label, bytes = buf.len(), took_us = took.as_micros() as u64, "tcp write");
        if took > SLOW_WRITE {
            debug!(
                link = %link.label,
                bytes = buf.len(),
                took_ms = took.as_millis() as u64,
                "tcp write was blocked: the link is slower than the offered load"
            );
        }
    }
}

/// Sends pings, logs link state, and fails when the peer has been silent for `dead_after`.
pub async fn keepalive(q: &Shared, link: &Link, t: &Tuning) -> anyhow::Result<()> {
    let mut iv = tokio::time::interval(t.keepalive);
    let mut last_report = Instant::now();
    let mut prev_retrans = link.tcp().map_or(0, |i| i.total_retrans);
    loop {
        iv.tick().await;
        let idle = idle_for(&link.last_rx);
        if idle > t.dead_after {
            let tcp = link.tcp().unwrap_or_default();
            anyhow::bail!(
                "nothing received for {:.1}s, connection is dead (tcp rtt {:.0}ms, unacked {}, notsent {}B, retrans {})",
                idle.as_secs_f32(),
                tcp.rtt_ms,
                tcp.unacked,
                tcp.notsent_bytes,
                tcp.total_retrans
            );
        }
        q.push_session(proto::encode_ping(now_ms()));

        let tcp = link.tcp().unwrap_or_default();
        let d = q.depth();
        debug!(
            link = %link.label,
            ping_rtt_ms = link.rtt_ms.load(Ordering::Relaxed),
            idle_ms = idle.as_millis() as u64,
            tcp_rtt_ms = tcp.rtt_ms,
            tcp_rttvar_ms = tcp.rttvar_ms,
            tcp_min_rtt_ms = tcp.min_rtt_ms,
            cwnd = tcp.cwnd,
            unacked = tcp.unacked,
            notsent_bytes = tcp.notsent_bytes,
            retrans_total = tcp.total_retrans,
            delivery_rate_kbit = tcp.delivery_rate_kbit,
            queued_frames = d.frames,
            queued_bytes = d.bytes,
            queued_control = d.control_frames,
            busy_peers = d.busy_peers,
            queue_oldest_ms = d.oldest_ms,
            "link"
        );

        if !t.stats_every.is_zero() && last_report.elapsed() >= t.stats_every {
            last_report = Instant::now();
            info!(
                link = %link.label,
                uptime_s = link.uptime().as_secs(),
                ping_rtt_ms = link.rtt_ms.load(Ordering::Relaxed),
                ping_rtt_max_ms = link.rtt_max_ms.swap(0, Ordering::Relaxed),
                tcp_rtt_ms = tcp.rtt_ms,
                retrans = tcp.total_retrans.saturating_sub(prev_retrans),
                sent_mb = tcp.bytes_acked / 1_000_000,
                received_mb = tcp.bytes_received / 1_000_000,
                "link stats"
            );
            prev_retrans = tcp.total_retrans;
        }
    }
}

/// Random 64-bit value without extra dependencies (RandomState is seeded from the OS).
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.finish()
}
