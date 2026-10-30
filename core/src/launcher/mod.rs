//! Launcher adapters. Each adapter reads instances from a launcher's data
//! directory and can create a new instance (or an import file the launcher
//! understands) from a pack manifest.

pub mod curseforge;
pub mod modrinth;
pub mod prism;

use crate::error::Result;
use crate::manifest::{CurseForgeRef, Loader, Manifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum LauncherKind {
    /// Prism Launcher and forks that share its instance format (PineconeMC).
    Prism,
    Modrinth,
    CurseForge,
}

impl LauncherKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "prism" | "prismlauncher" | "pinecone" | "pineconemc" | "elyprism" => Some(Self::Prism),
            "modrinth" | "modrinthapp" => Some(Self::Modrinth),
            "curseforge" | "cf" => Some(Self::CurseForge),
            _ => None,
        }
    }
}

impl std::fmt::Display for LauncherKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LauncherKind::Prism => "prism",
            LauncherKind::Modrinth => "modrinth",
            LauncherKind::CurseForge => "curseforge",
        })
    }
}

/// Resolution hint a launcher already knows about a file (e.g. CurseForge
/// records project/file IDs for each installed addon).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHint {
    pub file_name: String,
    pub murmur2: Option<u32>,
    pub curseforge: Option<CurseForgeRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub launcher: LauncherKind,
    /// Human label of the launcher install, e.g. "PineconeMC".
    pub launcher_label: String,
    /// Stable id within the launcher (folder name).
    pub id: String,
    pub name: String,
    /// Instance root folder.
    pub root: PathBuf,
    /// The `.minecraft`-equivalent folder holding mods/config/saves.
    pub game_dir: PathBuf,
    pub mc_version: Option<String>,
    pub loader: Loader,
    #[serde(default)]
    pub hints: Vec<FileHint>,
}

/// What `write_instance` produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WriteOutcome {
    /// A ready instance folder; its game dir still needs files installed.
    Created(Instance),
    /// The launcher must import this file (it owns its own database).
    ImportFile { path: PathBuf },
}

pub struct NewInstance<'a> {
    pub name: &'a str,
    pub manifest: &'a Manifest,
    /// Pack directory (index + overrides) to build from.
    pub pack_dir: &'a Path,
    /// Where import files (.mrpack / CurseForge zip) are written.
    pub export_dir: &'a Path,
}

pub trait Launcher: Send + Sync {
    fn kind(&self) -> LauncherKind;
    fn label(&self) -> &str;
    fn data_dir(&self) -> &Path;
    fn list_instances(&self) -> Result<Vec<Instance>>;
    fn read_instance(&self, id: &str) -> Result<Instance>;
    /// Create a new instance. Never overwrites an existing instance.
    fn write_instance(&self, spec: &NewInstance) -> Result<WriteOutcome>;
}

/// User overrides for launcher data directories (from settings).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherOverrides {
    /// Extra or replacement Prism-format data dirs (PineconeMC, portable Prism...).
    #[serde(default)]
    pub prism: Vec<PathBuf>,
    #[serde(default)]
    pub modrinth: Option<PathBuf>,
    #[serde(default)]
    pub curseforge: Option<PathBuf>,
}

/// Detect every launcher install on this machine.
pub fn detect(overrides: &LauncherOverrides) -> Vec<Box<dyn Launcher>> {
    let mut out: Vec<Box<dyn Launcher>> = Vec::new();
    for p in prism::PrismLauncher::detect(&overrides.prism) {
        out.push(Box::new(p));
    }
    if let Some(m) = modrinth::ModrinthApp::detect(overrides.modrinth.as_deref()) {
        out.push(Box::new(m));
    }
    if let Some(c) = curseforge::CurseForge::detect(overrides.curseforge.as_deref()) {
        out.push(Box::new(c));
    }
    out
}

/// True if `name` is a single plain folder name that stays inside whatever
/// directory it is joined onto: no separators, no drive prefix (`C:x`
/// joins to `C:x` on Windows), no `.`/`..`, no NUL, no Windows device names.
pub fn is_safe_component(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name == "." || name == ".." {
        return false;
    }
    if name.contains(['/', '\\', ':', '\0']) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    !reserved
}

/// Characters not allowed in instance folder names on any OS.
pub fn safe_folder_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "instance".into()
    } else {
        trimmed
    }
}

/// Pick a folder under `parent` that does not exist yet.
pub fn unique_dir(parent: &Path, name: &str) -> PathBuf {
    let base = safe_folder_name(name);
    let mut candidate = parent.join(&base);
    let mut n = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{base} ({n})"));
        n += 1;
    }
    candidate
}

/// Platform default base for per-user app data (`%APPDATA%`,
/// `~/Library/Application Support`, `~/.local/share`).
pub(crate) fn user_data_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().to_path_buf())
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Parse a simple INI file into `section.key -> value` (Prism configs).
pub(crate) fn parse_ini(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = if section.is_empty() || section == "General" {
                k.trim().to_string()
            } else {
                format!("{section}.{}", k.trim())
            };
            out.insert(key, unquote(v.trim()));
        }
    }
    out
}

fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}
