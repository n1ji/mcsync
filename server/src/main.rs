use anyhow::Result;
use clap::{Parser, Subcommand};
use mcsync_server::{app, generate_token, hash_token, Config, Db};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mcsync-server", version, about = "mcsync world snapshot + lock server")]
struct Cli {
    /// Data directory (snapshots + database)
    #[arg(long, env = "MCSYNC_DATA", default_value = "/data", global = true)]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server
    Serve {
        #[arg(long, env = "MCSYNC_BIND", default_value = "0.0.0.0:8080")]
        bind: SocketAddr,
        /// Snapshots kept per world
        #[arg(long, env = "MCSYNC_KEEP_SNAPSHOTS", default_value_t = 10)]
        keep: u64,
        /// Seconds a lock survives without a heartbeat
        #[arg(long, env = "MCSYNC_LOCK_TTL", default_value_t = 90)]
        lock_ttl: i64,
        /// Max upload size in MiB
        #[arg(long, env = "MCSYNC_MAX_UPLOAD_MB", default_value_t = 4096)]
        max_upload_mb: u64,
        /// Trust CF-Connecting-IP for rate limiting (only behind Cloudflare)
        #[arg(long, env = "MCSYNC_TRUST_CF_HEADER", default_value_t = false)]
        trust_cf_header: bool,
    },
    /// Issue (or re-issue) a token for a device. Printed once; only its hash is stored.
    IssueToken {
        #[arg(long)]
        device: String,
    },
    /// Revoke a device's token
    Revoke {
        #[arg(long)]
        device: String,
    },
    /// List devices
    ListDevices,
    /// Exit 0 if the local server answers /health (for container healthchecks)
    Healthcheck {
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: SocketAddr,
    },
}

fn healthcheck(addr: SocketAddr) -> Result<()> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut buf = [0u8; 12];
    s.read_exact(&mut buf)?;
    anyhow::ensure!(buf.ends_with(b"200"), "unhealthy");
    Ok(())
}

fn valid_device_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    if let Cmd::Healthcheck { addr } = cli.cmd {
        return healthcheck(addr);
    }
    std::fs::create_dir_all(&cli.data)?;
    match cli.cmd {
        Cmd::Serve { bind, keep, lock_ttl, max_upload_mb, trust_cf_header } => {
            anyhow::ensure!(keep >= 1, "keep must be at least 1");
            anyhow::ensure!(lock_ttl >= 10, "lock ttl must be at least 10s");
            let cfg = Config {
                data_dir: cli.data,
                keep_snapshots: keep,
                lock_ttl_secs: lock_ttl,
                max_upload_bytes: max_upload_mb * 1024 * 1024,
                trust_cf_header,
            };
            let router = app(cfg)?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!("listening on {bind}");
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Cmd::IssueToken { device } => {
            anyhow::ensure!(valid_device_name(&device), "device name: 1-64 chars of A-Z a-z 0-9 - _ .");
            let db = Db::open(&cli.data.join("mcsync.db"))?;
            let token = generate_token();
            db.add_device(&device, &hash_token(&token))?;
            println!("Device '{device}' token (shown once, store it in the mcsync app):\n{token}");
        }
        Cmd::Revoke { device } => {
            let db = Db::open(&cli.data.join("mcsync.db"))?;
            if db.revoke_device(&device)? {
                println!("Revoked '{device}'.");
            } else {
                anyhow::bail!("no device '{device}'");
            }
        }
        Cmd::ListDevices => {
            let db = Db::open(&cli.data.join("mcsync.db"))?;
            for d in db.list_devices()? {
                println!(
                    "{}\tcreated {}\tlast seen {}\t{}",
                    d.name,
                    d.created_at,
                    d.last_seen.map(|v| v.to_string()).unwrap_or_else(|| "never".into()),
                    if d.revoked { "REVOKED" } else { "active" }
                );
            }
        }
        Cmd::Healthcheck { .. } => unreachable!("handled above"),
    }
    Ok(())
}
