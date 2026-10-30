# mcsync

Keep Minecraft modpacks, configs and worlds in sync across computers and across launchers.

- **Packs** (mods, resource/shader packs, `config/`, `options.txt`) live in a git repo as a
  launcher-neutral manifest. The manifest is a superset of Modrinth's `.mrpack` index, so any
  pack directory zipped up is a valid `.mrpack`.
- **Worlds** sync through a small self-hosted server (Docker) that stores versioned
  `tar.zst` snapshots and hands out a per-world lock, so two PCs never play the same world at once.
- **Cross-launcher**: export from one launcher, rebuild in another on a different PC.

By n1ji. MIT licensed.

## Supported launchers

| Launcher | Read | Create instance |
| --- | --- | --- |
| Prism Launcher | `instances/<name>/instance.cfg` + `mmc-pack.json` | Writes a ready instance |
| PineconeMC (Prism fork, formerly ElyPrismLauncher) | Same as Prism; data dir `PineconeMC` or `ElyPrismLauncher` | Writes a ready instance |
| Modrinth App | `profiles/<name>/` + `app.db` (read-only) | `.mrpack` to import |
| CurseForge | `Instances/<name>/minecraftinstance.json` | CurseForge modpack zip to import |

Default locations (all can be overridden in Settings):

| | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Prism / PineconeMC | `%APPDATA%\PrismLauncher`, `%APPDATA%\PineconeMC`, `%APPDATA%\ElyPrismLauncher` | `~/Library/Application Support/<same names>` | `~/.local/share/<same names>`, Flatpak data dir |
| Modrinth App | `%APPDATA%\ModrinthApp` (or `com.modrinth.theseus`) | `~/Library/Application Support/ModrinthApp` | `~/.local/share/ModrinthApp` |
| CurseForge | `~\curseforge\minecraft\Instances` | `~/Documents/curseforge/minecraft/Instances` | n/a |

Prism's `InstanceDir=` setting (relocated instance folders) is respected.

**Why Modrinth and CurseForge get import files:** the Modrinth App keeps instance metadata in a
SQLite database whose schema changes often (2026 builds split it across `instances`,
`instance_content_sets`, `instance_files` and a shared content store). mcsync only ever opens it
read-only. CurseForge's `minecraftinstance.json` carries app-generated install data that is fragile
to hand-write. Both apps import their own pack formats reliably.

## How mods are resolved

1. Each jar is hashed. Modrinth is queried by sha512 (`POST /v2/version_files`) and project
   client/server sides are recorded.
2. CurseForge: IDs the CurseForge app already recorded are used directly. With an API key, the
   fingerprint endpoint (murmur2) fills in the rest.
3. Mods whose authors block third-party downloads, and that aren't on Modrinth, are marked
   **manual** and listed in the UI. Download those yourself on each PC.

Downloads are only accepted over https from `cdn.modrinth.com`, `edge.forgecdn.net`,
`mediafilez.forgecdn.net`, `github.com`, `raw.githubusercontent.com` and `gitlab.com`, and every
file is checked against its sha1 and sha512.

Never synced through git: worlds, logs, crash reports, caches, screenshots, account/auth files
(`accounts.json`, `launcher_profiles.json`, `usercache.json`...), and any file whose name contains
"token" or "credential".

## Safety

- mcsync never deletes files from an instance. Files that a pack update replaces or removes go
  to `<game dir>/.mcsync-backup/<timestamp>/`.
- Creating an instance never overwrites an existing folder; it picks a new name.
- Replacing a world first moves the old copy to `saves/.mcsync-backups/` (last 5 kept per world).
- World conflicts are never resolved silently (see below).
- Pack and world operations refuse to run while Minecraft is running from that instance.

---

## Setup

### 1. Deploy the world server

On the home server (Docker + Compose):

```bash
git clone https://github.com/n1ji/mcsync.git
cd mcsync
docker compose up -d --build
```

The container listens on `127.0.0.1:8080` only. Data (snapshots + SQLite) lives in the
`mcsync-data` volume. Settings via environment in `docker-compose.yml`:

| Variable | Default | Meaning |
| --- | --- | --- |
| `MCSYNC_KEEP_SNAPSHOTS` | 10 | Snapshots kept per world for rollback |
| `MCSYNC_LOCK_TTL` | 90 | Seconds a lock lives without a heartbeat |
| `MCSYNC_MAX_UPLOAD_MB` | 4096 | Max world snapshot size |
| `MCSYNC_TRUST_CF_HEADER` | false | Use `CF-Connecting-IP` for auth rate limiting |

### 2. Expose it with a Cloudflare Tunnel

The server assumes nothing about the network: every API call needs a device token, request sizes
are capped, and world ids are validated so they can't act as paths. Don't port-forward it;
publish it through a tunnel:

1. In the Cloudflare dashboard: Zero Trust > Networks > Tunnels > Create a tunnel.
2. Add a public hostname, e.g. `mcsync.example.com`, pointing to `http://mcsync:8080`.
3. Put the tunnel token in a `.env` file next to `docker-compose.yml` (it's gitignored):
   `CF_TUNNEL_TOKEN=...`
4. Start with the tunnel profile:

```bash
docker compose --profile tunnel up -d
```

When the server is reachable only through Cloudflare, set `MCSYNC_TRUST_CF_HEADER: "true"` so
rate limiting sees real client IPs. You can also put a Cloudflare Access policy in front, but
note that the desktop app authenticates with its own bearer token, not Access cookies.

### 3. Pair each device

Issue one token per device on the server. The token is printed once; only its hash is stored.

```bash
docker compose exec mcsync mcsync-server issue-token --device laptop
```

```bash
docker compose exec mcsync mcsync-server issue-token --device mac
```

Revoke a lost device with `mcsync-server revoke --device <name>`, and list devices with
`mcsync-server list-devices`.

In the app on each PC, open **Settings**: enter the server URL (`https://mcsync.example.com`)
and paste that device's token, then click **Test connection**. Tokens and API keys are stored in
the OS keychain (Windows Credential Manager / macOS Keychain), never in the settings file.

### 4. Create the pack repo

Create an empty **private** repo (e.g. `n1ji/mc-packs`) and put its URL under Settings >
Pack repo. SSH URLs use your SSH agent. HTTPS URLs use git's credential helper, or a
fine-grained token entered as "Git token".

Optional: a CurseForge API key (from console.curseforge.com) improves resolution of
CurseForge-only mods.

---

## Using it

### Desktop app

- **Export to pack repo**: hash mods, resolve them, copy configs, commit.
- **Sync pack repo**: commit, pull, push.
- **Pull pack into instance**: update an instance from the repo (replaced files are backed up).
- **Move pack to launcher…**: build the pack in another launcher (Prism/PineconeMC get a ready
  instance; Modrinth App/CurseForge get an import file, revealed in your file manager).
- **Worlds**: per-world status (in sync / local changes / server newer / conflict) with Pull and Push.
- **Play (auto sync)**: locks and pulls every world of the instance, waits for you to start the
  game from your launcher, keeps the locks alive while the java process runs, then pushes and
  unlocks when it exits.
- **Auto mode** (Settings): also catches games started straight from a launcher for linked
  instances, locks their worlds and pushes on exit.

### Conflicts

Each local world remembers the server version it came from. If the server moved on *and* your
copy changed, nothing is overwritten. You choose:

- **Keep server copy**: your local copy is moved to `saves/.mcsync-backups/`.
- **Keep local copy**: the server copy is downloaded to `saves/.mcsync-backups/`, then yours is
  uploaded. The server also keeps the old version in its history.
- **Cancel**.

### CLI

```text
mcsync detect                                   list launchers + instances
mcsync export --launcher pinecone --instance "Valhelsia 6" --commit
mcsync import --launcher prism --pack valhelsia-6
mcsync apply  --launcher modrinth --instance create --pack create
mcsync mrpack --pack valhelsia-6 -o valhelsia.mrpack
mcsync repo pull | push | status
mcsync worlds
mcsync pull-world --launcher prism --instance "Valhelsia 6" --world "New World" --lock
mcsync push-world --launcher prism --instance "Valhelsia 6" --world "New World" [--resolve keep-local|keep-server]
mcsync config set server-url https://mcsync.example.com
mcsync config secret device-token               reads the token from stdin
```

`--launcher` accepts `prism`, `pinecone`, `modrinth`, `curseforge`; add `--data-dir` to point at a
specific install (e.g. a copy of an instance folder for testing).

---

## Development

```text
core/     launcher adapters, manifest, resolution, git, world archives, server client
cli/      `mcsync` command line
server/   axum world server, Dockerfile
app/      Tauri v2 app (src-tauri/ backend, src/ TypeScript frontend)
```

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cd app && npm install && npx tauri dev
cd app && npx tauri build
```

Adapter tests run against fixture instance folders in `core/tests/fixtures/`, copied to a temp dir
per test. A real-file check of the CurseForge fingerprint:

```bash
MCSYNC_CF_JAR=path/to/mod.jar MCSYNC_CF_FINGERPRINT=<packageFingerprint from minecraftinstance.json> cargo test -p mcsync-core -- --ignored
```

### Server API

All `/v1` routes need `Authorization: Bearer <device token>`.

| Method | Path | |
| --- | --- | --- |
| GET | `/health` | Liveness (no auth) |
| GET | `/v1/whoami` | Device name |
| GET | `/v1/worlds` | All worlds |
| GET | `/v1/worlds/{id}` | Version, history, lock |
| POST | `/v1/worlds/{id}/lock` | Acquire or refresh (409 if held by another device) |
| POST | `/v1/worlds/{id}/heartbeat` | Same as lock |
| DELETE | `/v1/worlds/{id}/lock` | Release |
| PUT | `/v1/worlds/{id}/snapshot?base_version=N[&force=true]` | Upload `tar.zst` (lock holder only; 409 `conflict` if N is stale) |
| GET | `/v1/worlds/{id}/snapshot[?version=N]` | Download (`x-world-version`, `x-sha256` headers) |
