//! Settings (plain TOML, no secrets) and secrets (OS keychain).

use crate::error::{Error, IoContext, Result};
use crate::launcher::LauncherOverrides;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "mcsync";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Secret {
    DeviceToken,
    CurseForgeApiKey,
    GitToken,
}

impl Secret {
    fn account(self) -> &'static str {
        match self {
            Secret::DeviceToken => "device-token",
            Secret::CurseForgeApiKey => "curseforge-api-key",
            Secret::GitToken => "git-token",
        }
    }

    /// Environment override, handy for CI and headless use.
    fn env_var(self) -> &'static str {
        match self {
            Secret::DeviceToken => "MCSYNC_DEVICE_TOKEN",
            Secret::CurseForgeApiKey => "MCSYNC_CURSEFORGE_API_KEY",
            Secret::GitToken => "MCSYNC_GIT_TOKEN",
        }
    }

    pub fn get(self) -> Result<Option<String>> {
        if let Ok(v) = std::env::var(self.env_var()) {
            if !v.is_empty() {
                return Ok(Some(v));
            }
        }
        match keyring::Entry::new(KEYRING_SERVICE, self.account())?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set(self, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, self.account())?;
        if value.is_empty() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.into()),
            }
        } else {
            Ok(entry.set_password(value)?)
        }
    }

    pub fn is_set(self) -> bool {
        matches!(self.get(), Ok(Some(_)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Base URL of the world server, e.g. `https://mcsync.example.com`.
    pub server_url: Option<String>,
    /// Remote URL of the pack manifest repo.
    pub git_repo_url: Option<String>,
    /// Local checkout of the pack repo.
    pub repo_dir: PathBuf,
    /// Where `.mrpack` / CurseForge zips are written.
    pub export_dir: PathBuf,
    pub launchers: LauncherOverrides,
    /// Pull + lock before launch, push + unlock on exit.
    pub auto_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let data = data_dir();
        Settings {
            server_url: None,
            git_repo_url: None,
            repo_dir: data.join("repo"),
            export_dir: data.join("exports"),
            launchers: LauncherOverrides::default(),
            auto_mode: false,
        }
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "n1ji", "mcsync")
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("MCSYNC_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|p| p.config_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(".mcsync"))
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("MCSYNC_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|p| p.data_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(".mcsync"))
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir().join("settings.toml")
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).at(path),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::Invalid(e.to_string()))?;
        std::fs::write(path, text).at(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.toml");
        let mut s = Settings::default();
        s.server_url = Some("https://mc.example.com".into());
        s.launchers.prism.push(PathBuf::from("D:/Portable/PineconeMC"));
        s.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.to_lowercase().contains("token"));
        assert_eq!(Settings::load_from(&path).unwrap(), s);
    }
}
