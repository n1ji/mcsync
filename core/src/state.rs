//! Local sync state: which server version each local world was based on,
//! and which pack each instance is linked to. Kept outside game folders.

use crate::error::{Error, IoContext, Result};
use crate::launcher::Instance;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldState {
    /// Server version the local copy was pulled from / last pushed as.
    pub base_version: Option<u64>,
    /// Content fingerprint at that moment, to detect local changes.
    pub fingerprint: Option<String>,
    pub synced_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// world key (`<game_dir>|<world>`) -> state
    #[serde(default)]
    pub worlds: BTreeMap<String, WorldState>,
    /// instance key -> pack slug
    #[serde(default)]
    pub links: BTreeMap<String, String>,
}

pub fn instance_key(inst: &Instance) -> String {
    format!("{}|{}", inst.launcher, inst.root.display())
}

pub fn world_key(game_dir: &Path, world: &str) -> String {
    format!("{}|{world}", game_dir.display())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SyncState {
    pub fn default_path() -> PathBuf {
        crate::config::data_dir().join("state.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).map_err(Error::from),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).at(path),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?).at(&tmp)?;
        std::fs::rename(&tmp, path).at(path)
    }

    pub fn pack_for(&self, inst: &Instance) -> Option<&str> {
        self.links.get(&instance_key(inst)).map(String::as_str)
    }
}
