//! SQLite metadata: devices, worlds, snapshots, locks.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockRow {
    pub device: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorldRow {
    pub id: String,
    pub version: u64,
    pub size: u64,
    pub updated_at: i64,
    pub updated_by: Option<String>,
    pub lock: Option<LockRow>,
    pub versions: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceRow {
    pub name: String,
    pub created_at: i64,
    pub last_seen: Option<i64>,
    pub revoked: bool,
}

pub enum LockResult {
    Acquired(LockRow),
    HeldBy(String),
}

pub enum CommitResult {
    Committed { version: u64, pruned: Vec<u64> },
    Conflict { server_version: u64 },
    NotLockHolder { holder: Option<String> },
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
    name TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_seen INTEGER,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS worlds (
    id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT
);
CREATE TABLE IF NOT EXISTS snapshots (
    world_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    device TEXT NOT NULL,
    PRIMARY KEY (world_id, version)
);
CREATE TABLE IF NOT EXISTS locks (
    world_id TEXT PRIMARY KEY,
    device TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
";

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- devices ----

    pub fn add_device(&self, name: &str, token_hash: &str) -> anyhow::Result<()> {
        let n = self.conn().execute(
            "INSERT INTO devices (name, token_hash, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET token_hash = excluded.token_hash, revoked = 0",
            params![name, token_hash, crate::now()],
        )?;
        anyhow::ensure!(n == 1, "could not store device");
        Ok(())
    }

    pub fn revoke_device(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self.conn().execute("UPDATE devices SET revoked = 1 WHERE name = ?1", [name])? > 0)
    }

    pub fn list_devices(&self) -> anyhow::Result<Vec<DeviceRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, created_at, last_seen, revoked FROM devices ORDER BY name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DeviceRow { name: r.get(0)?, created_at: r.get(1)?, last_seen: r.get(2)?, revoked: r.get::<_, i64>(3)? != 0 })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn device_for_token(&self, token_hash: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn();
        let name: Option<String> = conn
            .query_row(
                "SELECT name FROM devices WHERE token_hash = ?1 AND revoked = 0",
                [token_hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(n) = &name {
            conn.execute("UPDATE devices SET last_seen = ?1 WHERE name = ?2", params![crate::now(), n])?;
        }
        Ok(name)
    }

    // ---- worlds ----

    fn lock_of(conn: &Connection, id: &str, now: i64) -> rusqlite::Result<Option<LockRow>> {
        conn.query_row(
            "SELECT device, expires_at FROM locks WHERE world_id = ?1 AND expires_at > ?2",
            params![id, now],
            |r| Ok(LockRow { device: r.get(0)?, expires_at: r.get(1)? }),
        )
        .optional()
    }

    fn versions_of(conn: &Connection, id: &str) -> rusqlite::Result<Vec<u64>> {
        let mut stmt = conn.prepare("SELECT version FROM snapshots WHERE world_id = ?1 ORDER BY version")?;
        let v = stmt.query_map([id], |r| r.get::<_, i64>(0).map(|v| v as u64))?.collect();
        v
    }

    pub fn world(&self, id: &str) -> anyhow::Result<Option<WorldRow>> {
        let conn = self.conn();
        let now = crate::now();
        let row = conn
            .query_row(
                "SELECT id, version, size, updated_at, updated_by FROM worlds WHERE id = ?1",
                [id],
                |r| {
                    Ok(WorldRow {
                        id: r.get(0)?,
                        version: r.get::<_, i64>(1)? as u64,
                        size: r.get::<_, i64>(2)? as u64,
                        updated_at: r.get(3)?,
                        updated_by: r.get(4)?,
                        lock: None,
                        versions: Vec::new(),
                    })
                },
            )
            .optional()?;
        let lock = Self::lock_of(&conn, id, now)?;
        Ok(match row {
            Some(mut w) => {
                w.lock = lock;
                w.versions = Self::versions_of(&conn, id)?;
                Some(w)
            }
            // A world can be locked before its first push.
            None => lock.map(|l| WorldRow {
                id: id.to_string(),
                version: 0,
                size: 0,
                updated_at: 0,
                updated_by: None,
                lock: Some(l),
                versions: Vec::new(),
            }),
        })
    }

    pub fn list_worlds(&self) -> anyhow::Result<Vec<WorldRow>> {
        let ids: Vec<String> = {
            let conn = self.conn();
            let mut stmt = conn.prepare("SELECT id FROM worlds UNION SELECT world_id FROM locks ORDER BY 1")?;
            let ids = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        let mut out = Vec::new();
        for id in ids {
            if let Some(w) = self.world(&id)? {
                out.push(w);
            }
        }
        Ok(out)
    }

    pub fn snapshot_meta(&self, id: &str, version: Option<u64>) -> anyhow::Result<Option<(u64, u64, String)>> {
        let conn = self.conn();
        let sql = "SELECT version, size, sha256 FROM snapshots WHERE world_id = ?1 AND (?2 IS NULL OR version = ?2)
                   ORDER BY version DESC LIMIT 1";
        Ok(conn
            .query_row(sql, params![id, version.map(|v| v as i64)], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get(2)?))
            })
            .optional()?)
    }

    // ---- locks ----

    /// Acquire or refresh a lock. Free, expired, or already ours -> ours.
    pub fn lock(&self, id: &str, device: &str, ttl: i64) -> anyhow::Result<LockResult> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let now = crate::now();
        if let Some(l) = Self::lock_of(&tx, id, now)? {
            if l.device != device {
                return Ok(LockResult::HeldBy(l.device));
            }
        }
        let expires_at = now + ttl;
        tx.execute(
            "INSERT INTO locks (world_id, device, expires_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(world_id) DO UPDATE SET device = excluded.device, expires_at = excluded.expires_at",
            params![id, device, expires_at],
        )?;
        tx.commit()?;
        Ok(LockResult::Acquired(LockRow { device: device.to_string(), expires_at }))
    }

    /// Release a lock. Ok(false) if another device holds it.
    pub fn unlock(&self, id: &str, device: &str) -> anyhow::Result<Result<(), String>> {
        let conn = self.conn();
        if let Some(l) = Self::lock_of(&conn, id, crate::now())? {
            if l.device != device {
                return Ok(Err(l.device));
            }
        }
        conn.execute("DELETE FROM locks WHERE world_id = ?1", [id])?;
        Ok(Ok(()))
    }

    pub fn lock_holder(&self, id: &str) -> anyhow::Result<Option<String>> {
        Ok(Self::lock_of(&self.conn(), id, crate::now())?.map(|l| l.device))
    }

    // ---- snapshots ----

    pub fn current_version(&self, id: &str) -> anyhow::Result<u64> {
        Ok(self
            .conn()
            .query_row("SELECT version FROM worlds WHERE id = ?1", [id], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0) as u64)
    }

    /// Atomically check lock + base version and record a new snapshot.
    /// `place` moves the uploaded file into its final location for the
    /// assigned version; it runs inside the transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_snapshot(
        &self,
        id: &str,
        device: &str,
        base: u64,
        force: bool,
        size: u64,
        sha256: &str,
        keep: u64,
        place: impl FnOnce(u64) -> std::io::Result<()>,
    ) -> anyhow::Result<CommitResult> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let now = crate::now();
        let holder = Self::lock_of(&tx, id, now)?.map(|l| l.device);
        if holder.as_deref() != Some(device) {
            return Ok(CommitResult::NotLockHolder { holder });
        }
        let current: u64 = tx
            .query_row("SELECT version FROM worlds WHERE id = ?1", [id], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0) as u64;
        if !force && base != current {
            return Ok(CommitResult::Conflict { server_version: current });
        }
        let version = current + 1;
        place(version)?;
        tx.execute(
            "INSERT INTO snapshots (world_id, version, size, sha256, created_at, device) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, version as i64, size as i64, sha256, now, device],
        )?;
        tx.execute(
            "INSERT INTO worlds (id, version, size, sha256, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version, size = excluded.size,
               sha256 = excluded.sha256, updated_at = excluded.updated_at, updated_by = excluded.updated_by",
            params![id, version as i64, size as i64, sha256, now, device],
        )?;
        let pruned: Vec<u64> = if version > keep {
            let cutoff = (version - keep) as i64;
            let mut stmt = tx.prepare("SELECT version FROM snapshots WHERE world_id = ?1 AND version <= ?2")?;
            let old = stmt.query_map(params![id, cutoff], |r| r.get::<_, i64>(0).map(|v| v as u64))?.collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM snapshots WHERE world_id = ?1 AND version <= ?2", params![id, cutoff])?;
            old
        } else {
            Vec::new()
        };
        tx.commit()?;
        Ok(CommitResult::Committed { version, pruned })
    }
}
