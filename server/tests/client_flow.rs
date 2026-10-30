//! End-to-end: two devices syncing one world through the core client and
//! its conflict handling.

mod common;

use mcsync_core::client::SyncClient;
use mcsync_core::state::SyncState;
use mcsync_core::sync::{pull_world, push_world, Outcome, Resolution, WorldRef};
use std::path::Path;

fn make_world(game: &Path, level: &[u8]) {
    let w = game.join("saves/World");
    std::fs::create_dir_all(w.join("region")).unwrap();
    std::fs::write(w.join("level.dat"), level).unwrap();
    std::fs::write(w.join("region/r.0.0.mca"), vec![1u8; 2048]).unwrap();
}

fn level(game: &Path) -> Vec<u8> {
    std::fs::read(game.join("saves/World/level.dat")).unwrap()
}

#[tokio::test]
async fn two_devices_sync_and_resolve_conflicts() {
    let srv = common::start(|_| {}).await;
    let laptop = SyncClient::new(&srv.url, &srv.tokens[0]).unwrap();
    let mac = SyncClient::new(&srv.url, &srv.tokens[1]).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let (game_a, game_b) = (tmp.path().join("a"), tmp.path().join("b"));
    let (mut st_a, mut st_b) = (SyncState::default(), SyncState::default());
    let id = mcsync_core::world::world_id("Pack", "World");
    let wa = WorldRef { game_dir: &game_a, world: "World", id: id.clone() };
    let wb = WorldRef { game_dir: &game_b, world: "World", id: id.clone() };

    assert_eq!(pull_world(&mac, &mut st_b, &wb, None).await.unwrap(), Outcome::NothingOnServer);

    // Laptop creates and pushes v1.
    make_world(&game_a, b"laptop-1");
    laptop.lock(&id).await.unwrap();
    assert!(matches!(push_world(&laptop, &mut st_a, &wa, None).await.unwrap(), Outcome::Pushed { version: 1, .. }));
    // Mac can't take the lock while the laptop plays.
    assert!(matches!(mac.lock(&id).await, Err(mcsync_core::Error::Locked { .. })));
    laptop.unlock(&id).await.unwrap();

    // Mac pulls v1, plays, pushes v2.
    assert!(matches!(pull_world(&mac, &mut st_b, &wb, None).await.unwrap(), Outcome::Pulled { version: 1, .. }));
    assert_eq!(level(&game_b), b"laptop-1");
    mac.lock(&id).await.unwrap();
    std::fs::write(game_b.join("saves/World/level.dat"), b"mac-2").unwrap();
    assert!(matches!(push_world(&mac, &mut st_b, &wb, None).await.unwrap(), Outcome::Pushed { version: 2, .. }));
    mac.unlock(&id).await.unwrap();

    // Laptop played offline meanwhile: both sides changed -> conflict.
    std::fs::write(game_a.join("saves/World/level.dat"), b"laptop-offline").unwrap();
    assert_eq!(
        pull_world(&laptop, &mut st_a, &wa, None).await.unwrap(),
        Outcome::Conflict { server_version: 2, base_version: Some(1) }
    );
    laptop.lock(&id).await.unwrap();
    assert_eq!(
        push_world(&laptop, &mut st_a, &wa, None).await.unwrap(),
        Outcome::Conflict { server_version: 2, base_version: Some(1) }
    );
    assert_eq!(level(&game_a), b"laptop-offline", "nothing overwritten on conflict");

    // Keep local: server copy is saved locally, then local is force-pushed.
    let Outcome::Pushed { version: 3, server_backup: Some(backup) } =
        push_world(&laptop, &mut st_a, &wa, Some(Resolution::KeepLocal)).await.unwrap()
    else {
        panic!("expected forced push");
    };
    assert!(backup.is_file());
    laptop.unlock(&id).await.unwrap();

    // Mac changed locally too; keep server -> its copy is backed up.
    std::fs::write(game_b.join("saves/World/level.dat"), b"mac-unsynced").unwrap();
    let Outcome::Pulled { version: 3, backup: Some(b) } =
        pull_world(&mac, &mut st_b, &wb, Some(Resolution::KeepServer)).await.unwrap()
    else {
        panic!("expected pull");
    };
    assert_eq!(level(&game_b), b"laptop-offline");
    assert_eq!(std::fs::read(b.join("level.dat")).unwrap(), b"mac-unsynced");

    // In sync now.
    assert!(matches!(pull_world(&mac, &mut st_b, &wb, None).await.unwrap(), Outcome::UpToDate { version: 3, .. }));
}
