use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use mcsync_core::client::SyncClient;
use mcsync_core::config::{Secret, Settings};
use mcsync_core::git::PackRepo;
use mcsync_core::launcher::{self, Instance, Launcher, LauncherKind, WriteOutcome};
use mcsync_core::manifest::{slugify, write_mrpack};
use mcsync_core::pack;
use mcsync_core::resolve::{http_client, Resolver};
use mcsync_core::state::{instance_key, SyncState};
use mcsync_core::sync::{self, Outcome, Resolution, WorldRef};
use mcsync_core::world;
use std::io::BufRead;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mcsync", version, about = "Sync Minecraft packs and worlds across launchers and PCs")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Target {
    /// Launcher: prism, pinecone, modrinth or curseforge
    #[arg(long)]
    launcher: String,
    /// Use this launcher data dir instead of the detected one
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(clap::Args, Clone)]
struct InstanceArg {
    #[command(flatten)]
    target: Target,
    /// Instance folder name or display name
    #[arg(long)]
    instance: String,
}

#[derive(Clone, Copy, ValueEnum)]
enum Choice {
    KeepServer,
    KeepLocal,
}

#[derive(Subcommand)]
enum Cmd {
    /// List detected launchers and their instances
    Detect {
        #[arg(long)]
        json: bool,
    },
    /// Export an instance to a pack in the manifest repo
    Export {
        #[command(flatten)]
        inst: InstanceArg,
        /// Pack slug (defaults to the instance name)
        #[arg(long)]
        pack: Option<String>,
        /// Write the pack here instead of the repo
        #[arg(long)]
        out: Option<PathBuf>,
        /// Commit (and push, if a remote is set) after exporting
        #[arg(long)]
        commit: bool,
    },
    /// Create a new instance in a launcher from a pack
    Import {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        pack: String,
        /// Instance name (defaults to the pack name)
        #[arg(long)]
        name: Option<String>,
        /// Read the pack from this directory instead of the repo
        #[arg(long)]
        pack_dir: Option<PathBuf>,
    },
    /// Update an existing instance from a pack (replaced files are backed up)
    Apply {
        #[command(flatten)]
        inst: InstanceArg,
        #[arg(long)]
        pack: String,
    },
    /// Write a pack as a .mrpack file
    Mrpack {
        #[arg(long)]
        pack: String,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Manifest repo operations
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// List worlds on the sync server
    Worlds,
    /// Lock, upload and unlock a world
    PushWorld {
        #[command(flatten)]
        inst: InstanceArg,
        #[arg(long)]
        world: String,
        #[arg(long, value_enum)]
        resolve: Option<Choice>,
        /// Keep holding the lock after pushing
        #[arg(long)]
        keep_lock: bool,
    },
    /// Download a world from the sync server
    PullWorld {
        #[command(flatten)]
        inst: InstanceArg,
        #[arg(long)]
        world: String,
        #[arg(long, value_enum)]
        resolve: Option<Choice>,
        /// Also take the world lock (before playing)
        #[arg(long)]
        lock: bool,
    },
    /// Release a world lock held by this device
    Unlock {
        #[command(flatten)]
        inst: InstanceArg,
        #[arg(long)]
        world: String,
    },
    /// Show or change settings
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    Status,
    Pull,
    Push {
        #[arg(short, long, default_value = "mcsync: update packs")]
        message: String,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    Show,
    /// Set server-url, git-repo-url, repo-dir, export-dir, prism-dir (adds),
    /// modrinth-dir, curseforge-dir, auto-mode
    Set { key: String, value: String },
    /// Store a secret in the OS keychain, read from stdin:
    /// device-token, curseforge-api-key, git-token
    Secret { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let settings = Settings::load()?;
    run(cli.cmd, settings).await
}

/// Pick a launcher by name. "pinecone" and "prism" both mean Prism format;
/// with several Prism-format installs, the label decides.
fn find_launcher(settings: &Settings, t: &Target) -> Result<Box<dyn Launcher>> {
    let kind = LauncherKind::parse(&t.launcher).ok_or_else(|| anyhow!("unknown launcher '{}'", t.launcher))?;
    if let Some(dir) = &t.data_dir {
        let l: Option<Box<dyn Launcher>> = match kind {
            LauncherKind::Prism => launcher::prism::PrismLauncher::open(dir).map(|l| Box::new(l) as _),
            LauncherKind::Modrinth => launcher::modrinth::ModrinthApp::open(dir).map(|l| Box::new(l) as _),
            LauncherKind::CurseForge => launcher::curseforge::CurseForge::open(dir).map(|l| Box::new(l) as _),
        };
        return l.ok_or_else(|| anyhow!("{} is not a {kind} data dir", dir.display()));
    }
    let mut all: Vec<_> = launcher::detect(&settings.launchers).into_iter().filter(|l| l.kind() == kind).collect();
    if all.is_empty() {
        bail!("no {kind} launcher detected; pass --data-dir or set a path override");
    }
    let wanted = t.launcher.to_lowercase();
    let pos = all
        .iter()
        .position(|l| {
            let label = l.label().to_lowercase();
            (wanted.contains("pinecone") || wanted.contains("ely")) && label.contains("pinecone")
                || wanted.starts_with("prism") && label.starts_with("prism launcher")
        })
        .unwrap_or(0);
    Ok(all.swap_remove(pos))
}

fn find_instance(settings: &Settings, a: &InstanceArg) -> Result<(Box<dyn Launcher>, Instance)> {
    let l = find_launcher(settings, &a.target)?;
    let inst = match l.read_instance(&a.instance) {
        Ok(i) => i,
        Err(_) => l
            .list_instances()?
            .into_iter()
            .find(|i| i.name.eq_ignore_ascii_case(&a.instance))
            .ok_or_else(|| anyhow!("no instance '{}' in {}", a.instance, l.label()))?,
    };
    Ok((l, inst))
}

fn open_repo(settings: &Settings) -> Result<PackRepo> {
    Ok(PackRepo::open_or_init(&settings.repo_dir, settings.git_repo_url.as_deref(), Secret::GitToken.get()?)?)
}

fn sync_client(settings: &Settings) -> Result<SyncClient> {
    let url = settings.server_url.as_deref().context("server-url not set (mcsync config set server-url ...)")?;
    let token = Secret::DeviceToken.get()?.context("device token not set (mcsync config secret device-token)")?;
    Ok(SyncClient::new(url, &token)?)
}

fn pack_slug(state: &SyncState, inst: &Instance) -> String {
    state.pack_for(inst).map(str::to_string).unwrap_or_else(|| slugify(&inst.name))
}

fn choice(c: Option<Choice>) -> Option<Resolution> {
    c.map(|c| match c {
        Choice::KeepServer => Resolution::KeepServer,
        Choice::KeepLocal => Resolution::KeepLocal,
    })
}

fn print_outcome(o: &Outcome) -> Result<()> {
    match o {
        Outcome::Conflict { server_version, base_version } => {
            println!(
                "CONFLICT: server is at v{server_version}, your copy was based on {}.",
                base_version.map(|v| format!("v{v}")).unwrap_or_else(|| "nothing (never synced)".into())
            );
            println!("Re-run with --resolve keep-server (backs up your copy) or --resolve keep-local (backs up the server copy).");
            std::process::exit(2);
        }
        other => println!("{}", serde_json::to_string_pretty(other)?),
    }
    Ok(())
}

async fn run(cmd: Cmd, mut settings: Settings) -> Result<()> {
    let state_path = SyncState::default_path();
    let mut state = SyncState::load(&state_path)?;
    match cmd {
        Cmd::Detect { json } => {
            let launchers = launcher::detect(&settings.launchers);
            let mut report = Vec::new();
            for l in &launchers {
                let instances = l.list_instances().unwrap_or_default();
                if !json {
                    println!("{} [{}] {}", l.label(), l.kind(), l.data_dir().display());
                    for i in &instances {
                        println!(
                            "  - {} (mc {}, {} {})",
                            i.name,
                            i.mc_version.as_deref().unwrap_or("?"),
                            i.loader.kind,
                            i.loader.version.as_deref().unwrap_or("")
                        );
                    }
                }
                report.push(serde_json::json!({
                    "label": l.label(), "kind": l.kind(), "data_dir": l.data_dir(), "instances": instances,
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if launchers.is_empty() {
                println!("No launchers detected.");
            }
        }
        Cmd::Export { inst, pack: slug, out, commit } => {
            let (_, instance) = find_instance(&settings, &inst)?;
            let slug = slug.map(|s| slugify(&s)).unwrap_or_else(|| pack_slug(&state, &instance));
            let repo = if out.is_none() { Some(open_repo(&settings)?) } else { None };
            let dir = match (&out, &repo) {
                (Some(o), _) => o.clone(),
                (None, Some(r)) => r.packs_dir().join(&slug),
                _ => unreachable!(),
            };
            let resolver = Resolver::new(Secret::CurseForgeApiKey.get()?);
            let report = pack::export_instance(&instance, &dir, &resolver).await?;
            state.links.insert(instance_key(&instance), slug.clone());
            state.save(&state_path)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.manual.is_empty() {
                eprintln!("{} mod(s) need manual download on other PCs (see 'manual').", report.manual.len());
            }
            if let (true, Some(r)) = (commit, &repo) {
                if r.commit_all(&format!("mcsync: export {slug}"))?.is_some() && r.has_remote() {
                    r.push()?;
                    println!("Committed and pushed.");
                }
            }
        }
        Cmd::Import { target, pack: slug, name, pack_dir } => {
            let l = find_launcher(&settings, &target)?;
            let dir = match pack_dir {
                Some(d) => d,
                None => open_repo(&settings)?.packs_dir().join(slugify(&slug)),
            };
            let manifest = mcsync_core::manifest::Manifest::load(&dir)?;
            let name = name.unwrap_or_else(|| manifest.name.clone());
            let report = pack::install_pack(&dir, l.as_ref(), &name, &settings.export_dir, &http_client()).await?;
            if let WriteOutcome::Created(i) = &report.outcome {
                state.links.insert(instance_key(i), slugify(&slug));
                state.save(&state_path)?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            if let WriteOutcome::ImportFile { path } = &report.outcome {
                println!("Import {} in {}.", path.display(), l.label());
            }
        }
        Cmd::Apply { inst, pack: slug } => {
            let (_, instance) = find_instance(&settings, &inst)?;
            if !mcsync_core::process::find_game_processes(&instance.game_dir).is_empty() {
                bail!("Minecraft is running from this instance; close it first");
            }
            let dir = open_repo(&settings)?.packs_dir().join(slugify(&slug));
            let report = pack::apply_pack(&instance.game_dir, &dir, &http_client()).await?;
            state.links.insert(instance_key(&instance), slugify(&slug));
            state.save(&state_path)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::Mrpack { pack: slug, out } => {
            let dir = open_repo(&settings)?.packs_dir().join(slugify(&slug));
            write_mrpack(&dir, &out)?;
            println!("Wrote {}", out.display());
        }
        Cmd::Repo { cmd } => {
            let repo = open_repo(&settings)?;
            match cmd {
                RepoCmd::Status => println!(
                    "repo: {}\nremote: {}\ndirty: {}",
                    repo.workdir().display(),
                    settings.git_repo_url.as_deref().unwrap_or("(none)"),
                    repo.is_dirty()?
                ),
                RepoCmd::Pull => println!("{}", serde_json::to_string(&repo.pull()?)?),
                RepoCmd::Push { message } => {
                    repo.commit_all(&message)?;
                    repo.push()?;
                    println!("Pushed.");
                }
            }
        }
        Cmd::Worlds => {
            let client = sync_client(&settings)?;
            println!("{}", serde_json::to_string_pretty(&client.list_worlds().await?)?);
        }
        Cmd::PushWorld { inst, world, resolve, keep_lock } => {
            let (_, instance) = find_instance(&settings, &inst)?;
            let client = sync_client(&settings)?;
            let w = WorldRef { game_dir: &instance.game_dir, world: &world, id: world::world_id(&pack_slug(&state, &instance), &world) };
            client.lock(&w.id).await?;
            let result = sync::push_world(&client, &mut state, &w, choice(resolve)).await;
            state.save(&state_path)?;
            if !keep_lock {
                client.unlock(&w.id).await?;
            }
            print_outcome(&result?)?;
        }
        Cmd::PullWorld { inst, world, resolve, lock } => {
            let (_, instance) = find_instance(&settings, &inst)?;
            if !mcsync_core::process::find_game_processes(&instance.game_dir).is_empty() {
                bail!("Minecraft is running from this instance; close it first");
            }
            let client = sync_client(&settings)?;
            let w = WorldRef { game_dir: &instance.game_dir, world: &world, id: world::world_id(&pack_slug(&state, &instance), &world) };
            if lock {
                client.lock(&w.id).await?;
            }
            let result = sync::pull_world(&client, &mut state, &w, choice(resolve)).await;
            state.save(&state_path)?;
            print_outcome(&result?)?;
        }
        Cmd::Unlock { inst, world } => {
            let (_, instance) = find_instance(&settings, &inst)?;
            let client = sync_client(&settings)?;
            client.unlock(&world::world_id(&pack_slug(&state, &instance), &world)).await?;
            println!("Unlocked.");
        }
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Show => {
                println!("# {}", Settings::path().display());
                println!("{}", serde_json::to_string_pretty(&settings)?);
                for s in [Secret::DeviceToken, Secret::CurseForgeApiKey, Secret::GitToken] {
                    println!("{s:?}: {}", if s.is_set() { "set" } else { "not set" });
                }
            }
            ConfigCmd::Set { key, value } => {
                let opt = |v: &str| if v.is_empty() { None } else { Some(v.to_string()) };
                match key.as_str() {
                    "server-url" => settings.server_url = opt(&value),
                    "git-repo-url" => settings.git_repo_url = opt(&value),
                    "repo-dir" => settings.repo_dir = value.into(),
                    "export-dir" => settings.export_dir = value.into(),
                    "prism-dir" => settings.launchers.prism.push(value.into()),
                    "modrinth-dir" => settings.launchers.modrinth = opt(&value).map(Into::into),
                    "curseforge-dir" => settings.launchers.curseforge = opt(&value).map(Into::into),
                    "auto-mode" => settings.auto_mode = value.parse()?,
                    other => bail!("unknown setting '{other}'"),
                }
                settings.save()?;
                println!("Saved.");
            }
            ConfigCmd::Secret { name } => {
                let secret = match name.as_str() {
                    "device-token" => Secret::DeviceToken,
                    "curseforge-api-key" => Secret::CurseForgeApiKey,
                    "git-token" => Secret::GitToken,
                    other => bail!("unknown secret '{other}'"),
                };
                eprintln!("Paste the value and press Enter (empty clears it):");
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                secret.set(line.trim())?;
                println!("Stored in the OS keychain.");
            }
        },
    }
    Ok(())
}
