//! Play sessions (auto mode): pull + lock every synced world before launch,
//! heartbeat while Minecraft runs, push + unlock when its java process exits.
//!
//! A session started with "Play" waits for the game to appear. With auto
//! mode on, a watcher also picks up games launched straight from the
//! launcher; those can't be pulled first, so a newer server copy shows up
//! as a conflict on push instead of being overwritten.

use crate::{client, find_instance, record_world_in_pack, settings, slug_for, worlds_for, AppState, CmdResult};
use mcsync_core::client::SyncClient;
use mcsync_core::state::{instance_key, SyncState};
use mcsync_core::sync::{self, Outcome, Resolution, WorldRef};
use mcsync_core::{process, world};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const HEARTBEAT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_secs(3);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const AUTO_SCAN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pulling,
    WaitingForGame,
    Playing,
    Pushing,
    NeedsAttention,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub world: String,
    pub server_version: u64,
    pub base_version: Option<u64>,
    /// "pull" (before playing) or "push" (after playing)
    pub stage: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub key: String,
    pub name: String,
    pub phase: Phase,
    pub worlds: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub message: Option<String>,
    pub auto_detected: bool,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<HashMap<String, SessionView>>,
}

impl Sessions {
    pub fn snapshot(&self) -> Vec<SessionView> {
        self.inner.lock().expect("sessions").values().cloned().collect()
    }

    /// True while a session holds this instance's world locks.
    pub fn holds(&self, key: &str) -> bool {
        self.inner
            .lock()
            .expect("sessions")
            .get(key)
            .is_some_and(|s| !matches!(s.phase, Phase::Done | Phase::Failed))
    }

    fn get(&self, key: &str) -> Option<SessionView> {
        self.inner.lock().expect("sessions").get(key).cloned()
    }

    fn update(&self, key: &str, f: impl FnOnce(&mut SessionView)) {
        if let Some(s) = self.inner.lock().expect("sessions").get_mut(key) {
            f(s);
        }
    }

    /// Insert a new session unless an active one exists.
    fn begin(&self, view: SessionView) -> bool {
        let mut map = self.inner.lock().expect("sessions");
        if map.get(&view.key).is_some_and(|s| !matches!(s.phase, Phase::Done | Phase::Failed)) {
            return false;
        }
        map.insert(view.key.clone(), view);
        true
    }
}

fn app(handle: &AppHandle) -> Arc<AppState> {
    handle.state::<Arc<AppState>>().inner().clone()
}

fn emit(handle: &AppHandle) {
    let _ = handle.emit("sessions-changed", app(handle).sessions.snapshot());
}

fn set_phase(handle: &AppHandle, key: &str, phase: Phase, message: Option<String>) {
    app(handle).sessions.update(key, |s| {
        s.phase = phase;
        s.message = message;
    });
    emit(handle);
}

struct Ctx {
    client: SyncClient,
    game_dir: PathBuf,
    slug: String,
    settings: mcsync_core::config::Settings,
}

async fn ctx(handle: &AppHandle, key: &str) -> CmdResult<(Ctx, String)> {
    let s = settings()?;
    let (_, inst) = find_instance(&s, key)?;
    let slug = slug_for(&*app(handle).sync.lock().await, &inst);
    Ok((Ctx { client: client(&s)?, game_dir: inst.game_dir.clone(), slug, settings: s }, inst.name))
}

impl Ctx {
    fn world_ref<'a>(&'a self, name: &'a str) -> WorldRef<'a> {
        WorldRef { game_dir: &self.game_dir, world: name, id: world::world_id(&self.slug, name) }
    }
}

async fn with_state<T>(handle: &AppHandle, f: impl AsyncFnOnce(&mut SyncState) -> T) -> CmdResult<T> {
    let a = app(handle);
    let mut st = a.sync.lock().await;
    let out = f(&mut st).await;
    st.save(&SyncState::default_path()).map_err(|e| e.to_string())?;
    Ok(out)
}

/// "Play": pull + lock, then wait for the game.
pub async fn start(handle: AppHandle, key: String) -> CmdResult<()> {
    let (c, name) = ctx(&handle, &key).await?;
    if !process::find_game_processes(&c.game_dir).is_empty() {
        return Err("Minecraft is already running from this instance.".into());
    }
    let s = settings()?;
    let (_, inst) = find_instance(&s, &key)?;
    let worlds = worlds_for(&s, &c.slug, &inst);
    let view = SessionView {
        key: key.clone(),
        name,
        phase: Phase::Pulling,
        worlds: worlds.clone(),
        conflicts: Vec::new(),
        message: None,
        auto_detected: false,
    };
    if !app(&handle).sessions.begin(view) {
        return Err("A session is already running for this instance.".into());
    }
    emit(&handle);

    // Lock everything first so nobody else starts while we pull.
    for w in &worlds {
        if let Err(e) = c.client.lock(&c.world_ref(w).id).await {
            release_all(&c, &worlds).await;
            set_phase(&handle, &key, Phase::Failed, Some(format!("Could not lock '{w}': {e}")));
            return Ok(());
        }
    }
    let mut conflicts = Vec::new();
    for w in &worlds {
        let wr = c.world_ref(w);
        match with_state(&handle, async |st| sync::pull_world(&c.client, st, &wr, None).await).await? {
            Ok(Outcome::Conflict { server_version, base_version }) => {
                conflicts.push(Conflict { world: w.clone(), server_version, base_version, stage: "pull" })
            }
            Ok(_) => {}
            Err(e) => {
                release_all(&c, &worlds).await;
                set_phase(&handle, &key, Phase::Failed, Some(format!("Pull of '{w}' failed: {e}")));
                return Ok(());
            }
        }
    }
    let has_conflicts = !conflicts.is_empty();
    app(&handle).sessions.update(&key, |s| s.conflicts = conflicts);
    if has_conflicts {
        set_phase(&handle, &key, Phase::NeedsAttention, Some("Resolve conflicts before launching.".into()));
    } else {
        set_phase(&handle, &key, Phase::WaitingForGame, Some("Launch the instance from your launcher.".into()));
    }
    tokio::spawn(run(handle, key, c));
    Ok(())
}

/// Heartbeat + process watch loop; pushes when the game exits.
async fn run(handle: AppHandle, key: String, c: Ctx) {
    let started = Instant::now();
    let mut last_beat = Instant::now();
    let mut pid: Option<u32> = None;
    loop {
        tokio::time::sleep(POLL).await;
        let Some(view) = app(&handle).sessions.get(&key) else { return };
        if matches!(view.phase, Phase::Done | Phase::Failed) {
            return;
        }
        if last_beat.elapsed() >= HEARTBEAT {
            for w in &view.worlds {
                if let Err(e) = c.client.heartbeat(&c.world_ref(w).id).await {
                    tracing::warn!("heartbeat for {w} failed: {e}");
                }
            }
            last_beat = Instant::now();
        }
        match view.phase {
            Phase::WaitingForGame => {
                if let Some(p) = process::find_game_processes(&c.game_dir).first() {
                    pid = Some(*p);
                    set_phase(&handle, &key, Phase::Playing, None);
                } else if started.elapsed() > LAUNCH_TIMEOUT {
                    release_all(&c, &view.worlds).await;
                    set_phase(&handle, &key, Phase::Done, Some("Game never started; locks released.".into()));
                    return;
                }
            }
            Phase::Playing => {
                let alive = match pid {
                    Some(p) => process::is_running(p),
                    None => match process::find_game_processes(&c.game_dir).first() {
                        Some(p) => {
                            pid = Some(*p);
                            true
                        }
                        None => false,
                    },
                };
                if !alive {
                    push_all(&handle, &key, &c).await;
                }
            }
            // Pulling / Pushing are transient; NeedsAttention waits for the
            // user while locks stay alive via heartbeats.
            _ => {}
        }
    }
}

async fn push_all(handle: &AppHandle, key: &str, c: &Ctx) {
    set_phase(handle, key, Phase::Pushing, None);
    // Include worlds created during this session.
    let mut worlds = app(handle).sessions.get(key).map(|s| s.worlds).unwrap_or_default();
    for w in world::list_worlds(&c.game_dir) {
        if !worlds.contains(&w) {
            worlds.push(w);
        }
    }
    let mut conflicts = Vec::new();
    let mut errors = Vec::new();
    for w in &worlds {
        let wr = c.world_ref(w);
        if let Err(e) = c.client.lock(&wr.id).await {
            errors.push(format!("{w}: {e}"));
            continue;
        }
        match with_state(handle, async |st| sync::push_world(&c.client, st, &wr, None).await).await {
            Ok(Ok(Outcome::Conflict { server_version, base_version })) => {
                conflicts.push(Conflict { world: w.clone(), server_version, base_version, stage: "push" });
                continue; // keep the lock until resolved
            }
            Ok(Ok(Outcome::Pushed { .. })) => {
                if let Err(e) = record_world_in_pack(&c.settings, &c.slug, w) {
                    tracing::warn!("could not record world {w}: {e}");
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => errors.push(format!("{w}: {e}")),
            Err(e) => errors.push(format!("{w}: {e}")),
        }
        let _ = c.client.unlock(&wr.id).await;
    }
    let pending: Vec<String> = conflicts.iter().map(|c| c.world.clone()).collect();
    app(handle).sessions.update(key, |s| {
        s.worlds = pending;
        s.conflicts = conflicts.clone();
    });
    if !conflicts.is_empty() {
        set_phase(handle, key, Phase::NeedsAttention, Some("Some worlds changed on another device.".into()));
    } else if !errors.is_empty() {
        set_phase(handle, key, Phase::Failed, Some(errors.join("; ")));
    } else {
        set_phase(handle, key, Phase::Done, Some("Worlds pushed and unlocked.".into()));
    }
}

async fn release_all(c: &Ctx, worlds: &[String]) {
    for w in worlds {
        let _ = c.client.unlock(&c.world_ref(w).id).await;
    }
}

/// Apply the user's conflict choice for one world.
pub async fn resolve(handle: AppHandle, key: String, world_name: String, resolution: Resolution) -> CmdResult<()> {
    let view = app(&handle).sessions.get(&key).ok_or("no such session")?;
    let conflict = view
        .conflicts
        .iter()
        .find(|c| c.world == world_name)
        .cloned()
        .ok_or("no conflict for that world")?;
    let (c, _) = ctx(&handle, &key).await?;
    let wr = c.world_ref(&world_name);
    let out = if conflict.stage == "pull" {
        with_state(&handle, async |st| sync::pull_world(&c.client, st, &wr, Some(resolution)).await).await?
    } else {
        with_state(&handle, async |st| sync::push_world(&c.client, st, &wr, Some(resolution)).await).await?
    }
    .map_err(|e| e.to_string())?;
    if conflict.stage == "push" {
        if matches!(out, Outcome::Pushed { .. }) {
            record_world_in_pack(&c.settings, &c.slug, &world_name)?;
        }
        let _ = c.client.unlock(&wr.id).await;
    }

    let mut remaining = Vec::new();
    app(&handle).sessions.update(&key, |s| {
        s.conflicts.retain(|x| x.world != world_name);
        if conflict.stage == "push" {
            s.worlds.retain(|w| w != &world_name);
        }
        remaining = s.conflicts.clone();
    });
    if remaining.is_empty() {
        let next = if conflict.stage == "pull" {
            (Phase::WaitingForGame, "Launch the instance from your launcher.")
        } else {
            (Phase::Done, "Worlds pushed and unlocked.")
        };
        set_phase(&handle, &key, next.0, Some(next.1.into()));
    } else {
        emit(&handle);
    }
    Ok(())
}

/// Cancel a session and release its locks (no push).
pub async fn cancel(handle: AppHandle, key: String) -> CmdResult<()> {
    let view = app(&handle).sessions.get(&key).ok_or("no such session")?;
    if view.phase == Phase::Playing {
        return Err("Minecraft is still running; the session will push when it exits.".into());
    }
    if let Ok((c, _)) = ctx(&handle, &key).await {
        release_all(&c, &view.worlds).await;
    }
    set_phase(&handle, &key, Phase::Done, Some("Cancelled; locks released.".into()));
    Ok(())
}

/// With auto mode on, notice games started directly from a launcher for
/// linked instances, lock their worlds, and push on exit.
pub fn spawn_auto_watcher(handle: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(AUTO_SCAN).await;
            let Ok(s) = settings() else { continue };
            if !s.auto_mode || s.server_url.is_none() {
                continue;
            }
            let linked: Vec<String> = app(&handle).sync.lock().await.links.keys().cloned().collect();
            if linked.is_empty() {
                continue;
            }
            let instances: Vec<_> = mcsync_core::launcher::detect(&s.launchers)
                .iter()
                .flat_map(|l| l.list_instances().unwrap_or_default())
                .filter(|i| linked.contains(&instance_key(i)))
                .collect();
            for inst in instances {
                let key = instance_key(&inst);
                if app(&handle).sessions.holds(&key) || process::find_game_processes(&inst.game_dir).is_empty() {
                    continue;
                }
                if let Err(e) = adopt(&handle, &key).await {
                    tracing::warn!("auto mode could not adopt {}: {e}", inst.name);
                }
            }
        }
    });
}

async fn adopt(handle: &AppHandle, key: &str) -> CmdResult<()> {
    let (c, name) = ctx(handle, key).await?;
    let s = settings()?;
    let (_, inst) = find_instance(&s, key)?;
    let worlds = worlds_for(&s, &c.slug, &inst);
    let view = SessionView {
        key: key.to_string(),
        name,
        phase: Phase::Playing,
        worlds: worlds.clone(),
        conflicts: Vec::new(),
        message: Some("Game started outside mcsync; worlds will be pushed when it exits.".into()),
        auto_detected: true,
    };
    if !app(handle).sessions.begin(view) {
        return Ok(());
    }
    for w in &worlds {
        if let Err(e) = c.client.lock(&c.world_ref(w).id).await {
            app(handle).sessions.update(key, |s| {
                s.message = Some(format!("Warning: could not lock '{w}': {e}"));
            });
        }
    }
    emit(handle);
    tokio::spawn(run(handle.clone(), key.to_string(), c));
    Ok(())
}
