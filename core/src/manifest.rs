//! Launcher-neutral pack manifest.
//!
//! The on-disk format is a superset of Modrinth's `modrinth.index.json`
//! (`.mrpack` index, format version 1). Extra fields (`curseforge`,
//! `modrinth`, `manual`) are ignored by other `.mrpack` importers, so a pack
//! directory zipped up is a valid `.mrpack`.
//!
//! Pack directory layout (one per pack in the git repo):
//! ```text
//! packs/<slug>/modrinth.index.json
//! packs/<slug>/overrides/config/...
//! packs/<slug>/overrides/options.txt
//! ```

use crate::error::{Error, IoContext, Result};
use crate::scope;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const INDEX_FILE: &str = "modrinth.index.json";
pub const OVERRIDES_DIR: &str = "overrides";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum LoaderKind {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl LoaderKind {
    /// Key used in `.mrpack` `dependencies`.
    pub fn mrpack_key(self) -> Option<&'static str> {
        match self {
            LoaderKind::Vanilla => None,
            LoaderKind::Fabric => Some("fabric-loader"),
            LoaderKind::Quilt => Some("quilt-loader"),
            LoaderKind::Forge => Some("forge"),
            LoaderKind::NeoForge => Some("neoforge"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "vanilla" | "" => Some(Self::Vanilla),
            "fabric" | "fabric-loader" => Some(Self::Fabric),
            "quilt" | "quilt-loader" => Some(Self::Quilt),
            "forge" => Some(Self::Forge),
            "neoforge" | "neo_forge" => Some(Self::NeoForge),
            _ => None,
        }
    }
}

impl std::fmt::Display for LoaderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LoaderKind::Vanilla => "vanilla",
            LoaderKind::Fabric => "fabric",
            LoaderKind::Quilt => "quilt",
            LoaderKind::Forge => "forge",
            LoaderKind::NeoForge => "neoforge",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loader {
    pub kind: LoaderKind,
    pub version: Option<String>,
}

/// `.mrpack` environment requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvSupport {
    Required,
    Optional,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Env {
    pub client: EnvSupport,
    pub server: EnvSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Client,
    Server,
    Both,
}

impl Env {
    pub fn from_side(side: Side) -> Self {
        use EnvSupport::*;
        match side {
            Side::Client => Env { client: Required, server: Unsupported },
            Side::Server => Env { client: Unsupported, server: Required },
            Side::Both => Env { client: Required, server: Required },
        }
    }

    pub fn side(&self) -> Side {
        match (self.client, self.server) {
            (EnvSupport::Unsupported, _) => Side::Server,
            (_, EnvSupport::Unsupported) => Side::Client,
            _ => Side::Both,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Hashes {
    pub sha1: String,
    pub sha512: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModrinthRef {
    pub project_id: String,
    pub version_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurseForgeRef {
    pub project_id: u64,
    pub file_id: u64,
    /// False when the author blocks third-party downloads.
    #[serde(default = "default_true")]
    pub distributable: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackFile {
    /// Game-dir relative path, e.g. `mods/sodium.jar`.
    pub path: String,
    pub hashes: Hashes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Env>,
    #[serde(default)]
    pub downloads: Vec<String>,
    pub file_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modrinth: Option<ModrinthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curseforge: Option<CurseForgeRef>,
    /// No automatic download source: the user must place this file manually.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
}

impl PackFile {
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format_version: u32,
    pub game: String,
    pub version_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub files: Vec<PackFile>,
    /// `.mrpack` dependencies: `minecraft` plus one loader key.
    pub dependencies: BTreeMap<String, String>,
}

impl Manifest {
    pub fn new(name: &str, mc_version: &str, loader: &Loader) -> Self {
        let mut dependencies = BTreeMap::new();
        dependencies.insert("minecraft".to_string(), mc_version.to_string());
        if let (Some(key), Some(v)) = (loader.kind.mrpack_key(), loader.version.as_ref()) {
            dependencies.insert(key.to_string(), v.clone());
        }
        Manifest {
            format_version: 1,
            game: "minecraft".into(),
            version_id: "1".into(),
            name: name.into(),
            summary: Some("Synced by mcsync".into()),
            files: Vec::new(),
            dependencies,
        }
    }

    pub fn mc_version(&self) -> Option<&str> {
        self.dependencies.get("minecraft").map(String::as_str)
    }

    pub fn loader(&self) -> Loader {
        for kind in [LoaderKind::NeoForge, LoaderKind::Forge, LoaderKind::Fabric, LoaderKind::Quilt] {
            if let Some(v) = self.dependencies.get(kind.mrpack_key().unwrap()) {
                return Loader { kind, version: Some(v.clone()) };
            }
        }
        Loader { kind: LoaderKind::Vanilla, version: None }
    }

    pub fn manual_files(&self) -> impl Iterator<Item = &PackFile> {
        self.files.iter().filter(|f| f.manual)
    }

    /// Reject manifests with unsafe paths before acting on them.
    pub fn validate(&self) -> Result<()> {
        if self.game != "minecraft" {
            return Err(Error::Invalid(format!("unsupported game '{}'", self.game)));
        }
        if self.mc_version().is_none() {
            return Err(Error::Invalid("manifest has no minecraft version".into()));
        }
        for f in &self.files {
            if !scope::is_safe_relative(&f.path) {
                return Err(Error::Invalid(format!("unsafe file path in manifest: {}", f.path)));
            }
            for url in &f.downloads {
                if !crate::resolve::is_allowed_download(url) {
                    return Err(Error::Invalid(format!("download url not allowed: {url}")));
                }
            }
        }
        Ok(())
    }

    pub fn load(pack_dir: &Path) -> Result<Self> {
        let path = pack_dir.join(INDEX_FILE);
        let text = std::fs::read_to_string(&path).at(&path)?;
        let m: Manifest = serde_json::from_str(&text)?;
        m.validate()?;
        Ok(m)
    }

    pub fn save(&self, pack_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(pack_dir).at(pack_dir)?;
        let path = pack_dir.join(INDEX_FILE);
        let mut files = self.files.clone();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let sorted = Manifest { files, ..self.clone() };
        let text = serde_json::to_string_pretty(&sorted)? + "\n";
        std::fs::write(&path, text).at(&path)
    }
}

/// Turn a display name into a filesystem/URL safe slug.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("pack");
    }
    out.truncate(64);
    out
}

/// Zip a pack directory into a `.mrpack` file. Only files referenced by URL
/// stay out of the archive; overrides are included.
pub fn write_mrpack(pack_dir: &Path, out: &Path) -> Result<PathBuf> {
    let manifest = Manifest::load(pack_dir)?;
    let file = std::fs::File::create(out).at(out)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // `.mrpack` requires every file entry to have a download; manual files
    // are left out of the index and reported to the user instead.
    let mut index = manifest.clone();
    index.files.retain(|f| !f.manual && !f.downloads.is_empty());
    zip.start_file(INDEX_FILE, opts)?;
    zip.write_all(serde_json::to_string_pretty(&index)?.as_bytes()).at(out)?;

    let overrides = pack_dir.join(OVERRIDES_DIR);
    if overrides.is_dir() {
        for entry in walkdir::WalkDir::new(&overrides).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(pack_dir).expect("walk under pack dir");
            let name = rel.to_string_lossy().replace('\\', "/");
            zip.start_file(name, opts)?;
            let bytes = std::fs::read(entry.path()).at(entry.path())?;
            zip.write_all(&bytes).at(out)?;
        }
    }
    zip.finish()?;
    Ok(out.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        let mut m = Manifest::new(
            "Test",
            "1.20.1",
            &Loader { kind: LoaderKind::Fabric, version: Some("0.15.0".into()) },
        );
        m.files.push(PackFile {
            path: "mods/sodium.jar".into(),
            hashes: Hashes { sha1: "aa".into(), sha512: "bb".into() },
            env: Some(Env::from_side(Side::Client)),
            downloads: vec!["https://cdn.modrinth.com/x.jar".into()],
            file_size: 10,
            modrinth: Some(ModrinthRef { project_id: "AANobbMI".into(), version_id: "v".into() }),
            curseforge: None,
            manual: false,
        });
        m
    }

    #[test]
    fn roundtrip_and_loader() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample();
        m.save(dir.path()).unwrap();
        let back = Manifest::load(dir.path()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.loader().kind, LoaderKind::Fabric);
        assert_eq!(back.mc_version(), Some("1.20.1"));
        assert_eq!(back.files[0].env.as_ref().unwrap().side(), Side::Client);
    }

    #[test]
    fn rejects_unsafe_paths() {
        let mut m = sample();
        m.files[0].path = "../../evil.jar".into();
        assert!(m.validate().is_err());
        let mut m = sample();
        m.files[0].downloads = vec!["http://insecure/x.jar".into()];
        assert!(m.validate().is_err());
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Otherworld [Dungeons & Dragons]"), "otherworld-dungeons-dragons");
        assert_eq!(slugify("!!!"), "pack");
    }
}
