//! World pull/push with conflict detection.
//!
//! A local world records the server version it was based on. If the server
//! has moved past that base and the local copy also changed, nothing is
//! overwritten silently: the caller gets `Conflict` and must choose
//! `KeepServer` (local copy is backed up) or `KeepLocal` (server copy is
//! downloaded to local backups, and the server keeps it in history).

use crate::client::SyncClient;
use crate::error::{Error, Result};
use crate::state::{now, world_key, SyncState, WorldState};
use crate::world;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    KeepServer,
    KeepLocal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outcome {
    UpToDate { version: u64, local_changed: bool },
    Pulled { version: u64, backup: Option<PathBuf> },
    Pushed { version: u64, server_backup: Option<PathBuf> },
    /// Local copy kept; server copy saved locally. Push next.
    KeptLocal { server_version: u64, server_backup: PathBuf },
    NothingOnServer,
    NoLocalWorld,
    Conflict { server_version: u64, base_version: Option<u64> },
}

pub struct WorldRef<'a> {
    pub game_dir: &'a Path,
    /// Folder name under `saves/`.
    pub world: &'a str,
    /// Server id (see [`world::world_id`]).
    pub id: String,
}

impl WorldRef<'_> {
    fn saves(&self) -> PathBuf {
        self.game_dir.join("saves")
    }
    fn dir(&self) -> PathBuf {
        self.saves().join(self.world)
    }
    fn key(&self) -> String {
        world_key(self.game_dir, self.world)
    }
}

fn local_changed(w: &WorldRef<'_>, st: Option<&WorldState>) -> Result<bool> {
    if !w.dir().join("level.dat").is_file() {
        return Ok(false);
    }
    let fp = world::fingerprint(&w.dir())?;
    Ok(st.and_then(|s| s.fingerprint.as_deref()) != Some(fp.as_str()))
}

fn record(state: &mut SyncState, w: &WorldRef<'_>, version: u64) -> Result<()> {
    let fp = world::fingerprint(&w.dir())?;
    state.worlds.insert(
        w.key(),
        WorldState { base_version: Some(version), fingerprint: Some(fp), synced_at: now() },
    );
    Ok(())
}

async fn download_and_install(client: &SyncClient, state: &mut SyncState, w: &WorldRef<'_>) -> Result<Outcome> {
    let tmp = tempfile::NamedTempFile::new().map_err(|e| Error::Io { path: std::env::temp_dir(), source: e })?;
    let version = client.pull(&w.id, None, tmp.path()).await?;
    let backup = world::install_world(tmp.path(), &w.saves(), w.world)?;
    record(state, w, version)?;
    Ok(Outcome::Pulled { version, backup })
}

async fn stash_server_copy(client: &SyncClient, w: &WorldRef<'_>, version: u64) -> Result<PathBuf> {
    let tmp = tempfile::NamedTempFile::new().map_err(|e| Error::Io { path: std::env::temp_dir(), source: e })?;
    client.pull(&w.id, Some(version), tmp.path()).await?;
    world::stash_archive(&w.saves(), w.world, tmp.path(), &format!("server-v{version}"))
}

pub async fn pull_world(
    client: &SyncClient,
    state: &mut SyncState,
    w: &WorldRef<'_>,
    resolution: Option<Resolution>,
) -> Result<Outcome> {
    let Some(info) = client.world(&w.id).await? else {
        return Ok(Outcome::NothingOnServer);
    };
    if info.version == 0 {
        return Ok(Outcome::NothingOnServer);
    }
    let st = state.worlds.get(&w.key()).cloned();
    let exists = w.dir().join("level.dat").is_file();
    if !exists {
        return download_and_install(client, state, w).await;
    }
    let changed = local_changed(w, st.as_ref())?;
    let base = st.as_ref().and_then(|s| s.base_version);
    if base == Some(info.version) {
        return Ok(Outcome::UpToDate { version: info.version, local_changed: changed });
    }
    if !changed {
        return download_and_install(client, state, w).await;
    }
    match resolution {
        None => Ok(Outcome::Conflict { server_version: info.version, base_version: base }),
        Some(Resolution::KeepServer) => download_and_install(client, state, w).await,
        Some(Resolution::KeepLocal) => {
            let server_backup = stash_server_copy(client, w, info.version).await?;
            // Rebase the local copy onto the server version so the next
            // push succeeds; keep the old fingerprint so it counts as changed.
            let entry = state.worlds.entry(w.key()).or_default();
            entry.base_version = Some(info.version);
            entry.synced_at = now();
            Ok(Outcome::KeptLocal { server_version: info.version, server_backup })
        }
    }
}

/// Push the local world. The caller must hold the world lock.
pub async fn push_world(
    client: &SyncClient,
    state: &mut SyncState,
    w: &WorldRef<'_>,
    resolution: Option<Resolution>,
) -> Result<Outcome> {
    if !w.dir().join("level.dat").is_file() {
        return Ok(Outcome::NoLocalWorld);
    }
    let server_version = client.world(&w.id).await?.map(|i| i.version).unwrap_or(0);
    let st = state.worlds.get(&w.key()).cloned();
    let base = st.as_ref().and_then(|s| s.base_version);
    let changed = local_changed(w, st.as_ref())?;

    let mut force = false;
    let mut server_backup = None;
    if server_version > 0 && base != Some(server_version) {
        match resolution {
            None => return Ok(Outcome::Conflict { server_version, base_version: base }),
            Some(Resolution::KeepServer) => return download_and_install(client, state, w).await,
            Some(Resolution::KeepLocal) => {
                server_backup = Some(stash_server_copy(client, w, server_version).await?);
                force = true;
            }
        }
    } else if !changed && base.is_some() {
        return Ok(Outcome::UpToDate { version: server_version, local_changed: false });
    }

    let tmp = tempfile::NamedTempFile::new().map_err(|e| Error::Io { path: std::env::temp_dir(), source: e })?;
    world::pack_world(&w.dir(), tmp.path())?;
    let version = client.push(&w.id, tmp.path(), base.or(Some(server_version)), force).await?;
    record(state, w, version)?;
    Ok(Outcome::Pushed { version, server_backup })
}
