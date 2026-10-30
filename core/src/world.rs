//! World snapshots: a world folder <-> `tar.zst` archive.
//!
//! Unpacking never writes outside the target and never replaces a world in
//! place: the new copy is unpacked next to it, the old one is moved to
//! `<saves>/.mcsync-backups/`, then the new one is renamed into place.

use crate::error::{Error, IoContext, Result};
use crate::manifest::slugify;
use sha1::Digest;
use std::path::{Component, Path, PathBuf};

pub const LOCAL_BACKUPS: &str = ".mcsync-backups";
/// Local backups kept per world.
pub const KEEP_LOCAL_BACKUPS: usize = 5;
const SKIP_FILES: &[&str] = &["session.lock"];

/// Server-side world id: `<pack-slug>__<world-slug>-<hash>`. The hash of the
/// exact folder name keeps "New World" and "New_World" apart.
pub fn world_id(pack: &str, world_folder: &str) -> String {
    let h = hex::encode(sha1::Sha1::digest(world_folder.as_bytes()));
    format!("{}__{}-{}", slugify(pack), slugify(world_folder), &h[..8])
}

/// List world folders in a game dir's `saves/`.
pub fn list_worlds(game_dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(game_dir.join("saves"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("level.dat").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// Cheap fingerprint of a world's contents (paths, sizes, mtimes) used to
/// tell whether it changed since the last sync.
pub fn fingerprint(world_dir: &Path) -> Result<String> {
    let mut entries = Vec::new();
    for e in walkdir::WalkDir::new(world_dir).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() || SKIP_FILES.contains(&e.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        let meta = e.metadata().map_err(|err| Error::Invalid(err.to_string()))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let rel = e.path().strip_prefix(world_dir).expect("under world").to_string_lossy().replace('\\', "/");
        entries.push(format!("{rel}\0{}\0{mtime}", meta.len()));
    }
    entries.sort();
    Ok(hex::encode(sha1::Sha1::digest(entries.join("\n").as_bytes())))
}

/// Pack a world folder into `out` (tar.zst). Paths in the archive are
/// relative to the world root.
pub fn pack_world(world_dir: &Path, out: &Path) -> Result<()> {
    if !world_dir.join("level.dat").is_file() {
        return Err(Error::NotFound(format!("no level.dat in {}", world_dir.display())));
    }
    let file = std::fs::File::create(out).at(out)?;
    let enc = zstd::Encoder::new(file, 6).at(out)?.auto_finish();
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);
    for e in walkdir::WalkDir::new(world_dir).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        let rel = e.path().strip_prefix(world_dir).expect("under world");
        if SKIP_FILES.contains(&e.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        if e.file_type().is_dir() {
            tar.append_dir(rel, e.path()).at(e.path())?;
        } else if e.file_type().is_file() {
            tar.append_path_with_name(e.path(), rel).at(e.path())?;
        }
    }
    tar.into_inner().at(out)?;
    Ok(())
}

/// Unpack `archive` into `dest`, checking every entry.
fn unpack_checked(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).at(archive)?;
    let dec = zstd::Decoder::new(file).at(archive)?;
    let mut tar = tar::Archive::new(dec);
    std::fs::create_dir_all(dest).at(dest)?;
    for entry in tar.entries().at(archive)? {
        let mut entry = entry.at(archive)?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(Error::Invalid("world archive contains links or special files".into()));
        }
        let path = entry.path().at(archive)?.into_owned();
        if !path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
            return Err(Error::Invalid(format!("unsafe path in world archive: {}", path.display())));
        }
        // unpack_in also refuses to escape `dest`.
        if !entry.unpack_in(dest).at(dest)? {
            return Err(Error::Invalid(format!("unsafe path in world archive: {}", path.display())));
        }
    }
    if !dest.join("level.dat").is_file() {
        return Err(Error::Invalid("world archive has no level.dat".into()));
    }
    Ok(())
}

/// Replace (or create) `saves/<world>` from an archive. Returns the backup
/// location of the previous copy, if there was one.
pub fn install_world(archive: &Path, saves_dir: &Path, world: &str) -> Result<Option<PathBuf>> {
    if world.is_empty() || world.contains(['/', '\\']) || world == "." || world == ".." {
        return Err(Error::Invalid(format!("bad world folder name {world:?}")));
    }
    std::fs::create_dir_all(saves_dir).at(saves_dir)?;
    let staging = saves_dir.join(format!(".{world}.mcsync-incoming"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).at(&staging)?;
    }
    if let Err(e) = unpack_checked(archive, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let target = saves_dir.join(world);
    let backup = if target.exists() { Some(backup_world(saves_dir, world)?) } else { None };
    std::fs::rename(&staging, &target).at(&target)?;
    Ok(backup)
}

/// Move `saves/<world>` into the local backups folder and prune old ones.
pub fn backup_world(saves_dir: &Path, world: &str) -> Result<PathBuf> {
    let root = saves_dir.join(LOCAL_BACKUPS);
    std::fs::create_dir_all(&root).at(&root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dst = root.join(format!("{world}.{stamp}"));
    let src = saves_dir.join(world);
    std::fs::rename(&src, &dst).at(&src)?;
    prune_backups(&root, world)?;
    Ok(dst)
}

/// Copy an archive into the local backups folder (used for "keep local,
/// back up the server copy").
pub fn stash_archive(saves_dir: &Path, world: &str, archive: &Path, label: &str) -> Result<PathBuf> {
    let root = saves_dir.join(LOCAL_BACKUPS);
    std::fs::create_dir_all(&root).at(&root)?;
    let dst = root.join(format!("{world}.{label}.tar.zst"));
    std::fs::copy(archive, &dst).at(archive)?;
    Ok(dst)
}

fn prune_backups(root: &Path, world: &str) -> Result<()> {
    let prefix = format!("{world}.");
    let mut backups: Vec<(u64, PathBuf)> = std::fs::read_dir(root)
        .at(root)?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stamp: u64 = name.strip_prefix(&prefix)?.parse().ok()?;
            Some((stamp, e.path()))
        })
        .collect();
    backups.sort();
    while backups.len() > KEEP_LOCAL_BACKUPS {
        let (_, p) = backups.remove(0);
        std::fs::remove_dir_all(&p).at(&p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_world(dir: &Path) {
        std::fs::create_dir_all(dir.join("region")).unwrap();
        std::fs::write(dir.join("level.dat"), b"level").unwrap();
        std::fs::write(dir.join("region/r.0.0.mca"), vec![7u8; 4096]).unwrap();
        std::fs::write(dir.join("session.lock"), b"lock").unwrap();
    }

    #[test]
    fn pack_and_install_roundtrip_with_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let saves = tmp.path().join("saves");
        make_world(&saves.join("World"));
        let archive = tmp.path().join("w.tar.zst");
        pack_world(&saves.join("World"), &archive).unwrap();

        std::fs::write(saves.join("World/level.dat"), b"changed").unwrap();
        let backup = install_world(&archive, &saves, "World").unwrap().unwrap();
        assert_eq!(std::fs::read(saves.join("World/level.dat")).unwrap(), b"level");
        assert!(!saves.join("World/session.lock").exists());
        assert_eq!(std::fs::read(backup.join("level.dat")).unwrap(), b"changed");
    }

    #[test]
    fn rejects_traversal_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("evil.tar.zst");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = zstd::Encoder::new(f, 1).unwrap().auto_finish();
            let mut b = tar::Builder::new(enc);
            let data = b"pwned";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            // Write the raw name to bypass tar-rs's own path checks.
            h.as_old_mut().name[..10].copy_from_slice(b"../evil.tx");
            h.set_cksum();
            b.append(&h, &data[..]).unwrap();
            b.finish().unwrap();
        }
        let saves = tmp.path().join("saves");
        assert!(install_world(&archive, &saves, "World").is_err());
        assert!(!tmp.path().join("evil.tx").exists());
        assert!(install_world(&archive, &saves, "..").is_err());
    }

    #[test]
    fn ids_and_fingerprints() {
        assert_ne!(world_id("Pack", "New World"), world_id("Pack", "New_World"));
        assert!(world_id("My Pack!", "World").starts_with("my-pack__world-"));
        let tmp = tempfile::tempdir().unwrap();
        make_world(tmp.path());
        let a = fingerprint(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("session.lock"), b"other").unwrap();
        assert_eq!(a, fingerprint(tmp.path()).unwrap());
        std::fs::write(tmp.path().join("level.dat"), b"different size").unwrap();
        assert_ne!(a, fingerprint(tmp.path()).unwrap());
    }
}
