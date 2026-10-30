//! Tauri backend for mcsync. Thin layer over `mcsync-core`.
//!
//! The webview never sees secret values (only whether each is set) and gets
//! no filesystem/opener permissions; file reveals go through
//! `reveal_path`, which only accepts paths under the export dir or an
//! instance's backup folders.

mod session;

use mcsync_core::client::SyncClient;
use mcsync_core::config::{Secret, Settings};
use mcsync_core::git::{PackRepo, PullResult};
use mcsync_core::launcher::{self, Instance, Launcher, WriteOutcome};
use mcsync_core::manifest::{slugify, Manifest};
use mcsync_core::pack::{self, ApplyReport, ExportReport, InstallReport};
use mcsync_core::resolve::{http_client, Resolver};
use mcsync_core::state::{instance_key, world_key, SyncState};
use mcsync_core::sync::{self, Outcome, Resolution, WorldRef};
use mcsync_core::{process, world};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, State};
use tokio::sync::Mutex;

pub type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub struct AppState {
    pub sync: Mutex<SyncState>,
    pub sessions: session::Sessions,
}

impl AppState {
    pub async fn save_state(&self) -> CmdResult<()> {
        self.sync.lock().await.save(&SyncState::default_path()).map_err(err)
    }
}

// ---------- helpers ----------

pub fn settings() -> CmdResult<Settings> {
    Settings::load().map_err(err)
}

pub fn find_instance(settings: &Settings, key: &str) -> CmdResult<(Box<dyn Launcher>, Instance)> {
    for l in launcher::detect(&settings.launchers) {
        if let Ok(list) = l.list_instances() {
            if let Some(i) = list.into_iter().find(|i| instance_key(i) == key) {
                return Ok((l, i));
            }
        }
    }
    Err(format!("instance not found: {key}"))
}

pub fn client(settings: &Settings) -> CmdResult<SyncClient> {
    let url = settings.server_url.as_deref().ok_or("Set the server URL in Settings first.")?;
    let token = Secret::DeviceToken.get().map_err(err)?.ok_or("Set the device token in Settings first.")?;
    SyncClient::new(url, &token).map_err(err)
}

pub fn slug_for(state: &SyncState, inst: &Instance) -> String {
    state.pack_for(inst).map(str::to_string).unwrap_or_else(|| slugify(&inst.name))
}

fn pack_dir(settings: &Settings, slug: &str) -> PathBuf {
    settings.repo_dir.join("packs").join(slug)
}

fn open_repo(settings: &Settings) -> CmdResult<PackRepo> {
    PackRepo::open_or_init(&settings.repo_dir, settings.git_repo_url.as_deref(), Secret::GitToken.get().map_err(err)?)
        .map_err(err)
}

/// World folder names to sync for an instance: local worlds plus worlds
/// other devices recorded in the pack's `worlds.json`.
pub fn worlds_for(settings: &Settings, slug: &str, inst: &Instance) -> Vec<String> {
    let mut names = world::list_worlds(&inst.game_dir);
    for n in pack::pack_worlds(&pack_dir(settings, slug)) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names.sort();
    names
}

fn check_not_running(inst: &Instance) -> CmdResult<()> {
    if process::find_game_processes(&inst.game_dir).is_empty() {
        Ok(())
    } else {
        Err("Minecraft is running from this instance. Close it first.".into())
    }
}

// ---------- views ----------

#[derive(Serialize)]
struct InstanceView {
    key: String,
    launcher: String,
    launcher_label: String,
    name: String,
    mc_version: Option<String>,
    loader: String,
    loader_version: Option<String>,
    game_dir: PathBuf,
    pack: Option<String>,
    pack_version: Option<String>,
    running: bool,
}

#[derive(Serialize)]
struct LauncherView {
    label: String,
    kind: String,
    data_dir: PathBuf,
    error: Option<String>,
}

#[derive(Serialize)]
struct Overview {
    launchers: Vec<LauncherView>,
    instances: Vec<InstanceView>,
    repo_dirty: Option<bool>,
}

#[tauri::command]
async fn get_overview(app: State<'_, Arc<AppState>>) -> CmdResult<Overview> {
    let s = settings()?;
    let state = app.sync.lock().await.clone();
    tokio::task::spawn_blocking(move || {
        let mut launchers = Vec::new();
        let mut instances = Vec::new();
        for l in launcher::detect(&s.launchers) {
            let list = l.list_instances();
            launchers.push(LauncherView {
                label: l.label().into(),
                kind: l.kind().to_string(),
                data_dir: l.data_dir().into(),
                error: list.as_ref().err().map(|e| e.to_string()),
            });
            for i in list.unwrap_or_default() {
                let pack = state.pack_for(&i).map(str::to_string);
                let pack_version = pack
                    .as_ref()
                    .and_then(|p| Manifest::load(&pack_dir(&s, p)).ok())
                    .map(|m| m.version_id);
                instances.push(InstanceView {
                    key: instance_key(&i),
                    launcher: i.launcher.to_string(),
                    launcher_label: i.launcher_label.clone(),
                    name: i.name.clone(),
                    mc_version: i.mc_version.clone(),
                    loader: i.loader.kind.to_string(),
                    loader_version: i.loader.version.clone(),
                    running: false,
                    game_dir: i.game_dir.clone(),
                    pack,
                    pack_version,
                });
            }
        }
        let repo_dirty = PackRepo::open_or_init(&s.repo_dir, None, None).ok().and_then(|r| r.is_dirty().ok());
        Ok(Overview { launchers, instances, repo_dirty })
    })
    .await
    .map_err(err)?
}

#[derive(Serialize)]
struct WorldView {
    name: String,
    id: String,
    local: bool,
    base_version: Option<u64>,
    local_changed: bool,
    server_version: Option<u64>,
    lock_holder: Option<String>,
    status: &'static str,
}

#[derive(Serialize)]
struct Detail {
    pack: String,
    pack_exists: bool,
    manual: Vec<String>,
    running: bool,
    worlds: Vec<WorldView>,
    server_error: Option<String>,
}

#[tauri::command]
async fn instance_detail(app: State<'_, Arc<AppState>>, key: String) -> CmdResult<Detail> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    let state = app.sync.lock().await.clone();
    let slug = slug_for(&state, &inst);
    let manifest = Manifest::load(&pack_dir(&s, &slug)).ok();
    let mut server_error = None;
    let client = match client(&s) {
        Ok(c) => Some(c),
        Err(e) => {
            server_error = Some(e);
            None
        }
    };
    let mut worlds = Vec::new();
    for name in worlds_for(&s, &slug, &inst) {
        let id = world::world_id(&slug, &name);
        let dir = inst.game_dir.join("saves").join(&name);
        let local = dir.join("level.dat").is_file();
        let ws = state.worlds.get(&world_key(&inst.game_dir, &name));
        let local_changed = local
            && world::fingerprint(&dir).ok().as_deref() != ws.and_then(|w| w.fingerprint.as_deref());
        let (server_version, lock_holder) = match &client {
            Some(c) => match c.world(&id).await {
                Ok(Some(info)) => (Some(info.version), info.lock.map(|l| l.device)),
                Ok(None) => (None, None),
                Err(e) => {
                    server_error.get_or_insert(e.to_string());
                    (None, None)
                }
            },
            None => (None, None),
        };
        let base = ws.and_then(|w| w.base_version);
        let server_newer = matches!((server_version, base), (Some(sv), b) if sv > 0 && b != Some(sv));
        let status = match (local, server_version.filter(|v| *v > 0)) {
            (false, Some(_)) => "server_only",
            (true, None) => "local_only",
            (false, None) => "missing",
            _ if server_newer && local_changed => "conflict",
            _ if server_newer => "behind",
            _ if local_changed => "ahead",
            _ => "synced",
        };
        worlds.push(WorldView { name, id, local, base_version: base, local_changed, server_version, lock_holder, status });
    }
    Ok(Detail {
        pack_exists: manifest.is_some(),
        manual: manifest.map(|m| m.manual_files().map(|f| f.path.clone()).collect()).unwrap_or_default(),
        pack: slug,
        running: !process::find_game_processes(&inst.game_dir).is_empty(),
        worlds,
        server_error,
    })
}

// ---------- packs ----------

#[tauri::command]
async fn export_pack(app: State<'_, Arc<AppState>>, key: String, slug: Option<String>) -> CmdResult<ExportReport> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    let slug = {
        let state = app.sync.lock().await;
        slug.filter(|v| !v.trim().is_empty()).map(|v| slugify(&v)).unwrap_or_else(|| slug_for(&state, &inst))
    };
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || open_repo(&s2).map(|_| ())).await.map_err(err)??;
    let resolver = Resolver::new(Secret::CurseForgeApiKey.get().map_err(err)?);
    let report = pack::export_instance(&inst, &pack_dir(&s, &slug), &resolver).await.map_err(err)?;
    app.sync.lock().await.links.insert(instance_key(&inst), slug.clone());
    app.save_state().await?;
    let msg = format!("mcsync: export {slug} from {}", inst.launcher_label);
    tokio::task::spawn_blocking(move || open_repo(&s)?.commit_all(&msg).map_err(err)).await.map_err(err)??;
    Ok(report)
}

#[derive(Serialize)]
struct RepoSync {
    pull: Option<PullResult>,
    pushed: bool,
    remote: bool,
}

#[tauri::command]
async fn sync_repo() -> CmdResult<RepoSync> {
    let s = settings()?;
    tokio::task::spawn_blocking(move || {
        let repo = open_repo(&s)?;
        repo.commit_all("mcsync: local changes").map_err(err)?;
        if !repo.has_remote() {
            return Ok(RepoSync { pull: None, pushed: false, remote: false });
        }
        let pull = repo.pull().map_err(err)?;
        if let PullResult::Conflict { paths } = &pull {
            return Err(format!(
                "Pack repo conflict in {}. Resolve it in {} with git, then sync again.",
                paths.join(", "),
                s.repo_dir.display()
            ));
        }
        repo.push().map_err(err)?;
        Ok(RepoSync { pull: Some(pull), pushed: true, remote: true })
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn apply_pack(app: State<'_, Arc<AppState>>, key: String) -> CmdResult<ApplyReport> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    check_not_running(&inst)?;
    let slug = slug_for(&*app.sync.lock().await, &inst);
    pack::apply_pack(&inst.game_dir, &pack_dir(&s, &slug), &http_client()).await.map_err(err)
}

#[tauri::command]
async fn move_pack(
    app: State<'_, Arc<AppState>>,
    key: String,
    target_label: String,
    name: String,
) -> CmdResult<InstallReport> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    let slug = slug_for(&*app.sync.lock().await, &inst);
    let dir = pack_dir(&s, &slug);
    if !pack::is_pack_dir(&dir) {
        return Err("Export this instance to the pack repo first.".into());
    }
    let target = launcher::detect(&s.launchers)
        .into_iter()
        .find(|l| l.label() == target_label)
        .ok_or_else(|| format!("launcher '{target_label}' not detected"))?;
    let name = if name.trim().is_empty() { inst.name.clone() } else { name };
    let report = pack::install_pack(&dir, target.as_ref(), &name, &s.export_dir, &http_client())
        .await
        .map_err(err)?;
    if let WriteOutcome::Created(new) = &report.outcome {
        app.sync.lock().await.links.insert(instance_key(new), slug);
        app.save_state().await?;
    }
    Ok(report)
}

// ---------- worlds ----------

#[tauri::command]
async fn pull_world(
    app: State<'_, Arc<AppState>>,
    key: String,
    world: String,
    resolution: Option<Resolution>,
) -> CmdResult<Outcome> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    check_not_running(&inst)?;
    let c = client(&s)?;
    let mut state = app.sync.lock().await;
    let w = WorldRef { game_dir: &inst.game_dir, world: &world, id: world::world_id(&slug_for(&state, &inst), &world) };
    let out = sync::pull_world(&c, &mut state, &w, resolution).await.map_err(err);
    state.save(&SyncState::default_path()).map_err(err)?;
    out
}

#[tauri::command]
async fn push_world(
    app: State<'_, Arc<AppState>>,
    key: String,
    world: String,
    resolution: Option<Resolution>,
) -> CmdResult<Outcome> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    let c = client(&s)?;
    let slug = slug_for(&*app.sync.lock().await, &inst);
    let id = world::world_id(&slug, &world);
    let held_by_session = app.sessions.holds(&key);
    c.lock(&id).await.map_err(err)?;
    let out = {
        let mut state = app.sync.lock().await;
        let w = WorldRef { game_dir: &inst.game_dir, world: &world, id: id.clone() };
        let out = sync::push_world(&c, &mut state, &w, resolution).await.map_err(err);
        state.save(&SyncState::default_path()).map_err(err)?;
        out
    };
    if !held_by_session {
        c.unlock(&id).await.map_err(err)?;
    }
    if matches!(out, Ok(Outcome::Pushed { .. })) {
        record_world_in_pack(&s, &slug, &world)?;
    }
    out
}

/// Remember the world folder name in the pack so other devices can pull it.
pub fn record_world_in_pack(s: &Settings, slug: &str, world: &str) -> CmdResult<()> {
    let dir = pack_dir(s, slug);
    if pack::add_pack_world(&dir, world).map_err(err)? {
        open_repo(s)?.commit_all(&format!("mcsync: track world {world} in {slug}")).map_err(err)?;
    }
    Ok(())
}

// ---------- sessions (auto mode) ----------

#[tauri::command]
async fn start_play(handle: AppHandle, key: String) -> CmdResult<()> {
    session::start(handle, key).await
}

#[tauri::command]
async fn resolve_session_conflict(
    handle: AppHandle,
    key: String,
    world: String,
    resolution: Resolution,
) -> CmdResult<()> {
    session::resolve(handle, key, world, resolution).await
}

#[tauri::command]
async fn stop_session(handle: AppHandle, key: String) -> CmdResult<()> {
    session::cancel(handle, key).await
}

#[tauri::command]
fn get_sessions(app: State<'_, Arc<AppState>>) -> Vec<session::SessionView> {
    app.sessions.snapshot()
}

// ---------- settings ----------

#[derive(Serialize)]
struct SettingsView {
    settings: Settings,
    device_token_set: bool,
    curseforge_key_set: bool,
    git_token_set: bool,
    settings_path: PathBuf,
}

#[tauri::command]
fn get_settings() -> CmdResult<SettingsView> {
    Ok(SettingsView {
        settings: settings()?,
        device_token_set: Secret::DeviceToken.is_set(),
        curseforge_key_set: Secret::CurseForgeApiKey.is_set(),
        git_token_set: Secret::GitToken.is_set(),
        settings_path: Settings::path(),
    })
}

#[tauri::command]
fn save_settings(new: Settings) -> CmdResult<()> {
    if let Some(url) = &new.server_url {
        // Validates scheme (https, or http on localhost) without a token.
        SyncClient::new(url, "-").map_err(err)?;
    }
    new.save().map_err(err)
}

#[tauri::command]
fn set_secret(name: String, value: String) -> CmdResult<()> {
    let secret = match name.as_str() {
        "device-token" => Secret::DeviceToken,
        "curseforge-api-key" => Secret::CurseForgeApiKey,
        "git-token" => Secret::GitToken,
        _ => return Err(format!("unknown secret {name}")),
    };
    secret.set(value.trim()).map_err(err)
}

#[tauri::command]
async fn test_connection() -> CmdResult<String> {
    let c = client(&settings()?)?;
    c.health().await.map_err(err)?;
    let device = c.whoami().await.map_err(err)?;
    Ok(format!("Connected as device '{device}'."))
}

/// Reveal a file in the OS file manager. Only export files and mcsync
/// backups may be revealed.
#[tauri::command]
fn reveal_path(handle: AppHandle, path: PathBuf) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let s = settings()?;
    let canon = path.canonicalize().map_err(err)?;
    let allowed = canon.starts_with(s.export_dir.canonicalize().unwrap_or(s.export_dir.clone()))
        || canon.components().any(|c| {
            let c = c.as_os_str();
            c == world::LOCAL_BACKUPS || c == pack::BACKUP_DIR
        });
    if !allowed {
        return Err("Only export files and mcsync backups can be revealed.".into());
    }
    handle.opener().reveal_item_in_dir(&canon).map_err(err)
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let sync = SyncState::load(&SyncState::default_path()).unwrap_or_default();
    let state = Arc::new(AppState { sync: Mutex::new(sync), sessions: session::Sessions::default() });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(|app| {
            session::spawn_auto_watcher(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_overview,
            instance_detail,
            export_pack,
            sync_repo,
            apply_pack,
            move_pack,
            pull_world,
            push_world,
            start_play,
            resolve_session_conflict,
            stop_session,
            get_sessions,
            get_settings,
            save_settings,
            set_secret,
            test_connection,
            reveal_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running mcsync");
}
