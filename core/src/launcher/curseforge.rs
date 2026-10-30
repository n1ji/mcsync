//! CurseForge app. Instances in `~/curseforge/minecraft/Instances/<name>/`
//! (`~/Documents/curseforge/...` on macOS) with `minecraftinstance.json`.
//!
//! `minecraftinstance.json` carries many app-internal fields (install
//! profile JSON, GUIDs) that the app regenerates itself, so a hand-written
//! one is fragile. New instances are produced as a CurseForge modpack zip
//! (`manifest.json` + `overrides/`) for the app's Import.

use super::{
    home_dir, FileHint, Instance, Launcher, LauncherKind, NewInstance, WriteOutcome,
};
use crate::error::{Error, IoContext, Result};
use crate::manifest::{slugify, CurseForgeRef, Loader, LoaderKind, OVERRIDES_DIR};
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct CurseForge {
    instances_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstanceJson {
    name: Option<String>,
    game_version: Option<String>,
    base_mod_loader: Option<BaseModLoader>,
    #[serde(default)]
    installed_addons: Vec<Addon>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BaseModLoader {
    name: Option<String>,
    minecraft_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Addon {
    #[serde(rename = "addonID")]
    addon_id: Option<u64>,
    installed_file: Option<AddonFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddonFile {
    id: u64,
    file_name: Option<String>,
    file_name_on_disk: Option<String>,
    package_fingerprint: Option<u32>,
    download_url: Option<String>,
}

impl CurseForge {
    pub fn open(instances_dir: &Path) -> Option<Self> {
        instances_dir
            .is_dir()
            .then(|| CurseForge { instances_dir: instances_dir.to_path_buf() })
    }

    pub fn detect(override_dir: Option<&Path>) -> Option<Self> {
        if let Some(d) = override_dir {
            return Self::open(d);
        }
        let home = home_dir()?;
        let candidates = [
            home.join("curseforge/minecraft/Instances"),
            home.join("Documents/curseforge/minecraft/Instances"),
        ];
        candidates.iter().find_map(|p| Self::open(p))
    }

    fn read_dir_instance(&self, dir: &Path) -> Result<Instance> {
        let path = dir.join("minecraftinstance.json");
        let text = std::fs::read_to_string(&path).at(&path)?;
        let json: InstanceJson = serde_json::from_str(&text)?;
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mc = json
            .game_version
            .clone()
            .or_else(|| json.base_mod_loader.as_ref().and_then(|b| b.minecraft_version.clone()));
        let loader = json
            .base_mod_loader
            .as_ref()
            .and_then(|b| b.name.as_deref())
            .map(|n| parse_loader_name(n, mc.as_deref()))
            .unwrap_or(Loader { kind: LoaderKind::Vanilla, version: None });

        let hints = json
            .installed_addons
            .iter()
            .filter_map(|a| {
                let f = a.installed_file.as_ref()?;
                let file_name = f.file_name_on_disk.clone().or_else(|| f.file_name.clone())?;
                Some(FileHint {
                    file_name,
                    murmur2: f.package_fingerprint,
                    curseforge: a.addon_id.map(|pid| CurseForgeRef {
                        project_id: pid,
                        file_id: f.id,
                        distributable: f.download_url.is_some(),
                    }),
                })
            })
            .collect();

        Ok(Instance {
            launcher: LauncherKind::CurseForge,
            launcher_label: "CurseForge".into(),
            name: json.name.unwrap_or_else(|| id.clone()),
            id,
            root: dir.to_path_buf(),
            game_dir: dir.to_path_buf(),
            mc_version: mc,
            loader,
            hints,
        })
    }
}

/// `forge-47.4.0`, `neoforge-21.1.1`, `fabric-0.15.7-1.20.1`, `quilt-0.26.0-1.20.1`
pub fn parse_loader_name(name: &str, mc: Option<&str>) -> Loader {
    let Some((kind, rest)) = name.split_once('-') else {
        return Loader { kind: LoaderKind::Vanilla, version: None };
    };
    let Some(kind) = LoaderKind::parse(kind) else {
        return Loader { kind: LoaderKind::Vanilla, version: None };
    };
    let mut version = rest.to_string();
    if let Some(mc) = mc {
        if let Some(stripped) = version.strip_suffix(&format!("-{mc}")) {
            version = stripped.to_string();
        }
    }
    Loader { kind, version: Some(version) }
}

fn loader_id(loader: &Loader) -> Option<String> {
    let v = loader.version.as_ref()?;
    match loader.kind {
        LoaderKind::Vanilla => None,
        kind => Some(format!("{kind}-{v}")),
    }
}

impl Launcher for CurseForge {
    fn kind(&self) -> LauncherKind {
        LauncherKind::CurseForge
    }

    fn label(&self) -> &str {
        "CurseForge"
    }

    fn data_dir(&self) -> &Path {
        &self.instances_dir
    }

    fn list_instances(&self) -> Result<Vec<Instance>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.instances_dir).at(&self.instances_dir)?.flatten() {
            let path = entry.path();
            if !path.join("minecraftinstance.json").is_file() {
                continue;
            }
            match self.read_dir_instance(&path) {
                Ok(i) => out.push(i),
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        }
        out.sort_by_key(|a| a.name.to_lowercase());
        Ok(out)
    }

    fn read_instance(&self, id: &str) -> Result<Instance> {
        if !super::is_safe_component(id) {
            return Err(Error::Invalid(format!("bad instance id {id}")));
        }
        let dir = self.instances_dir.join(id);
        if !dir.join("minecraftinstance.json").is_file() {
            return Err(Error::NotFound(format!("CurseForge instance '{id}'")));
        }
        self.read_dir_instance(&dir)
    }

    /// Writes a CurseForge modpack zip. Files with CurseForge IDs are
    /// referenced; anything else must already be staged under the pack's
    /// `overrides/` (the installer downloads Modrinth-only files there).
    fn write_instance(&self, spec: &NewInstance) -> Result<WriteOutcome> {
        let m = spec.manifest;
        let mc = m
            .mc_version()
            .ok_or_else(|| Error::Invalid("manifest has no minecraft version".into()))?;
        let loaders: Vec<_> = loader_id(&m.loader())
            .map(|id| json!({ "id": id, "primary": true }))
            .into_iter()
            .collect();
        let files: Vec<_> = m
            .files
            .iter()
            .filter_map(|f| f.curseforge.as_ref())
            .map(|cf| json!({ "projectID": cf.project_id, "fileID": cf.file_id, "required": true }))
            .collect();
        let cf_manifest = json!({
            "minecraft": { "version": mc, "modLoaders": loaders },
            "manifestType": "minecraftModpack",
            "manifestVersion": 1,
            "name": spec.name,
            "version": m.version_id,
            "author": "mcsync",
            "files": files,
            "overrides": OVERRIDES_DIR,
        });

        std::fs::create_dir_all(spec.export_dir).at(spec.export_dir)?;
        let out = spec.export_dir.join(format!("{}-curseforge.zip", slugify(spec.name)));
        let file = std::fs::File::create(&out).at(&out)?;
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("manifest.json", opts)?;
        zip.write_all(serde_json::to_string_pretty(&cf_manifest)?.as_bytes()).at(&out)?;

        let overrides = spec.pack_dir.join(OVERRIDES_DIR);
        if overrides.is_dir() {
            for entry in crate::pack::walk_repo_dir(&overrides) {
                let rel = entry.path().strip_prefix(spec.pack_dir).expect("under pack dir");
                zip.start_file(rel.to_string_lossy().replace('\\', "/"), opts)?;
                zip.write_all(&std::fs::read(entry.path()).at(entry.path())?).at(&out)?;
            }
        }
        zip.finish()?;
        Ok(WriteOutcome::ImportFile { path: out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_names() {
        assert_eq!(
            parse_loader_name("forge-43.3.5", Some("1.19.2")),
            Loader { kind: LoaderKind::Forge, version: Some("43.3.5".into()) }
        );
        assert_eq!(
            parse_loader_name("fabric-0.15.7-1.20.1", Some("1.20.1")),
            Loader { kind: LoaderKind::Fabric, version: Some("0.15.7".into()) }
        );
        assert_eq!(parse_loader_name("neoforge-21.1.1", None).kind, LoaderKind::NeoForge);
        assert_eq!(
            loader_id(&Loader { kind: LoaderKind::Fabric, version: Some("0.15.7".into()) }).as_deref(),
            Some("fabric-0.15.7")
        );
    }
}
