//! Outbound queue in front of the TCP stream.
//!
//! The whole point of the relay is to never let packets pile up: TCP absorbs loss by
//! buffering, so WireGuard traffic tunneled over TCP would otherwise see seconds of
//! latency under load (and inner TCP flows would never back off). Instead:
//! - session-level frames (ping/pong/close) go first, always;
//! - WireGuard control messages (handshakes, cookies, keepalives) go next, so sessions
//!   stay alive under load;
//! - data is queued per peer and served round-robin, so one heavy peer cannot starve others;
//! - each peer's data queue is bounded by bytes, and data that waited longer than
//!   `max_delay` is dropped on dequeue (CoDel-like), turning congestion into packet loss
//!   that inner TCP flows react to.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Max queued data bytes per peer.
    pub peer_bytes: usize,
    /// Max queued control bytes (all peers together).
    pub control_bytes: usize,
    /// Data older than this is dropped instead of being sent.
    pub max_delay: Duration,
    /// Control messages older than this are dropped (a stale handshake is useless).
    pub control_max_delay: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            peer_bytes: 256 * 1024,
            control_bytes: 64 * 1024,
            max_delay: Duration::from_millis(150),
            control_max_delay: Duration::from_secs(2),
        }
    }
}

struct Item {
    frame: Vec<u8>,
    at: Instant,
}

#[derive(Default)]
struct PeerQueue {
    items: VecDeque<Item>,
    bytes: usize,
}

#[derive(Default, Debug)]
pub struct DropStats {
    pub overflow: AtomicU64,
    pub aged: AtomicU64,
}

/// Queue backlog snapshot.
#[derive(Debug, Default, Clone, Copy)]
pub struct Depth {
    pub frames: usize,
    pub bytes: usize,
    pub control_frames: usize,
    /// Peers with queued data.
    pub busy_peers: usize,
    /// Age of the oldest queued WireGuard message.
    pub oldest_ms: u64,
}

pub struct Queue {
    limits: Limits,
    session: VecDeque<Vec<u8>>,
    control: VecDeque<Item>,
    control_bytes: usize,
    peers: HashMap<u32, PeerQueue>,
    /// Round-robin order of peers with non-empty data queues.
    active: VecDeque<u32>,
}

impl Queue {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            session: VecDeque::new(),
            control: VecDeque::new(),
            control_bytes: 0,
            peers: HashMap::new(),
            active: VecDeque::new(),
        }
    }

    /// Session frames (ping/pong/close): never dropped.
    pub fn push_session(&mut self, frame: Vec<u8>) {
        self.session.push_back(frame);
    }

    /// Returns false if the frame was dropped.
    pub fn push_control(&mut self, frame: Vec<u8>, now: Instant) -> bool {
        if self.control_bytes + frame.len() > self.limits.control_bytes {
            return false;
        }
        self.control_bytes += frame.len();
        self.control.push_back(Item { frame, at: now });
        true
    }

    /// Returns false if the frame was dropped.
    pub fn push_data(&mut self, peer: u32, frame: Vec<u8>, now: Instant) -> bool {
        let q = self.peers.entry(peer).or_default();
        if q.bytes + frame.len() > self.limits.peer_bytes {
            return false;
        }
        if q.items.is_empty() {
            self.active.push_back(peer);
        }
        q.bytes += frame.len();
        q.items.push_back(Item { frame, at: now });
        true
    }

    /// Forgets a peer's queue.
    pub fn remove_peer(&mut self, peer: u32) {
        if self.peers.remove(&peer).is_some() {
            self.active.retain(|p| *p != peer);
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.session.is_empty() && self.control.is_empty() && self.active.is_empty()
    }

    /// Current backlog, for troubleshooting.
    pub fn depth(&self, now: Instant) -> Depth {
        let mut d = Depth {
            frames: self.session.len() + self.control.len(),
            bytes: self.control_bytes,
            control_frames: self.control.len(),
            busy_peers: self.active.len(),
            oldest_ms: 0,
        };
        let mut oldest = self.control.front().map(|i| i.at);
        for q in self.peers.values() {
            d.frames += q.items.len();
            d.bytes += q.bytes;
            if let Some(i) = q.items.front() {
                oldest = Some(oldest.map_or(i.at, |o| o.min(i.at)));
            }
        }
        d.oldest_ms = oldest.map_or(0, |o| now.duration_since(o).as_millis() as u64);
        d
    }

    /// Next frame to send. Expired frames are dropped and counted in `aged`.
    pub fn pop(&mut self, now: Instant, aged: &mut u64) -> Option<Vec<u8>> {
        if let Some(f) = self.session.pop_front() {
            return Some(f);
        }
        while let Some(it) = self.control.pop_front() {
            self.control_bytes -= it.frame.len();
            if now.duration_since(it.at) <= self.limits.control_max_delay {
                return Some(it.frame);
            }
            *aged += 1;
        }
        while let Some(peer) = self.active.pop_front() {
            let q = self.peers.get_mut(&peer).expect("active peer has a queue");
            let mut out = None;
            while let Some(it) = q.items.pop_front() {
                q.bytes -= it.frame.len();
                if now.duration_since(it.at) <= self.limits.max_delay {
                    out = Some(it.frame);
                    break;
                }
                *aged += 1;
            }
            if !q.items.is_empty() {
                self.active.push_back(peer);
            }
            if out.is_some() {
                return out;
            }
        }
        None
    }
}

/// A queue shared between producers (UDP readers) and the single TCP writer.
pub struct Shared {
    q: Mutex<Queue>,
    notify: Notify,
    pub drops: DropStats,
}

impl Shared {
    pub fn new(limits: Limits) -> Self {
        Self {
            q: Mutex::new(Queue::new(limits)),
            notify: Notify::new(),
            drops: DropStats::default(),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Queue) -> T) -> T {
        f(&mut self.q.lock().unwrap())
    }

    pub fn push_session(&self, frame: Vec<u8>) {
        self.with(|q| q.push_session(frame));
        self.notify.notify_one();
    }

    /// Queues a WireGuard datagram, prioritized by kind.
    pub fn push_wg(&self, peer: u32, kind: crate::wg::Kind, frame: Vec<u8>) {
        let now = Instant::now();
        let len = frame.len();
        let ok = self.with(|q| match kind {
            crate::wg::Kind::Control => q.push_control(frame, now),
            crate::wg::Kind::Data => q.push_data(peer, frame, now),
        });
        if ok {
            self.notify.notify_one();
        } else {
            self.drops.overflow.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(peer, ?kind, len, "drop: queue full");
        }
    }

    pub fn remove_peer(&self, peer: u32) {
        self.with(|q| q.remove_peer(peer));
    }

    pub fn depth(&self) -> Depth {
        self.with(|q| q.depth(Instant::now()))
    }

    /// Moves up to `max_bytes` of frames into `out`. Waits while the queue is empty.
    pub async fn pop_batch(&self, out: &mut Vec<u8>, max_bytes: usize) {
        loop {
            let mut aged = 0;
            self.with(|q| {
                let now = Instant::now();
                while out.len() < max_bytes {
                    match q.pop(now, &mut aged) {
                        Some(f) => out.extend_from_slice(&f),
                        None => break,
                    }
                }
            });
            if aged > 0 {
                self.drops.aged.fetch_add(aged, Ordering::Relaxed);
                tracing::trace!(count = aged, "drop: waited too long in queue");
            }
            if !out.is_empty() {
                return;
            }
            // Notify stores a permit if notified while nobody waits, so no wakeup is lost.
            self.notify.notified().await;
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.with(|q| q.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            peer_bytes: 10,
            control_bytes: 6,
            max_delay: Duration::from_millis(100),
            control_max_delay: Duration::from_millis(500),
        }
    }

    fn pop_all(q: &mut Queue, now: Instant) -> (Vec<Vec<u8>>, u64) {
        let mut aged = 0;
        let mut v = Vec::new();
        while let Some(f) = q.pop(now, &mut aged) {
            v.push(f);
        }
        (v, aged)
    }

    #[test]
    fn priority_order() {
        let mut q = Queue::new(limits());
        let t = Instant::now();
        assert!(q.push_data(1, vec![b'd'], t));
        assert!(q.push_control(vec![b'c'], t));
        q.push_session(vec![b's']);
        let (out, _) = pop_all(&mut q, t);
        assert_eq!(out, vec![vec![b's'], vec![b'c'], vec![b'd']]);
        assert!(q.is_empty());
    }

    #[test]
    fn round_robin_between_peers() {
        let mut q = Queue::new(limits());
        let t = Instant::now();
        for _ in 0..3 {
            q.push_data(1, vec![1], t);
        }
        q.push_data(2, vec![2], t);
        q.push_data(2, vec![2], t);
        let (out, _) = pop_all(&mut q, t);
        assert_eq!(out, vec![vec![1], vec![2], vec![1], vec![2], vec![1]]);
    }

    #[test]
    fn per_peer_byte_limit() {
        let mut q = Queue::new(limits());
        let t = Instant::now();
        assert!(q.push_data(1, vec![0; 6], t));
        assert!(!q.push_data(1, vec![0; 6], t)); // would exceed 10 bytes
        assert!(q.push_data(2, vec![0; 6], t)); // other peer unaffected
        assert!(q.push_data(1, vec![0; 4], t)); // fits exactly
        assert!(!q.push_control(vec![0; 7], t)); // control limit is 6
    }

    #[test]
    fn drops_aged_data_but_keeps_fresh_control() {
        let mut q = Queue::new(limits());
        let t = Instant::now();
        q.push_data(1, vec![1], t);
        q.push_data(1, vec![2], t + Duration::from_millis(150));
        q.push_control(vec![9], t);
        let (out, aged) = pop_all(&mut q, t + Duration::from_millis(200));
        assert_eq!(out, vec![vec![9], vec![2]]);
        assert_eq!(aged, 1);
        // byte accounting released: peer 1 can queue 10 bytes again
        assert!(q.push_data(1, vec![0; 10], t));
    }

    #[test]
    fn remove_peer() {
        let mut q = Queue::new(limits());
        let t = Instant::now();
        q.push_data(1, vec![1], t);
        q.push_data(2, vec![2], t);
        q.remove_peer(1);
        let (out, _) = pop_all(&mut q, t);
        assert_eq!(out, vec![vec![2]]);
    }

    #[tokio::test]
    async fn shared_batch_wakes_up() {
        let s = std::sync::Arc::new(Shared::new(Limits::default()));
        let s2 = s.clone();
        let h = tokio::spawn(async move {
            let mut out = Vec::new();
            s2.pop_batch(&mut out, 1024).await;
            out
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.push_session(vec![1, 2, 3]);
        assert_eq!(h.await.unwrap(), vec![1, 2, 3]);
        assert!(s.is_empty());
    }
}
