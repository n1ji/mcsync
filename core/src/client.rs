//! HTTP client for the mcsync world server.

use crate::error::{Error, IoContext, Result};
use crate::resolve::{read_json, API_TIMEOUT, MAX_API_RESPONSE};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockInfo {
    pub device: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorldInfo {
    pub id: String,
    pub version: u64,
    pub size: u64,
    pub updated_at: i64,
    pub updated_by: Option<String>,
    pub lock: Option<LockInfo>,
    #[serde(default)]
    pub versions: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PushResponse {
    pub version: u64,
}

/// Largest world snapshot the client will download.
pub const MAX_WORLD_DOWNLOAD: u64 = 16 * 1024 * 1024 * 1024;

pub struct SyncClient {
    http: reqwest::Client,
    base: url::Url,
    token: String,
}

impl SyncClient {
    pub fn new(server_url: &str, token: &str) -> Result<Self> {
        let mut base = url::Url::parse(server_url)
            .map_err(|e| Error::Invalid(format!("bad server url {server_url}: {e}")))?;
        let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if base.scheme() != "https" && !(base.scheme() == "http" && loopback) {
            return Err(Error::Invalid("server url must use https (http only for localhost)".into()));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(SyncClient { http: crate::resolve::http_client(), base, token: token.to_string() })
    }

    fn url(&self, path: &str) -> url::Url {
        self.base.join(path).expect("relative api path")
    }

    fn world_url(&self, id: &str, suffix: &str) -> url::Url {
        let mut u = self.url("v1/worlds/");
        u.path_segments_mut().expect("base url").pop_if_empty().push(id);
        if !suffix.is_empty() {
            u.path_segments_mut().expect("base url").push(suffix);
        }
        u
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = crate::resolve::error_body(resp).await;
        #[derive(Deserialize)]
        struct ErrBody {
            error: String,
            holder: Option<String>,
            server_version: Option<u64>,
            base_version: Option<u64>,
        }
        if let Ok(e) = serde_json::from_str::<ErrBody>(&body) {
            match e.error.as_str() {
                "locked" => return Err(Error::Locked { holder: e.holder.unwrap_or_default() }),
                "conflict" => {
                    return Err(Error::Conflict { server: e.server_version.unwrap_or(0), base: e.base_version })
                }
                _ => {}
            }
        }
        Err(Error::Server { status: status.as_u16(), body })
    }

    pub async fn health(&self) -> Result<()> {
        Self::check(self.http.get(self.url("health")).timeout(API_TIMEOUT).send().await?).await?;
        Ok(())
    }

    pub async fn whoami(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Me {
            device: String,
        }
        let resp = self.http.get(self.url("v1/whoami")).bearer_auth(&self.token).timeout(API_TIMEOUT).send().await?;
        Ok(read_json::<Me>(Self::check(resp).await?, MAX_API_RESPONSE).await?.device)
    }

    pub async fn list_worlds(&self) -> Result<Vec<WorldInfo>> {
        let resp = self.http.get(self.url("v1/worlds")).bearer_auth(&self.token).timeout(API_TIMEOUT).send().await?;
        read_json(Self::check(resp).await?, MAX_API_RESPONSE).await
    }

    /// World metadata, or None if the server has never seen it.
    pub async fn world(&self, id: &str) -> Result<Option<WorldInfo>> {
        let resp = self.http.get(self.world_url(id, "")).bearer_auth(&self.token).timeout(API_TIMEOUT).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(read_json(Self::check(resp).await?, MAX_API_RESPONSE).await?))
    }

    pub async fn lock(&self, id: &str) -> Result<LockInfo> {
        let resp = self.http.post(self.world_url(id, "lock")).bearer_auth(&self.token).timeout(API_TIMEOUT).send().await?;
        read_json(Self::check(resp).await?, MAX_API_RESPONSE).await
    }

    pub async fn heartbeat(&self, id: &str) -> Result<LockInfo> {
        let resp = self.http.post(self.world_url(id, "heartbeat")).bearer_auth(&self.token).timeout(API_TIMEOUT).send().await?;
        read_json(Self::check(resp).await?, MAX_API_RESPONSE).await
    }

    pub async fn unlock(&self, id: &str) -> Result<()> {
        let resp = self.http.delete(self.world_url(id, "lock")).bearer_auth(&self.token).timeout(API_TIMEOUT).send().await?;
        Self::check(resp).await?;
        Ok(())
    }

    /// Upload a snapshot. `base` is the version the local copy came from;
    /// `force` overwrites a newer server version (the server keeps history).
    pub async fn push(&self, id: &str, archive: &Path, base: Option<u64>, force: bool) -> Result<u64> {
        let file = tokio::fs::File::open(archive).await.at(archive)?;
        let len = file.metadata().await.at(archive)?.len();
        let stream = tokio_util_stream(file);
        let mut url = self.world_url(id, "snapshot");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("base_version", &base.unwrap_or(0).to_string());
            if force {
                q.append_pair("force", "true");
            }
        }
        let resp = self
            .http
            .put(url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .header(reqwest::header::CONTENT_TYPE, "application/zstd")
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await?;
        Ok(read_json::<PushResponse>(Self::check(resp).await?, MAX_API_RESPONSE).await?.version)
    }

    /// Download a snapshot (latest if `version` is None) to `out`.
    pub async fn pull(&self, id: &str, version: Option<u64>, out: &Path) -> Result<u64> {
        let mut url = self.world_url(id, "snapshot");
        if let Some(v) = version {
            url.query_pairs_mut().append_pair("version", &v.to_string());
        }
        let resp = Self::check(self.http.get(url).bearer_auth(&self.token).send().await?).await?;
        let v: u64 = resp
            .headers()
            .get("x-world-version")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::Invalid("server did not send x-world-version".into()))?;
        let declared = resp.content_length().unwrap_or(MAX_WORLD_DOWNLOAD);
        if declared > MAX_WORLD_DOWNLOAD {
            return Err(Error::Invalid(format!("world snapshot larger than {MAX_WORLD_DOWNLOAD} bytes")));
        }
        let mut f = tokio::fs::File::create(out).await.at(out)?;
        let mut stream = resp.bytes_stream();
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            written += chunk.len() as u64;
            if written > declared {
                return Err(Error::Invalid("world snapshot exceeded its declared size".into()));
            }
            f.write_all(&chunk).await.at(out)?;
        }
        f.flush().await.at(out)?;
        Ok(v)
    }
}

fn tokio_util_stream(
    mut file: tokio::fs::File,
) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
    use tokio::io::AsyncReadExt;
    async_stream_lite(move |tx| async move {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            tx.send(Ok(buf[..n].to_vec())).await.ok();
        }
    })
}

/// Minimal channel-backed stream so we don't need tokio-util.
fn async_stream_lite<F, Fut>(f: F) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static
where
    F: FnOnce(tokio::sync::mpsc::Sender<std::io::Result<Vec<u8>>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let err_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = f(tx).await {
            let _ = err_tx.send(Err(e)).await;
        }
    });
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
