use clap::{Args, Parser, Subcommand};
use std::net::SocketAddr;
use std::time::Duration;
use wg_tcp_relay::common::Tuning;
use wg_tcp_relay::queue::Limits;
use wg_tcp_relay::{client, server};

/// WireGuard-aware UDP-over-TCP relay.
///
/// Carries WireGuard traffic of many peers over a single TCP stream (e.g. an SSH or Cloak
/// tunnel) without stalling: queues are bounded, stale packets are dropped instead of
/// piling up, handshakes and keepalives always go first.
#[derive(Parser)]
#[command(version, about, long_about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    #[command(flatten)]
    tune: TuneArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Listen for WireGuard peers on UDP, forward them over TCP to a relay server.
    Client {
        /// UDP address WireGuard peers connect to.
        #[arg(long, env = "WGTR_LISTEN", default_value = "0.0.0.0:51820")]
        listen: SocketAddr,
        /// Relay server TCP address (host:port).
        #[arg(long, env = "WGTR_CONNECT")]
        connect: String,
    },
    /// Accept relay clients on TCP, forward their traffic to a WireGuard server over UDP.
    Server {
        /// TCP address to accept relay clients on.
        #[arg(long, env = "WGTR_LISTEN", default_value = "0.0.0.0:5000")]
        listen: SocketAddr,
        /// WireGuard server UDP address (host:port), resolved for every new peer.
        #[arg(long, env = "WGTR_TARGET")]
        target: String,
    },
}

#[derive(Args)]
struct TuneArgs {
    /// Data packets that waited longer than this in the queue are dropped.
    #[arg(long, global = true, env = "WGTR_MAX_DELAY_MS", default_value_t = 150)]
    max_delay_ms: u64,
    /// Queue limit per peer, KiB.
    #[arg(long, global = true, env = "WGTR_PEER_QUEUE_KB", default_value_t = 256)]
    peer_queue_kb: usize,
    /// Ping interval, seconds.
    #[arg(long, global = true, env = "WGTR_KEEPALIVE_SECS", default_value_t = 5)]
    keepalive_secs: u64,
    /// Drop the TCP connection if nothing was received for this long, seconds.
    #[arg(
        long,
        global = true,
        env = "WGTR_DEAD_AFTER_SECS",
        default_value_t = 15
    )]
    dead_after_secs: u64,
    /// Forget peers idle for this long, seconds.
    #[arg(
        long,
        global = true,
        env = "WGTR_PEER_TIMEOUT_SECS",
        default_value_t = 180
    )]
    peer_timeout_secs: u64,
    /// TCP congestion control for the relay connection; empty for the system default.
    /// BBR keeps bottleneck queues short, which is what tunneled traffic needs.
    #[arg(long, global = true, env = "WGTR_CONGESTION", default_value = "bbr")]
    congestion: String,
    /// Cap the relay's TCP sending rate, Mbit/s (0 = no cap). Set it a bit (~10%) below the
    /// real capacity of the link between relay client and server: then the outer TCP never
    /// overflows the link, and overload turns into cheap drops instead of stalls.
    #[arg(long, global = true, env = "WGTR_MAX_RATE_MBIT", default_value_t = 0.0)]
    max_rate_mbit: f64,
    /// Log traffic/drop counters every N seconds (0 disables).
    #[arg(long, global = true, env = "WGTR_STATS_SECS", default_value_t = 60)]
    stats_secs: u64,
}

impl TuneArgs {
    fn tuning(&self) -> Tuning {
        Tuning {
            limits: Limits {
                peer_bytes: self.peer_queue_kb * 1024,
                max_delay: Duration::from_millis(self.max_delay_ms),
                ..Limits::default()
            },
            keepalive: Duration::from_secs(self.keepalive_secs.max(1)),
            dead_after: Duration::from_secs(self.dead_after_secs.max(2)),
            peer_timeout: Duration::from_secs(self.peer_timeout_secs.max(10)),
            stats_every: Duration::from_secs(self.stats_secs),
            congestion: Some(self.congestion.trim().to_string()).filter(|c| !c.is_empty()),
            max_rate_mbit: self.max_rate_mbit.max(0.0),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let tune = cli.tune.tuning();
    let app = async move {
        match cli.cmd {
            Cmd::Client { listen, connect } => {
                client::run(client::Config {
                    listen,
                    connect,
                    tune,
                })
                .await
            }
            Cmd::Server { listen, target } => {
                server::run(server::Config {
                    listen,
                    target,
                    tune,
                })
                .await
            }
        }
    };
    tokio::select! {
        r = app => r,
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// As PID 1 in a container we must handle SIGTERM ourselves.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
