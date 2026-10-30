//! What part of a game directory is synced through the pack repo.
//!
//! Mods, resource packs and shader packs are tracked by hash in the manifest.
//! `config/`, `options.txt` (and resource/shader packs that cannot be resolved
//! remotely) travel as overrides. Account/auth files, logs, crash reports,
//! caches and worlds are never synced through git.

use std::path::Path;

/// Top-level folders whose files are resolved by hash.
pub const HASHED_DIRS: &[&str] = &["mods", "resourcepacks", "shaderpacks"];

/// Top-level entries copied verbatim as overrides.
pub const OVERRIDE_ROOTS: &[&str] = &["config", "options.txt"];

/// Path components that are never synced, wherever they appear.
const DENY_COMPONENTS: &[&str] = &[
    "logs",
    "crash-reports",
    "cache",
    ".cache",
    "caches",
    "saves",
    "screenshots",
    ".fabric",
    ".mixin.out",
    "natives",
];

/// File names that are never synced (auth, sessions, telemetry).
const DENY_FILES: &[&str] = &[
    "accounts.json",
    "launcher_accounts.json",
    "launcher_profiles.json",
    "usercache.json",
    "usernamecache.json",
    "session.lock",
    "servers.dat_old",
    "options.txt.bak",
];

const DENY_EXTENSIONS: &[&str] = &["log", "tmp", "lock", "bak"];

/// Max size of a single override file; larger files are skipped so the git
/// repo stays small.
pub const MAX_OVERRIDE_FILE: u64 = 10 * 1024 * 1024;

/// Returns true if a game-dir relative path may be synced via the pack repo.
pub fn is_syncable(rel: &Path) -> bool {
    let mut comps = rel.components().map(|c| c.as_os_str().to_string_lossy());
    let Some(first) = comps.next() else {
        return false;
    };
    let in_scope = OVERRIDE_ROOTS.contains(&first.as_ref()) || HASHED_DIRS.contains(&first.as_ref());
    if !in_scope {
        return false;
    }
    if rel
        .components()
        .any(|c| DENY_COMPONENTS.contains(&c.as_os_str().to_string_lossy().as_ref()))
    {
        return false;
    }
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if DENY_FILES.contains(&name.as_str()) || name.contains("token") || name.contains("credential") {
        return false;
    }
    if let Some(ext) = rel.extension() {
        if DENY_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str()) {
            return false;
        }
    }
    true
}

/// Validates a relative path from an untrusted manifest: no absolute paths,
/// no `..`, no drive prefixes, and it must be in sync scope.
pub fn is_safe_relative(rel: &str) -> bool {
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') || rel.contains(':') {
        return false;
    }
    let p = Path::new(rel);
    p.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        && is_syncable(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_rules() {
        assert!(is_syncable(Path::new("config/sodium.json")));
        assert!(is_syncable(Path::new("options.txt")));
        assert!(is_syncable(Path::new("mods/foo.jar")));
        assert!(!is_syncable(Path::new("logs/latest.log")));
        assert!(!is_syncable(Path::new("saves/World/level.dat")));
        assert!(!is_syncable(Path::new("config/cache/x.json")));
        assert!(!is_syncable(Path::new("config/foo.log")));
        assert!(!is_syncable(Path::new("config/mytoken.json")));
        assert!(!is_syncable(Path::new("crash-reports/a.txt")));
        assert!(!is_syncable(Path::new("accounts.json")));
    }

    #[test]
    fn rejects_traversal() {
        assert!(is_safe_relative("config/a.json"));
        assert!(!is_safe_relative("../config/a.json"));
        assert!(!is_safe_relative("config/../../x"));
        assert!(!is_safe_relative("/etc/passwd"));
        assert!(!is_safe_relative("C:/Windows/x"));
        assert!(!is_safe_relative("saves/w/level.dat"));
    }
}
