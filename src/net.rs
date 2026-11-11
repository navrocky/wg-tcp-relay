//! Socket setup helpers.

use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};

/// Socket buffer size we ask for; the kernel clamps it to `net.core.{r,w}mem_max`.
pub const UDP_BUFFER: usize = 8 * 1024 * 1024;

/// Unsent bytes allowed in the kernel TCP send buffer. Keeping it small keeps the queue in
/// our userspace queue, where it is bounded and aged out, instead of in the kernel.
pub const TCP_NOTSENT_LOWAT: u32 = 64 * 1024;

pub fn udp_socket(bind: SocketAddr) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    if bind.is_ipv6() {
        // Accept IPv4-mapped too when bound to [::].
        let _ = s.set_only_v6(false);
    }
    let _ = s.set_recv_buffer_size(UDP_BUFFER);
    let _ = s.set_send_buffer_size(UDP_BUFFER);
    s.set_nonblocking(true)?;
    s.bind(&bind.into())?;
    UdpSocket::from_std(s.into())
}

/// Sends a datagram without ever waiting: a full socket buffer means the datagram is dropped.
///
/// Calls the syscall directly instead of tokio's `try_send*`: those consult tokio's readiness
/// state, which reports "not writable" for a fresh socket until the reactor has seen its first
/// writable event, dropping perfectly sendable packets.
pub fn send_nowait(sock: &UdpSocket, buf: &[u8], to: Option<SocketAddr>) -> io::Result<usize> {
    let s = SockRef::from(sock);
    match to {
        Some(a) => s.send_to(buf, &a.into()),
        None => s.send(buf),
    }
}

/// Unconnected-bind wildcard of the same family as `target`.
pub fn wildcard_for(target: SocketAddr) -> SocketAddr {
    if target.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    }
}

pub fn tune_tcp(s: &TcpStream) -> io::Result<()> {
    s.set_nodelay(true)?;
    let sr = SockRef::from(s);
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5));
    #[cfg(target_os = "linux")]
    let ka = ka.with_retries(3);
    sr.set_tcp_keepalive(&ka)?;
    #[cfg(target_os = "linux")]
    set_notsent_lowat(s, TCP_NOTSENT_LOWAT)?;
    Ok(())
}

/// Sets the congestion control algorithm (e.g. "bbr") and returns the one in effect.
///
/// Matters a lot for tunneling: loss-based algorithms (cubic) fill the bottleneck buffer,
/// adding up to seconds of latency for everything inside the tunnel; BBR keeps it short.
#[cfg(target_os = "linux")]
pub fn set_congestion(s: &TcpStream, name: &str) -> io::Result<String> {
    let sr = SockRef::from(s);
    sr.set_tcp_congestion(name.as_bytes())?;
    current_congestion(s)
}

#[cfg(target_os = "linux")]
pub fn current_congestion(s: &TcpStream) -> io::Result<String> {
    let v = SockRef::from(s).tcp_congestion()?;
    Ok(String::from_utf8_lossy(&v)
        .trim_end_matches('\0')
        .to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn set_congestion(_s: &TcpStream, _name: &str) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "congestion control selection is Linux-only",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn current_congestion(_s: &TcpStream) -> io::Result<String> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Linux-only"))
}

/// Caps the kernel's sending rate for this socket (bytes/s), with smooth pacing.
///
/// Sending slightly below the link's capacity means the relay's own TCP never overflows
/// the bottleneck: no outer retransmissions (each one stalls *all* tunneled traffic for an
/// RTT); excess load waits in the relay queue instead, where it is aged out and turned
/// into loss for the inner flows.
#[cfg(target_os = "linux")]
pub fn set_max_pacing_rate(s: &TcpStream, bytes_per_sec: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let v: u32 = bytes_per_sec.min(u32::MAX as u64) as u32;
    // SAFETY: valid fd, pointer to a u32 of the given size.
    let r = unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_MAX_PACING_RATE,
            &v as *const _ as *const libc::c_void,
            std::mem::size_of::<u32>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_max_pacing_rate(_s: &TcpStream, _bytes_per_sec: u64) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Linux-only"))
}

/// Applies all TCP settings, logging (not failing) on errors.
pub fn setup_tcp(s: &TcpStream, congestion: Option<&str>, max_rate_mbit: f64) {
    if max_rate_mbit > 0.0 {
        let bps = (max_rate_mbit * 1_000_000.0 / 8.0) as u64;
        match set_max_pacing_rate(s, bps) {
            Ok(()) => tracing::debug!(max_rate_mbit, "tcp send rate capped"),
            Err(e) => tracing::warn!(max_rate_mbit, "cannot cap tcp send rate: {e}"),
        }
    }
    if let Err(e) = tune_tcp(s) {
        tracing::warn!("tcp tuning failed (continuing): {e}");
    }
    match congestion {
        Some(cc) => match set_congestion(s, cc) {
            Ok(cur) => tracing::debug!(congestion = %cur, "tcp congestion control set"),
            Err(e) => tracing::warn!(
                wanted = cc,
                current = current_congestion(s).ok().as_deref().unwrap_or("?"),
                "cannot set tcp congestion control (module not loaded? try `modprobe tcp_{cc}`): {e}"
            ),
        },
        None => tracing::debug!(
            congestion = current_congestion(s).ok().as_deref().unwrap_or("?"),
            "tcp congestion control: system default"
        ),
    }
}

#[cfg(target_os = "linux")]
fn set_notsent_lowat(s: &TcpStream, v: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let v = v as libc::c_int;
    // SAFETY: valid fd, pointer to a c_int of the right size.
    let r = unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NOTSENT_LOWAT,
            &v as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Resolves `host:port`, preferring IPv4.
pub async fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr).await?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {addr}")))
}

/// Subset of the kernel's `struct tcp_info` (linux/tcp.h). Declared here instead of using
/// `libc::tcp_info` because the musl and glibc definitions differ; the kernel copies
/// `min(optlen, sizeof(kernel struct))`, so a prefix is fine.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct RawTcpInfo {
    state: u8,
    ca_state: u8,
    retransmits: u8,
    probes: u8,
    backoff: u8,
    options: u8,
    wscale: u8,
    flags: u8,
    rto: u32,
    ato: u32,
    snd_mss: u32,
    rcv_mss: u32,
    unacked: u32,
    sacked: u32,
    lost: u32,
    retrans: u32,
    fackets: u32,
    last_data_sent: u32,
    last_ack_sent: u32,
    last_data_recv: u32,
    last_ack_recv: u32,
    pmtu: u32,
    rcv_ssthresh: u32,
    rtt: u32,
    rttvar: u32,
    snd_ssthresh: u32,
    snd_cwnd: u32,
    advmss: u32,
    reordering: u32,
    rcv_rtt: u32,
    rcv_space: u32,
    total_retrans: u32,
    pacing_rate: u64,
    max_pacing_rate: u64,
    bytes_acked: u64,
    bytes_received: u64,
    segs_out: u32,
    segs_in: u32,
    notsent_bytes: u32,
    min_rtt: u32,
    data_segs_in: u32,
    data_segs_out: u32,
    delivery_rate: u64,
}

/// Kernel view of a TCP connection, for troubleshooting logs.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpInfo {
    pub rtt_ms: f32,
    pub rttvar_ms: f32,
    pub min_rtt_ms: f32,
    pub cwnd: u32,
    pub unacked: u32,
    pub notsent_bytes: u32,
    pub total_retrans: u32,
    pub delivery_rate_kbit: u64,
    pub bytes_acked: u64,
    pub bytes_received: u64,
}

#[cfg(target_os = "linux")]
pub fn tcp_info(fd: std::os::fd::RawFd) -> Option<TcpInfo> {
    let mut raw = RawTcpInfo::default();
    let mut len = std::mem::size_of::<RawTcpInfo>() as libc::socklen_t;
    // SAFETY: `raw` is a plain-old-data buffer of `len` bytes.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            &mut raw as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    Some(TcpInfo {
        rtt_ms: raw.rtt as f32 / 1000.0,
        rttvar_ms: raw.rttvar as f32 / 1000.0,
        min_rtt_ms: raw.min_rtt as f32 / 1000.0,
        cwnd: raw.snd_cwnd,
        unacked: raw.unacked,
        notsent_bytes: raw.notsent_bytes,
        total_retrans: raw.total_retrans,
        delivery_rate_kbit: raw.delivery_rate * 8 / 1000,
        bytes_acked: raw.bytes_acked,
        bytes_received: raw.bytes_received,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn tcp_info(_fd: i32) -> Option<TcpInfo> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[tokio::test]
    async fn tcp_info_reads_live_connection() {
        use std::os::fd::AsRawFd;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let c = tokio::net::TcpStream::connect(l.local_addr().unwrap())
            .await
            .unwrap();
        let _s = l.accept().await.unwrap();
        let i = super::tcp_info(c.as_raw_fd()).expect("tcp_info");
        assert!(i.cwnd > 0);
    }
}
