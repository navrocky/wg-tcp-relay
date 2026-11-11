//! End-to-end test on localhost: peers -> client -> TCP -> server -> fake WireGuard echo server.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::timeout;
use wg_tcp_relay::common::Tuning;
use wg_tcp_relay::queue::Limits;
use wg_tcp_relay::{client, server};

fn tuning() -> Tuning {
    Tuning {
        limits: Limits::default(),
        keepalive: Duration::from_secs(1),
        dead_after: Duration::from_secs(3),
        peer_timeout: Duration::from_secs(60),
        stats_every: Duration::ZERO,
        congestion: None,
        max_rate_mbit: 0.0,
    }
}

/// WireGuard transport message of `len` bytes carrying `tag` in its body.
fn wg_data(tag: u8, len: usize) -> Vec<u8> {
    let mut v = vec![tag; len];
    v[..4].copy_from_slice(&[4, 0, 0, 0]);
    v
}

async fn free_tcp() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn free_udp() -> SocketAddr {
    UdpSocket::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Echoes every datagram back; records source addresses.
async fn echo_server() -> (SocketAddr, Arc<Mutex<HashSet<SocketAddr>>>) {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = s.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, from) = s.recv_from(&mut buf).await.unwrap();
            seen2.lock().unwrap().insert(from);
            s.send_to(&buf[..n], from).await.unwrap();
        }
    });
    (addr, seen)
}

async fn start_relay(target: SocketAddr) -> SocketAddr {
    let tcp = free_tcp().await;
    let udp = free_udp().await;
    tokio::spawn(server::run(server::Config {
        listen: tcp,
        target: target.to_string(),
        tune: tuning(),
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::spawn(client::run(client::Config {
        listen: udp,
        connect: tcp.to_string(),
        tune: tuning(),
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    udp
}

async fn roundtrip(peer: &UdpSocket, relay: SocketAddr, msg: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 65536];
    // The first packet may race the TCP connection setup; retry a few times.
    for _ in 0..20 {
        peer.send_to(msg, relay).await.unwrap();
        if let Ok(Ok((n, _))) = timeout(Duration::from_millis(250), peer.recv_from(&mut buf)).await
        {
            return buf[..n].to_vec();
        }
    }
    panic!("no reply through relay");
}

#[tokio::test]
async fn peers_are_relayed_separately() {
    let (wg, seen) = echo_server().await;
    let relay = start_relay(wg).await;

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for i in 0..50u8 {
        let ma = wg_data(i, 100 + i as usize);
        let mb = wg_data(200 - i, 1452);
        assert_eq!(roundtrip(&a, relay, &ma).await, ma);
        assert_eq!(roundtrip(&b, relay, &mb).await, mb);
    }
    // Each relayed peer gets its own source address at the WireGuard server.
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn handshake_sizes_pass_and_garbage_is_dropped() {
    let (wg, seen) = echo_server().await;
    let relay = start_relay(wg).await;
    let p = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let mut init = vec![0xAB; 148];
    init[..4].copy_from_slice(&[1, 0, 0, 0]);
    assert_eq!(roundtrip(&p, relay, &init).await, init);

    // Not WireGuard: must never reach the server.
    let before = seen.lock().unwrap().len();
    p.send_to(b"GET / HTTP/1.1\r\n\r\n", relay).await.unwrap();
    let mut buf = [0u8; 64];
    assert!(
        timeout(Duration::from_millis(300), p.recv_from(&mut buf))
            .await
            .is_err()
    );
    assert_eq!(seen.lock().unwrap().len(), before);
}

#[tokio::test]
async fn burst_does_not_stall() {
    let (wg, _) = echo_server().await;
    let relay = start_relay(wg).await;
    let p = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ka = wg_data(0, 32);
    roundtrip(&p, relay, &ka).await;

    // Blast far more than the queues hold; some loss is fine, a stall is not.
    let big = wg_data(7, 1452);
    for _ in 0..20_000 {
        let _ = p.send_to(&big, relay).await;
    }
    // Drain whatever comes back, then the tunnel must still answer promptly.
    let mut buf = vec![0u8; 65536];
    while timeout(Duration::from_millis(200), p.recv_from(&mut buf))
        .await
        .is_ok()
    {}
    let probe = wg_data(9, 64);
    let t = std::time::Instant::now();
    assert_eq!(roundtrip(&p, relay, &probe).await, probe);
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "tunnel stalled: {:?}",
        t.elapsed()
    );
}
