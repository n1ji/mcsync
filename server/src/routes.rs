use crate::auth::Device;
use crate::db::{CommitResult, LockResult};
use crate::{valid_world_id, AppState};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// JSON request bodies are tiny; only snapshot uploads get a larger limit.
const SMALL_BODY_LIMIT: usize = 16 * 1024;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, error: &str) -> Self {
        ApiError { status, body: json!({ "error": error }) }
    }
    fn with(status: StatusCode, body: Value) -> Self {
        ApiError { status, body }
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }
    pub fn internal(e: impl std::fmt::Display) -> Self {
        tracing::error!("internal error: {e}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal")
    }
    fn locked(holder: String) -> Self {
        Self::with(StatusCode::CONFLICT, json!({ "error": "locked", "holder": holder }))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e)
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn check_id(id: &str) -> ApiResult<()> {
    if valid_world_id(id) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_world_id"))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let upload_limit = usize::try_from(state.cfg.max_upload_bytes).unwrap_or(usize::MAX);
    let snapshot = Router::new()
        .route("/v1/worlds/{id}/snapshot", put(push_snapshot).get(pull_snapshot))
        .layer(DefaultBodyLimit::max(upload_limit));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/whoami", get(whoami))
        .route("/v1/worlds", get(list_worlds))
        .route("/v1/worlds/{id}", get(get_world))
        .route("/v1/worlds/{id}/lock", post(lock).delete(unlock))
        .route("/v1/worlds/{id}/heartbeat", post(lock))
        .layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT))
        .merge(snapshot)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn whoami(device: Device) -> Json<Value> {
    Json(json!({ "device": device.name }))
}

async fn list_worlds(State(s): State<Arc<AppState>>, _d: Device) -> ApiResult<Json<Value>> {
    Ok(Json(serde_json::to_value(s.db.list_worlds()?).map_err(ApiError::internal)?))
}

async fn get_world(State(s): State<Arc<AppState>>, _d: Device, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    check_id(&id)?;
    match s.db.world(&id)? {
        Some(w) => Ok(Json(serde_json::to_value(w).map_err(ApiError::internal)?)),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found")),
    }
}

async fn lock(State(s): State<Arc<AppState>>, d: Device, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    check_id(&id)?;
    match s.db.lock(&id, &d.name, s.cfg.lock_ttl_secs)? {
        LockResult::Acquired(l) => Ok(Json(json!({ "device": l.device, "expires_at": l.expires_at }))),
        LockResult::HeldBy(holder) => Err(ApiError::locked(holder)),
    }
}

async fn unlock(State(s): State<Arc<AppState>>, d: Device, Path(id): Path<String>) -> ApiResult<StatusCode> {
    check_id(&id)?;
    match s.db.unlock(&id, &d.name)? {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(holder) => Err(ApiError::locked(holder)),
    }
}

#[derive(Deserialize)]
struct PushQuery {
    base_version: u64,
    #[serde(default)]
    force: bool,
}

fn snapshot_path(s: &AppState, id: &str, version: u64) -> std::path::PathBuf {
    s.cfg.data_dir.join("snapshots").join(id).join(format!("{version}.tar.zst"))
}

async fn push_snapshot(
    State(s): State<Arc<AppState>>,
    d: Device,
    Path(id): Path<String>,
    Query(q): Query<PushQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    check_id(&id)?;
    let max = s.cfg.max_upload_bytes;
    if let Some(len) = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok()) {
        if len > max {
            return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
        }
    }
    // Fail fast before reading a large body.
    let holder = s.db.lock_holder(&id)?;
    if holder.as_deref() != Some(d.name.as_str()) {
        return Err(lock_required(holder));
    }
    let current = s.db.current_version(&id)?;
    if !q.force && q.base_version != current {
        return Err(conflict(current, q.base_version));
    }

    // Stream to a temp file, counting bytes and hashing.
    let tmp_path = s.cfg.data_dir.join("tmp").join(format!("{}.part", crate::generate_token()));
    let result = receive(body, &tmp_path, max).await;
    let (size, sha) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }
    };

    let dir = s.cfg.data_dir.join("snapshots").join(&id);
    tokio::fs::create_dir_all(&dir).await.map_err(ApiError::internal)?;
    let s2 = s.clone();
    let id2 = id.clone();
    let tmp2 = tmp_path.clone();
    let device = d.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        s2.db.commit_snapshot(&id2, &device, q.base_version, q.force, size, &sha, s2.cfg.keep_snapshots, |v| {
            std::fs::rename(&tmp2, snapshot_path(&s2, &id2, v))
        })
    })
    .await
    .map_err(ApiError::internal)??;

    match res {
        CommitResult::Committed { version, pruned } => {
            for v in pruned {
                let _ = tokio::fs::remove_file(snapshot_path(&s, &id, v)).await;
            }
            Ok(Json(json!({ "version": version })))
        }
        CommitResult::Conflict { server_version } => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(conflict(server_version, q.base_version))
        }
        CommitResult::NotLockHolder { holder } => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(lock_required(holder))
        }
    }
}

fn lock_required(holder: Option<String>) -> ApiError {
    match holder {
        Some(h) => ApiError::locked(h),
        None => ApiError::new(StatusCode::CONFLICT, "lock_required"),
    }
}

fn conflict(server: u64, base: u64) -> ApiError {
    ApiError::with(
        StatusCode::CONFLICT,
        json!({ "error": "conflict", "server_version": server, "base_version": base }),
    )
}

async fn receive(body: Body, path: &std::path::Path, max: u64) -> ApiResult<(u64, String)> {
    let mut file = tokio::fs::File::create(path).await.map_err(ApiError::internal)?;
    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut head: Vec<u8> = Vec::with_capacity(4);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            // Body limit hits surface as stream errors.
            tracing::debug!("upload stream error: {e}");
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large_or_aborted")
        })?;
        size += chunk.len() as u64;
        if size > max {
            return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
        }
        if head.len() < 4 {
            head.extend(chunk.iter().take(4 - head.len()));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
    }
    file.flush().await.map_err(ApiError::internal)?;
    if head != ZSTD_MAGIC {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "not_zstd"));
    }
    Ok((size, hex::encode(hasher.finalize())))
}

#[derive(Deserialize)]
struct PullQuery {
    version: Option<u64>,
}

async fn pull_snapshot(
    State(s): State<Arc<AppState>>,
    _d: Device,
    Path(id): Path<String>,
    Query(q): Query<PullQuery>,
) -> ApiResult<Response> {
    check_id(&id)?;
    let Some((version, size, sha)) = s.db.snapshot_meta(&id, q.version)? else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    };
    let file = tokio::fs::File::open(snapshot_path(&s, &id, version)).await.map_err(ApiError::internal)?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Body::from_stream(stream).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zstd"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert("x-world-version", HeaderValue::from(version));
    h.insert("x-sha256", HeaderValue::from_str(&sha).map_err(ApiError::internal)?);
    Ok(resp)
}
