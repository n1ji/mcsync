//! HTTP-level tests: auth, locks, push/pull, conflicts, retention, limits.

mod common;

use reqwest::StatusCode;
use serde_json::Value;

const W: &str = "pack__world-1a2b3c4d";

struct Api {
    http: reqwest::Client,
    url: String,
}

impl Api {
    fn new(url: &str) -> Self {
        Api { http: reqwest::Client::new(), url: url.into() }
    }
    async fn lock(&self, tok: &str, id: &str) -> reqwest::Response {
        self.http.post(format!("{}/v1/worlds/{id}/lock", self.url)).bearer_auth(tok).send().await.unwrap()
    }
    async fn unlock(&self, tok: &str, id: &str) -> reqwest::Response {
        self.http.delete(format!("{}/v1/worlds/{id}/lock", self.url)).bearer_auth(tok).send().await.unwrap()
    }
    async fn push(&self, tok: &str, id: &str, body: Vec<u8>, base: u64, force: bool) -> reqwest::Response {
        self.http
            .put(format!("{}/v1/worlds/{id}/snapshot?base_version={base}&force={force}", self.url))
            .bearer_auth(tok)
            .body(body)
            .send()
            .await
            .unwrap()
    }
    async fn pull(&self, tok: &str, id: &str, version: Option<u64>) -> reqwest::Response {
        let q = version.map(|v| format!("?version={v}")).unwrap_or_default();
        self.http.get(format!("{}/v1/worlds/{id}/snapshot{q}", self.url)).bearer_auth(tok).send().await.unwrap()
    }
    async fn world(&self, tok: &str, id: &str) -> Value {
        self.http.get(format!("{}/v1/worlds/{id}", self.url)).bearer_auth(tok).send().await.unwrap().json().await.unwrap()
    }
}

async fn err(resp: reqwest::Response) -> (StatusCode, Value) {
    let s = resp.status();
    (s, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn auth_is_required_and_checked() {
    let srv = common::start(|_| {}).await;
    let api = Api::new(&srv.url);
    assert_eq!(api.http.get(format!("{}/health", srv.url)).send().await.unwrap().status(), StatusCode::OK);
    let r = api.http.get(format!("{}/v1/worlds", srv.url)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = api.http.get(format!("{}/v1/worlds", srv.url)).bearer_auth("mcs_wrong").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = api.http.get(format!("{}/v1/whoami", srv.url)).bearer_auth(&srv.tokens[0]).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["device"], "laptop");

    // Revoked tokens stop working.
    let db = mcsync_server::Db::open(&srv.dir.path().join("mcsync.db")).unwrap();
    db.revoke_device("mac").unwrap();
    let r = api.http.get(format!("{}/v1/whoami", srv.url)).bearer_auth(&srv.tokens[1]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn repeated_auth_failures_are_rate_limited() {
    let srv = common::start(|_| {}).await;
    let api = Api::new(&srv.url);
    let mut last = StatusCode::OK;
    for _ in 0..25 {
        last = api.http.get(format!("{}/v1/worlds", srv.url)).bearer_auth("mcs_bad").send().await.unwrap().status();
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    // Real devices on the same address still get in.
    let r = api.http.get(format!("{}/v1/whoami", srv.url)).bearer_auth(&srv.tokens[0]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn lock_is_exclusive_and_push_requires_it() {
    let srv = common::start(|_| {}).await;
    let api = Api::new(&srv.url);
    let (laptop, mac) = (&srv.tokens[0], &srv.tokens[1]);
    let archive = std::fs::read(common::world_archive(srv.dir.path(), "a", b"v1")).unwrap();

    // No lock -> refused.
    let (s, b) = err(api.push(laptop, W, archive.clone(), 0, false).await).await;
    assert_eq!((s, b["error"].as_str()), (StatusCode::CONFLICT, Some("lock_required")));

    assert_eq!(api.lock(laptop, W).await.status(), StatusCode::OK);
    assert_eq!(api.lock(laptop, W).await.status(), StatusCode::OK, "re-lock by holder refreshes");
    let (s, b) = err(api.lock(mac, W).await).await;
    assert_eq!((s, b["holder"].as_str()), (StatusCode::CONFLICT, Some("laptop")));

    // Non-holder push is refused even with a correct base.
    let (s, b) = err(api.push(mac, W, archive.clone(), 0, false).await).await;
    assert_eq!((s, b["error"].as_str()), (StatusCode::CONFLICT, Some("locked")));
    // Non-holder can't release it either.
    assert_eq!(api.unlock(mac, W).await.status(), StatusCode::CONFLICT);

    let r = api.push(laptop, W, archive.clone(), 0, false).await;
    assert_eq!(r.json::<Value>().await.unwrap()["version"], 1);
    assert_eq!(api.unlock(laptop, W).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(api.lock(mac, W).await.status(), StatusCode::OK);

    let r = api.pull(laptop, W, None).await;
    assert_eq!(r.headers()["x-world-version"], "1");
    assert_eq!(r.bytes().await.unwrap().to_vec(), archive);
}

#[tokio::test]
async fn stale_base_conflicts_and_force_keeps_history() {
    let srv = common::start(|_| {}).await;
    let api = Api::new(&srv.url);
    let laptop = &srv.tokens[0];
    let v1 = std::fs::read(common::world_archive(srv.dir.path(), "1", b"one")).unwrap();
    let v2 = std::fs::read(common::world_archive(srv.dir.path(), "2", b"two")).unwrap();
    api.lock(laptop, W).await;
    api.push(laptop, W, v1.clone(), 0, false).await;
    api.push(laptop, W, v2.clone(), 1, false).await;

    let (s, b) = err(api.push(laptop, W, v1.clone(), 1, false).await).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"], "conflict");
    assert_eq!(b["server_version"], 2);

    let r = api.push(laptop, W, v1.clone(), 1, true).await;
    assert_eq!(r.json::<Value>().await.unwrap()["version"], 3);
    // v2 is still downloadable for rollback.
    assert_eq!(api.pull(laptop, W, Some(2)).await.bytes().await.unwrap().to_vec(), v2);
    assert_eq!(api.world(laptop, W).await["versions"], serde_json::json!([1, 2, 3]));
}

#[tokio::test]
async fn keeps_only_last_n_snapshots() {
    let srv = common::start(|c| c.keep_snapshots = 2).await;
    let api = Api::new(&srv.url);
    let t = &srv.tokens[0];
    let a = std::fs::read(common::world_archive(srv.dir.path(), "a", b"a")).unwrap();
    api.lock(t, W).await;
    for base in 0..4 {
        assert!(api.push(t, W, a.clone(), base, false).await.status().is_success());
    }
    assert_eq!(api.world(t, W).await["versions"], serde_json::json!([3, 4]));
    assert_eq!(api.pull(t, W, Some(1)).await.status(), StatusCode::NOT_FOUND);
    let files = std::fs::read_dir(srv.dir.path().join("snapshots").join(W)).unwrap().count();
    assert_eq!(files, 2);
}

#[tokio::test]
async fn locks_expire_without_heartbeat() {
    let srv = common::start(|c| c.lock_ttl_secs = 1).await;
    let api = Api::new(&srv.url);
    api.lock(&srv.tokens[0], W).await;
    assert_eq!(api.lock(&srv.tokens[1], W).await.status(), StatusCode::CONFLICT);
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    assert_eq!(api.lock(&srv.tokens[1], W).await.status(), StatusCode::OK);
    // The old holder's heartbeat no longer works.
    let r = api.http.post(format!("{}/v1/worlds/{W}/heartbeat", srv.url)).bearer_auth(&srv.tokens[0]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn rejects_bad_world_ids_and_paths() {
    let srv = common::start(|_| {}).await;
    let api = Api::new(&srv.url);
    let t = &srv.tokens[0];
    for id in ["..", "%2e%2e", "..%2F..%2Fetc", "Upper", ".hidden", "a%5Cb", "a..b"] {
        let r = api.lock(t, id).await;
        assert!(
            r.status() == StatusCode::BAD_REQUEST || r.status() == StatusCode::NOT_FOUND,
            "{id} -> {}",
            r.status()
        );
    }
    assert!(!srv.dir.path().join("snapshots").join("..").join("etc").exists());
}

#[tokio::test]
async fn enforces_upload_limit_and_format() {
    let srv = common::start(|c| c.max_upload_bytes = 1024).await;
    let api = Api::new(&srv.url);
    let t = &srv.tokens[0];
    api.lock(t, W).await;
    let (s, _) = err(api.push(t, W, vec![0u8; 4096], 0, false).await).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    let (s, b) = err(api.push(t, W, b"not zstd at all".to_vec(), 0, false).await).await;
    assert_eq!((s, b["error"].as_str()), (StatusCode::BAD_REQUEST, Some("not_zstd")));
    // Nothing was committed and no temp files linger.
    assert_eq!(api.world(t, W).await["version"], 0);
    assert_eq!(std::fs::read_dir(srv.dir.path().join("tmp")).unwrap().count(), 0);
}
