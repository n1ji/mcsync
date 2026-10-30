//! Prism Launcher format, shared by PineconeMC (formerly ElyPrismLauncher).
//!
//! `<data>/instances/<folder>/instance.cfg` + `mmc-pack.json`, game files in
//! `minecraft/` or `.minecraft/`. The data dir may relocate instances via
//! `InstanceDir=` in `prismlauncher.cfg`.

use super::{
    parse_ini, unique_dir, user_data_dir, Instance, Launcher, LauncherKind, NewInstance,
    WriteOutcome,
};
use crate::error::{Error, IoContext, Result};
use crate::manifest::{Loader, LoaderKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Folder names of Prism-format launchers, in detection order.
pub const KNOWN_DATA_DIRS: &[&str] = &["PineconeMC", "ElyPrismLauncher", "PrismLauncher"];

const UID_MINECRAFT: &str = "net.minecraft";
const LOADER_UIDS: &[(&str, LoaderKind)] = &[
    ("net.minecraftforge", LoaderKind::Forge),
    ("net.neoforged", LoaderKind::NeoForge),
    ("net.fabricmc.fabric-loader", LoaderKind::Fabric),
    ("org.quiltmc.quilt-loader", LoaderKind::Quilt),
];

pub struct PrismLauncher {
    label: String,
    data_dir: PathBuf,
    instances_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MmcPack {
    components: Vec<Component>,
    format_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Component {
    uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    important: Option<bool>,
}

impl PrismLauncher {
    /// Open a Prism-format data dir. Returns None if it has no `instances`
    /// folder and no launcher config.
    pub fn open(data_dir: &Path) -> Option<Self> {
        let cfg = ["prismlauncher.cfg", "pineconemc.cfg", "elyprismlauncher.cfg"]
            .iter()
            .map(|n| data_dir.join(n))
            .find(|p| p.is_file());
        let instances_dir = cfg
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| parse_ini(&t).get("InstanceDir").cloned())
            .filter(|s| !s.is_empty())
            .map(|s| {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    p
                } else {
                    data_dir.join(p)
                }
            })
            .unwrap_or_else(|| data_dir.join("instances"));
        if cfg.is_none() && !instances_dir.is_dir() {
            return None;
        }
        Some(PrismLauncher {
            label: label_for(data_dir),
            data_dir: data_dir.to_path_buf(),
            instances_dir,
        })
    }

    pub fn detect(overrides: &[PathBuf]) -> Vec<Self> {
        let mut dirs: Vec<PathBuf> = overrides.to_vec();
        for base in default_bases() {
            for name in KNOWN_DATA_DIRS {
                dirs.push(base.join(name));
            }
        }
        let mut seen = Vec::new();
        dirs.into_iter()
            .filter(|d| d.is_dir())
            .filter_map(|d| {
                let canon = d.canonicalize().unwrap_or(d.clone());
                if seen.contains(&canon) {
                    return None;
                }
                seen.push(canon);
                PrismLauncher::open(&d)
            })
            .collect()
    }

    pub fn instances_dir(&self) -> &Path {
        &self.instances_dir
    }

    fn read_dir_instance(&self, dir: &Path) -> Result<Instance> {
        let cfg_path = dir.join("instance.cfg");
        let cfg = parse_ini(&std::fs::read_to_string(&cfg_path).at(&cfg_path)?);
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = cfg.get("name").cloned().unwrap_or_else(|| id.clone());

        let (mc_version, loader) = match std::fs::read_to_string(dir.join("mmc-pack.json")) {
            Ok(text) => parse_mmc_pack(&text)?,
            Err(_) => (None, Loader { kind: LoaderKind::Vanilla, version: None }),
        };

        let game_dir = [".minecraft", "minecraft"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_dir())
            .unwrap_or_else(|| dir.join("minecraft"));

        Ok(Instance {
            launcher: LauncherKind::Prism,
            launcher_label: self.label.clone(),
            id,
            name,
            root: dir.to_path_buf(),
            game_dir,
            mc_version,
            loader,
            hints: Vec::new(),
        })
    }
}

fn label_for(data_dir: &Path) -> String {
    let name = data_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("pinecone") || name.contains("ely") {
        "PineconeMC".into()
    } else if name.contains("prism") {
        "Prism Launcher".into()
    } else {
        format!("Prism-format ({})", data_dir.display())
    }
}

fn default_bases() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = user_data_dir().into_iter().collect();
    if cfg!(target_os = "linux") {
        if let Some(home) = super::home_dir() {
            bases.push(home.join(".var/app/org.prismlauncher.PrismLauncher/data"));
        }
    }
    bases
}

fn parse_mmc_pack(text: &str) -> Result<(Option<String>, Loader)> {
    let pack: MmcPack = serde_json::from_str(text)?;
    let version_of = |c: &Component| c.version.clone().or_else(|| c.cached_version.clone());
    let mc = pack
        .components
        .iter()
        .find(|c| c.uid == UID_MINECRAFT)
        .and_then(version_of);
    let loader = pack
        .components
        .iter()
        .find_map(|c| {
            LOADER_UIDS
                .iter()
                .find(|(uid, _)| *uid == c.uid)
                .map(|(_, kind)| Loader { kind: *kind, version: version_of(c) })
        })
        .unwrap_or(Loader { kind: LoaderKind::Vanilla, version: None });
    Ok((mc, loader))
}

fn mmc_pack_for(mc: &str, loader: &Loader) -> MmcPack {
    let mut components = vec![Component {
        uid: UID_MINECRAFT.into(),
        version: Some(mc.into()),
        cached_version: None,
        important: Some(true),
    }];
    if let Some((uid, _)) = LOADER_UIDS.iter().find(|(_, k)| *k == loader.kind) {
        components.push(Component {
            uid: (*uid).into(),
            version: loader.version.clone(),
            cached_version: None,
            important: None,
        });
    }
    // Prism resolves the remaining dependencies (LWJGL, intermediary) itself.
    MmcPack { components, format_version: 1 }
}

impl Launcher for PrismLauncher {
    fn kind(&self) -> LauncherKind {
        LauncherKind::Prism
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn list_instances(&self) -> Result<Vec<Instance>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.instances_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).at(&self.instances_dir),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = entry.file_name().to_string_lossy().starts_with(['_', '.']);
            if hidden || !path.join("instance.cfg").is_file() {
                continue;
            }
            match self.read_dir_instance(&path) {
                Ok(i) => out.push(i),
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }

    fn read_instance(&self, id: &str) -> Result<Instance> {
        if id.contains(['/', '\\']) || id == ".." {
            return Err(Error::Invalid(format!("bad instance id {id}")));
        }
        let dir = self.instances_dir.join(id);
        if !dir.join("instance.cfg").is_file() {
            return Err(Error::NotFound(format!("{} instance '{id}'", self.label)));
        }
        self.read_dir_instance(&dir)
    }

    fn write_instance(&self, spec: &NewInstance) -> Result<WriteOutcome> {
        let mc = spec
            .manifest
            .mc_version()
            .ok_or_else(|| Error::Invalid("manifest has no minecraft version".into()))?;
        std::fs::create_dir_all(&self.instances_dir).at(&self.instances_dir)?;
        let dir = unique_dir(&self.instances_dir, spec.name);
        std::fs::create_dir(&dir).at(&dir)?;
        let game_dir = dir.join("minecraft");
        std::fs::create_dir(&game_dir).at(&game_dir)?;

        let cfg = format!(
            "[General]\nConfigVersion=1.2\nInstanceType=OneSix\niconKey=default\nname={}\n",
            spec.name.replace(['\r', '\n'], " ")
        );
        std::fs::write(dir.join("instance.cfg"), cfg).at(&dir)?;
        let pack = mmc_pack_for(mc, &spec.manifest.loader());
        std::fs::write(dir.join("mmc-pack.json"), serde_json::to_string_pretty(&pack)?)
            .at(&dir)?;

        Ok(WriteOutcome::Created(self.read_dir_instance(&dir)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forge_pack() {
        let text = r#"{"components":[
            {"uid":"org.lwjgl3","version":"3.3.1","dependencyOnly":true},
            {"uid":"net.minecraft","version":"1.20.1","important":true},
            {"uid":"net.minecraftforge","version":"47.4.0"}],"formatVersion":1}"#;
        let (mc, loader) = parse_mmc_pack(text).unwrap();
        assert_eq!(mc.as_deref(), Some("1.20.1"));
        assert_eq!(loader, Loader { kind: LoaderKind::Forge, version: Some("47.4.0".into()) });
    }

    #[test]
    fn labels() {
        assert_eq!(label_for(Path::new("/x/ElyPrismLauncher")), "PineconeMC");
        assert_eq!(label_for(Path::new("/x/PineconeMC")), "PineconeMC");
        assert_eq!(label_for(Path::new("/x/PrismLauncher")), "Prism Launcher");
    }
}
