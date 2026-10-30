//! Export an instance to a pack directory, and build/update instances from
//! a pack directory.
//!
//! Files in launcher instance folders are never deleted: anything that
//! would be removed or overwritten is moved to
//! `<game_dir>/.mcsync-backup/<timestamp>/` first.

use crate::error::{Error, IoContext, Result};
use crate::hashing::{hash_file, sha1_file};
use crate::launcher::{Instance, Launcher, LauncherKind, NewInstance, WriteOutcome};
use crate::manifest::{Manifest, PackFile, INDEX_FILE, OVERRIDES_DIR};
use crate::resolve::{is_allowed_download, LocalFile, Resolver};
use crate::scope::{self, HASHED_DIRS, MAX_OVERRIDE_FILE, OVERRIDE_ROOTS};
use serde::Serialize;
use sha2::Digest;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const BACKUP_DIR: &str = ".mcsync-backup";

#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub pack_dir: PathBuf,
    pub files: usize,
    pub overrides: usize,
    /// Mods with no automatic download source.
    pub manual: Vec<String>,
    /// Files skipped (too large / unreadable).
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub downloaded: Vec<String>,
    pub overrides_written: usize,
    /// Files moved to the backup folder (stale or replaced).
    pub backed_up: Vec<String>,
    /// Manual files missing from the instance.
    pub manual_missing: Vec<String>,
    pub backup_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub outcome: WriteOutcome,
    pub apply: Option<ApplyReport>,
    pub manual: Vec<String>,
}

fn rel_string(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Files in `mods/`, `resourcepacks/`, `shaderpacks/` (top level only;
/// disabled mods are skipped).
fn hashed_candidates(game_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in HASHED_DIRS {
        let Ok(entries) = std::fs::read_dir(game_dir.join(dir)) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if p.is_file() && (name.ends_with(".jar") || name.ends_with(".zip")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Export `inst` into `pack_dir` (manifest + overrides). Overwrites the
/// pack directory's previous contents; it is owned by mcsync.
pub async fn export_instance(inst: &Instance, pack_dir: &Path, resolver: &Resolver) -> Result<ExportReport> {
    let mc = inst
        .mc_version
        .clone()
        .ok_or_else(|| Error::Invalid(format!("instance '{}' has no known Minecraft version", inst.name)))?;
    let game = &inst.game_dir;
    let mut skipped = Vec::new();

    let mut locals = Vec::new();
    for path in hashed_candidates(game) {
        let rel = rel_string(path.strip_prefix(game).expect("under game dir"));
        match hash_file(&path) {
            Ok(hashes) => locals.push(LocalFile { rel_path: rel, hashes }),
            Err(e) => skipped.push(format!("{rel}: {e}")),
        }
    }
    let resolved = resolver.resolve(&locals, &inst.hints).await?;

    let previous = Manifest::load(pack_dir).ok();
    let mut manifest = Manifest::new(&inst.name, &mc, &inst.loader);
    let overrides_dir = pack_dir.join(OVERRIDES_DIR);
    if overrides_dir.exists() {
        std::fs::remove_dir_all(&overrides_dir).at(&overrides_dir)?;
    }
    std::fs::create_dir_all(&overrides_dir).at(&overrides_dir)?;

    let mut overrides = 0;
    let mut manual = Vec::new();
    for entry in resolved {
        let is_mod = entry.path.starts_with("mods/");
        if entry.manual && !is_mod {
            // Unresolvable resource/shader pack: ship the file itself.
            if entry.file_size <= MAX_OVERRIDE_FILE {
                copy_into(&game.join(&entry.path), &overrides_dir.join(&entry.path))?;
                overrides += 1;
            } else {
                skipped.push(format!("{} (too large for overrides)", entry.path));
            }
            continue;
        }
        if entry.manual {
            manual.push(entry.path.clone());
        }
        manifest.files.push(entry);
    }

    for root in OVERRIDE_ROOTS {
        let src = game.join(root);
        if !src.exists() {
            continue;
        }
        for e in WalkDir::new(&src).into_iter().filter_map(|e| e.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            let rel = e.path().strip_prefix(game).expect("under game dir");
            if !scope::is_syncable(rel) {
                continue;
            }
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            if size > MAX_OVERRIDE_FILE {
                skipped.push(format!("{} (too large)", rel_string(rel)));
                continue;
            }
            copy_into(e.path(), &overrides_dir.join(rel))?;
            overrides += 1;
        }
    }

    // Bump the pack version when content changed.
    manifest.version_id = match previous {
        Some(prev) => {
            let mut sorted = manifest.files.clone();
            sorted.sort_by(|a, b| a.path.cmp(&b.path));
            let n: u64 = prev.version_id.parse().unwrap_or(0);
            if prev.files == sorted && prev.dependencies == manifest.dependencies {
                prev.version_id
            } else {
                (n + 1).to_string()
            }
        }
        None => "1".into(),
    };
    manifest.save(pack_dir)?;

    Ok(ExportReport {
        pack_dir: pack_dir.to_path_buf(),
        files: manifest.files.len(),
        overrides,
        manual,
        skipped,
    })
}

fn copy_into(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).at(parent)?;
    }
    std::fs::copy(src, dst).at(src)?;
    Ok(())
}

fn backup_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

struct Backup {
    dir: PathBuf,
    moved: Vec<String>,
    created: bool,
}

impl Backup {
    fn new(game_dir: &Path) -> Self {
        Backup { dir: game_dir.join(BACKUP_DIR).join(backup_stamp()), moved: Vec::new(), created: false }
    }

    fn stash(&mut self, game_dir: &Path, rel: &str) -> Result<()> {
        let src = game_dir.join(rel);
        if !src.exists() {
            return Ok(());
        }
        let dst = self.dir.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        self.created = true;
        std::fs::rename(&src, &dst).at(&src)?;
        self.moved.push(rel.to_string());
        Ok(())
    }
}

/// Download one manifest file to `dest`, verifying size and hashes.
pub async fn download_verified(http: &reqwest::Client, file: &PackFile, dest: &Path) -> Result<()> {
    let mut last_err = None;
    for url in file.downloads.iter().filter(|u| is_allowed_download(u)) {
        match fetch(http, url, file).await {
            Ok(bytes) => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).at(parent)?;
                }
                let tmp = dest.with_extension("mcsync-part");
                std::fs::write(&tmp, &bytes).at(&tmp)?;
                std::fs::rename(&tmp, dest).at(dest)?;
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::Invalid(format!("no allowed download for {}", file.path))))
}

async fn fetch(http: &reqwest::Client, url: &str, file: &PackFile) -> Result<Vec<u8>> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?.to_vec();
    if file.file_size != 0 && bytes.len() as u64 != file.file_size {
        return Err(Error::Invalid(format!("{}: size mismatch from {url}", file.path)));
    }
    let sha1 = hex::encode(sha1::Sha1::digest(&bytes));
    let sha512 = hex::encode(sha2::Sha512::digest(&bytes));
    if (!file.hashes.sha1.is_empty() && sha1 != file.hashes.sha1)
        || (!file.hashes.sha512.is_empty() && sha512 != file.hashes.sha512)
    {
        return Err(Error::Invalid(format!("{}: hash mismatch from {url}", file.path)));
    }
    Ok(bytes)
}

/// Bring an existing instance's game dir in line with a pack: download
/// missing/changed files, move stale hashed files and replaced overrides to
/// a backup folder, then copy overrides.
pub async fn apply_pack(game_dir: &Path, pack_dir: &Path, http: &reqwest::Client) -> Result<ApplyReport> {
    let manifest = Manifest::load(pack_dir)?;
    std::fs::create_dir_all(game_dir).at(game_dir)?;
    let mut backup = Backup::new(game_dir);
    let mut downloaded = Vec::new();
    let mut manual_missing = Vec::new();

    let wanted: std::collections::HashSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let override_root = pack_dir.join(OVERRIDES_DIR);

    // Stale hashed files (not in the manifest and not shipped as overrides).
    for path in hashed_candidates(game_dir) {
        let rel = rel_string(path.strip_prefix(game_dir).expect("under game dir"));
        if !wanted.contains(rel.as_str()) && !override_root.join(&rel).is_file() {
            backup.stash(game_dir, &rel)?;
        }
    }

    for f in &manifest.files {
        let dest = game_dir.join(&f.path);
        if dest.is_file() && sha1_file(&dest)? == f.hashes.sha1 {
            continue;
        }
        if f.manual {
            manual_missing.push(f.path.clone());
            continue;
        }
        backup.stash(game_dir, &f.path)?;
        download_verified(http, f, &dest).await?;
        downloaded.push(f.path.clone());
    }

    let mut overrides_written = 0;
    if override_root.is_dir() {
        for e in WalkDir::new(&override_root).into_iter().filter_map(|e| e.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            let rel = rel_string(e.path().strip_prefix(&override_root).expect("under overrides"));
            if !scope::is_safe_relative(&rel) {
                tracing::warn!("ignoring unsafe override path {rel}");
                continue;
            }
            let dest = game_dir.join(&rel);
            if dest.is_file() {
                if sha1_file(&dest)? == sha1_file(e.path())? {
                    continue;
                }
                backup.stash(game_dir, &rel)?;
            }
            copy_into(e.path(), &dest)?;
            overrides_written += 1;
        }
    }

    Ok(ApplyReport {
        downloaded,
        overrides_written,
        backup_dir: backup.created.then(|| backup.dir.clone()),
        backed_up: backup.moved,
        manual_missing,
    })
}

/// Build a new instance in `launcher` from a pack directory.
pub async fn install_pack(
    pack_dir: &Path,
    launcher: &dyn Launcher,
    name: &str,
    export_dir: &Path,
    http: &reqwest::Client,
) -> Result<InstallReport> {
    let manifest = Manifest::load(pack_dir)?;
    let manual: Vec<String> = manifest.manual_files().map(|f| f.path.clone()).collect();

    match launcher.kind() {
        LauncherKind::Prism => {
            let outcome = launcher.write_instance(&NewInstance { name, manifest: &manifest, pack_dir, export_dir })?;
            let apply = match &outcome {
                WriteOutcome::Created(inst) => Some(apply_pack(&inst.game_dir, pack_dir, http).await?),
                WriteOutcome::ImportFile { .. } => None,
            };
            Ok(InstallReport { outcome, apply, manual })
        }
        LauncherKind::Modrinth => {
            let outcome = launcher.write_instance(&NewInstance { name, manifest: &manifest, pack_dir, export_dir })?;
            Ok(InstallReport { outcome, apply: None, manual })
        }
        LauncherKind::CurseForge => {
            // Stage a copy of the pack and download files CurseForge can't
            // reference by ID into its overrides.
            let staging = tempfile::tempdir().at(std::env::temp_dir())?;
            copy_tree(pack_dir, staging.path())?;
            for f in manifest.files.iter().filter(|f| f.curseforge.is_none() && !f.manual) {
                let dest = staging.path().join(OVERRIDES_DIR).join(&f.path);
                download_verified(http, f, &dest).await?;
            }
            let outcome = launcher.write_instance(&NewInstance {
                name,
                manifest: &manifest,
                pack_dir: staging.path(),
                export_dir,
            })?;
            Ok(InstallReport { outcome, apply: None, manual })
        }
    }
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for e in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        let rel = e.path().strip_prefix(src).expect("under src");
        let target = dst.join(rel);
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&target).at(&target)?;
        } else if e.file_type().is_file() {
            copy_into(e.path(), &target)?;
        }
    }
    Ok(())
}

/// True if a directory looks like a pack directory.
pub fn is_pack_dir(dir: &Path) -> bool {
    dir.join(INDEX_FILE).is_file()
}
