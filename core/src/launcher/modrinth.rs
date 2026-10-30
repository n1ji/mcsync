//! Modrinth App. Instance folders live in `<data>/profiles/<path>/`; metadata
//! lives in the app's SQLite DB (`app.db`).
//!
//! The DB is only ever opened read-only. Its schema changes often (2026
//! builds split metadata across `instances`, `instance_content_sets`,
//! `instance_files` and a shared content store), so writing to it is not
//! safe. New instances are produced as a `.mrpack` for the app to import.
//!
//! Supported schemas:
//! - current: `instances` + `instance_content_sets` (game_version, loader)
//! - legacy (`com.modrinth.theseus` era): `profiles` table
//! - no DB / unreadable DB: profile folders listed with unknown version

use super::{user_data_dir, Instance, Launcher, LauncherKind, NewInstance, WriteOutcome};
use crate::error::{Error, IoContext, Result};
use crate::manifest::{slugify, write_mrpack, Loader, LoaderKind};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const KNOWN_DATA_DIRS: &[&str] = &["ModrinthApp", "com.modrinth.theseus"];

pub struct ModrinthApp {
    data_dir: PathBuf,
}

struct DbProfile {
    name: String,
    game_version: Option<String>,
    loader: Loader,
}

impl ModrinthApp {
    pub fn open(data_dir: &Path) -> Option<Self> {
        data_dir
            .join("profiles")
            .is_dir()
            .then(|| ModrinthApp { data_dir: data_dir.to_path_buf() })
    }

    pub fn detect(override_dir: Option<&Path>) -> Option<Self> {
        if let Some(d) = override_dir {
            return Self::open(d);
        }
        let base = user_data_dir()?;
        KNOWN_DATA_DIRS.iter().find_map(|n| Self::open(&base.join(n)))
    }

    fn profiles_dir(&self) -> PathBuf {
        self.data_dir.join("profiles")
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("app.db")
    }

    /// Read profile metadata keyed by profile folder name. Errors are logged
    /// and produce an empty map so folders are still listed.
    fn read_db(&self) -> HashMap<String, DbProfile> {
        let path = self.db_path();
        if !path.is_file() {
            return HashMap::new();
        }
        match open_read_only(&path).and_then(|c| query_profiles(&c)) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("could not read Modrinth DB {}: {e}", path.display());
                HashMap::new()
            }
        }
    }

    fn build(&self, folder: &str, meta: Option<&DbProfile>) -> Instance {
        let root = self.profiles_dir().join(folder);
        Instance {
            launcher: LauncherKind::Modrinth,
            launcher_label: "Modrinth App".into(),
            id: folder.to_string(),
            name: meta.map(|m| m.name.clone()).unwrap_or_else(|| folder.to_string()),
            game_dir: root.clone(),
            root,
            mc_version: meta.and_then(|m| m.game_version.clone()),
            loader: meta
                .map(|m| m.loader.clone())
                .unwrap_or(Loader { kind: LoaderKind::Vanilla, version: None }),
            hints: Vec::new(),
        }
    }
}

fn open_read_only(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    Ok(conn)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let cols = stmt
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn query_profiles(conn: &Connection) -> Result<HashMap<String, DbProfile>> {
    let mut out = HashMap::new();
    let instance_cols = table_columns(conn, "instances")?;
    let set_cols = table_columns(conn, "instance_content_sets")?;
    let has = |cols: &[String], names: &[&str]| names.iter().all(|n| cols.iter().any(|c| c == n));

    if has(&instance_cols, &["path", "name", "applied_content_set_id"])
        && has(&set_cols, &["id", "game_version", "loader", "loader_version"])
    {
        let mut stmt = conn.prepare(
            "SELECT i.path, i.name, s.game_version, s.loader, s.loader_version
             FROM instances i
             LEFT JOIN instance_content_sets s ON s.id = i.applied_content_set_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (path, name, gv, loader, lv) = row?;
            out.insert(path, profile(name, gv, loader, lv));
        }
        return Ok(out);
    }

    let legacy_cols = table_columns(conn, "profiles")?;
    if has(&legacy_cols, &["path", "name", "game_version", "mod_loader"]) {
        let lv_col = if legacy_cols.iter().any(|c| c == "mod_loader_version") {
            "mod_loader_version"
        } else {
            "NULL"
        };
        let sql = format!("SELECT path, name, game_version, mod_loader, {lv_col} FROM profiles");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (path, name, gv, loader, lv) = row?;
            out.insert(path, profile(name, gv, loader, lv));
        }
        return Ok(out);
    }

    Err(Error::Invalid("unrecognised Modrinth App database schema".into()))
}

fn profile(name: String, gv: Option<String>, loader: Option<String>, lv: Option<String>) -> DbProfile {
    let kind = loader
        .as_deref()
        .and_then(LoaderKind::parse)
        .unwrap_or(LoaderKind::Vanilla);
    DbProfile {
        name,
        game_version: gv.filter(|s| !s.is_empty()),
        loader: Loader {
            kind,
            version: if kind == LoaderKind::Vanilla { None } else { lv },
        },
    }
}

impl Launcher for ModrinthApp {
    fn kind(&self) -> LauncherKind {
        LauncherKind::Modrinth
    }

    fn label(&self) -> &str {
        "Modrinth App"
    }

    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn list_instances(&self) -> Result<Vec<Instance>> {
        let db = self.read_db();
        let dir = self.profiles_dir();
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).at(&dir)?.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().into_owned();
            // Without DB metadata, only list folders that look like instances.
            let meta = db.get(&folder);
            if meta.is_none() && !(entry.path().join("mods").is_dir() || entry.path().join("options.txt").is_file()) {
                continue;
            }
            out.push(self.build(&folder, meta));
        }
        out.sort_by_key(|a| a.name.to_lowercase());
        Ok(out)
    }

    fn read_instance(&self, id: &str) -> Result<Instance> {
        if !super::is_safe_component(id) {
            return Err(Error::Invalid(format!("bad instance id {id}")));
        }
        if !self.profiles_dir().join(id).is_dir() {
            return Err(Error::NotFound(format!("Modrinth profile '{id}'")));
        }
        let db = self.read_db();
        Ok(self.build(id, db.get(id)))
    }

    fn write_instance(&self, spec: &NewInstance) -> Result<WriteOutcome> {
        std::fs::create_dir_all(spec.export_dir).at(spec.export_dir)?;
        let out = spec.export_dir.join(format!("{}.mrpack", slugify(spec.name)));
        write_mrpack(spec.pack_dir, &out)?;
        Ok(WriteOutcome::ImportFile { path: out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_schema_db(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE instances (id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                applied_content_set_id TEXT, name TEXT NOT NULL);
             CREATE TABLE instance_content_sets (id TEXT PRIMARY KEY, instance_id TEXT,
                game_version TEXT NOT NULL, loader TEXT NOT NULL, loader_version TEXT);
             INSERT INTO instances VALUES ('legacy:create','create','legacy:create:default','Create');
             INSERT INTO instance_content_sets VALUES
                ('legacy:create:default','legacy:create','1.20.1','fabric','0.18.4');",
        )
        .unwrap();
    }

    #[test]
    fn reads_current_schema() {
        let dir = tempfile::tempdir().unwrap();
        current_schema_db(&dir.path().join("db"));
        let c = open_read_only(&dir.path().join("db")).unwrap();
        let m = query_profiles(&c).unwrap();
        let p = &m["create"];
        assert_eq!(p.name, "Create");
        assert_eq!(p.game_version.as_deref(), Some("1.20.1"));
        assert_eq!(p.loader, Loader { kind: LoaderKind::Fabric, version: Some("0.18.4".into()) });
    }

    #[test]
    fn reads_legacy_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE profiles (path TEXT PRIMARY KEY, name TEXT, game_version TEXT,
                    mod_loader TEXT, mod_loader_version TEXT);
                 INSERT INTO profiles VALUES ('old','Old Pack','1.19.2','forge','43.3.5');",
            )
            .unwrap();
        let m = query_profiles(&open_read_only(&path).unwrap()).unwrap();
        assert_eq!(m["old"].loader.kind, LoaderKind::Forge);
    }

    #[test]
    fn read_only_connection_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        current_schema_db(&path);
        let c = open_read_only(&path).unwrap();
        assert!(c.execute("DELETE FROM instances", []).is_err());
    }
}
