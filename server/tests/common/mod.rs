#![allow(dead_code)]

use mcsync_server::{app, generate_token, hash_token, Config, Db};
use std::net::SocketAddr;
use std::path::Path;

pub struct TestServer {
    pub url: String,
    pub tokens: Vec<String>,
    pub dir: tempfile::TempDir,
}

/// Start a server on a random port with devices `laptop` and `mac`.
pub async fn start(tweak: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config { data_dir: dir.path().to_path_buf(), ..Config::default() };
    tweak(&mut cfg);
    let router = app(cfg).unwrap();
    let db = Db::open(&dir.path().join("mcsync.db")).unwrap();
    let mut tokens = Vec::new();
    for name in ["laptop", "mac"] {
        let t = generate_token();
        db.add_device(name, &hash_token(&t)).unwrap();
        tokens.push(t);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    TestServer { url: format!("http://{addr}"), tokens, dir }
}

/// A small valid world archive (tar.zst) with the given level.dat content.
pub fn world_archive(dir: &Path, name: &str, level: &[u8]) -> std::path::PathBuf {
    let world = dir.join(format!("src-{name}"));
    std::fs::create_dir_all(&world).unwrap();
    std::fs::write(world.join("level.dat"), level).unwrap();
    let out = dir.join(format!("{name}.tar.zst"));
    mcsync_core::world::pack_world(&world, &out).unwrap();
    out
}
