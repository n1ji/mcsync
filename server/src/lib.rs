//! mcsync world server.
//!
//! Stores world snapshots (`tar.zst`) with a version counter, keeps the last
//! N per world, and arbitrates a per-world lock so only one device plays a
//! world at a time. Every `/v1` route needs a per-device bearer token.
//!
//! Designed to sit behind a Cloudflare Tunnel: no LAN trust, strict request
//! size limits, strict world id validation.

mod auth;
mod db;
mod routes;

pub use auth::{generate_token, hash_token};
pub use db::Db;

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Snapshots kept per world.
    pub keep_snapshots: u64,
    /// Seconds a lock lives without a heartbeat.
    pub lock_ttl_secs: i64,
    /// Max snapshot upload size in bytes.
    pub max_upload_bytes: u64,
    /// Use `CF-Connecting-IP` as the client address for auth rate limiting.
    /// Only enable when the server is reachable solely through Cloudflare.
    pub trust_cf_header: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("/data"),
            keep_snapshots: 10,
            lock_ttl_secs: 90,
            max_upload_bytes: 4 * 1024 * 1024 * 1024,
            trust_cf_header: false,
        }
    }
}

pub struct AppState {
    pub cfg: Config,
    pub db: Db,
    pub limiter: auth::FailureLimiter,
}

pub fn app(cfg: Config) -> anyhow::Result<axum::Router> {
    std::fs::create_dir_all(cfg.data_dir.join("snapshots"))?;
    std::fs::create_dir_all(cfg.data_dir.join("tmp"))?;
    let db = Db::open(&cfg.data_dir.join("mcsync.db"))?;
    let state = Arc::new(AppState { cfg, db, limiter: auth::FailureLimiter::default() });
    Ok(routes::router(state))
}

/// Valid world ids: 1-128 chars of `[a-z0-9._-]`, starting with a letter or
/// digit, and no `..`. Rejects anything that could act as a path.
pub fn valid_world_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_ids() {
        assert!(valid_world_id("my-pack__world-1a2b3c4d"));
        assert!(!valid_world_id(""));
        assert!(!valid_world_id(".."));
        assert!(!valid_world_id("a..b"));
        assert!(!valid_world_id(".hidden"));
        assert!(!valid_world_id("a/b"));
        assert!(!valid_world_id("a\\b"));
        assert!(!valid_world_id("World"));
        assert!(!valid_world_id(&"a".repeat(129)));
    }
}
