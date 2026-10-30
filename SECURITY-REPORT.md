# Security report: mcsync

- **Date:** 2026-10-30
- **Version / commit:** 0.1.0, pre-release (first public release)
- **Verdict:** **SHIP** after fixes (policy: block on Medium and above). The initial scan was **DON'T SHIP** (1 High, 5 Medium); all of those are fixed in this pass and re-verified.

## Summary
Checked the Rust workspace (core, CLI, axum server, Tauri app) and the TypeScript frontend: dependency advisories (cargo audit, npm audit), secrets in the working tree and git history, a manual review of every trust boundary, and the Docker/Compose and Tauri configs.
The most important issue was a reachable memory-safety advisory in `zstd-safe`, which decodes world archives downloaded from the server. It was fixed by upgrading zstd. The remaining items are Low/Info.

| Severity | Found | Open after fixes |
|---|---|---|
| Critical | 0 | 0 |
| High | 1 | 0 |
| Medium | 5 | 0 |
| Low / Info | 12 | 9 |

## Findings (Medium and above first)

### [High] Reachable zstd advisories in world archive decoding (fixed)
- **Where:** `zstd@0.13.3`, `zstd-safe@7.3.0` (used by `core/src/world.rs` `unpack_checked`, `pack_world`)
- **Advisory:** RUSTSEC-2026-0403 (uninitialized memory exposed when streaming decompression of a legacy frame fails), RUSTSEC-2026-0398, RUSTSEC-2026-0391, plus 11 "unsound" warnings on zstd-safe 7.3.0
- **Impact in this app:** the client streams decompression of `tar.zst` snapshots downloaded from the sync server. A malicious or compromised server, or anyone holding a device token, could upload a crafted frame that reaches the vulnerable decoder path. Reachable.
- **Fix (applied):** `zstd = "0.14"` in the workspace `Cargo.toml`, giving zstd 0.14.x / zstd-safe 8.2.0. cargo audit now reports no zstd advisories.

### [Medium] Vulnerable `zip` crate (fixed)
- **Where:** `zip@2.6.1` (`core/Cargo.toml`)
- **Advisory:** RUSTSEC-2026-0367, RUSTSEC-2026-0368, RUSTSEC-2026-0369 (extraction: SUID bits, symlinks escaping the destination, permissions applied through symlinks)
- **Impact in this app:** mcsync only *writes* zips (`.mrpack`, CurseForge import zips) and never calls `ZipArchive::extract`, so the vulnerable path is not reachable today. Bumped so a future import feature can't inherit it.
- **Fix (applied):** `zip = { version = "9", ... }` in `core/Cargo.toml`.

### [Medium] World/instance names with a Windows drive prefix escape their base directory (fixed)
- **Where:** `core/src/world.rs` `install_world`, `core/src/pack.rs` `pack_worlds`, `core/src/sync.rs`, `read_instance` in all three launcher adapters
- **Impact in this app:** names were checked only for `/`, `\` and `..`. On Windows, `saves.join("C:evil")` resolves to `C:evil` (verified), outside `saves/`. World names come from `worlds.json` in the pack repo (written by other devices) and from Tauri IPC, so a bad repo entry could make a pull stage, back up or rename a path outside the instance.
- **Fix (applied):** new `launcher::is_safe_component` rejects separators, `:`, NUL, `.`/`..` and Windows device names (CON, NUL, COM1...). It is enforced in `install_world`, `pack_worlds`/`add_pack_world`, `pull_world`/`push_world` and every `read_instance`. Regression test: `world::tests::rejects_folder_names_that_escape_saves`.

### [Medium] Symlinked `overrides/` in the pack repo was followed (fixed)
- **Where:** `core/src/pack.rs` `apply_pack`, `copy_tree`; `core/src/manifest.rs` `write_mrpack`; `core/src/launcher/curseforge.rs` `write_instance`
- **Impact in this app:** `walkdir` follows a symlinked root by default (`follow_root_links: true`, verified in walkdir 2.5.0). A pack repo commit that replaced `overrides/` with a symlink could make "Pull pack" copy in-scope files (e.g. a `config/` folder) from elsewhere on disk into the instance, and a later export would push them to the repo.
- **Fix (applied):** a `walk_repo_dir` helper refuses a symlinked root and never follows links. The pack repo is cloned with `core.symlinks=false`, so committed symlinks check out as plain files.

### [Medium] Unbounded network responses (fixed)
- **Where:** `core/src/pack.rs` `fetch` (whole mod download buffered, no cap), `core/src/resolve.rs` (Modrinth/CurseForge JSON with no cap or timeout), `core/src/client.rs` (server JSON, error bodies and snapshot downloads uncapped)
- **Impact in this app:** a misbehaving CDN, API or server could exhaust memory or disk, or hang the app.
- **Fix (applied):** `resolve::read_limited`/`read_json` cap bodies (32 MiB for API JSON, 64 KiB for error bodies). Mod downloads are capped at the manifest's `fileSize` (512 MiB if unknown) with a 10-minute timeout. API calls time out after 60 s. Snapshot downloads must not exceed their declared `Content-Length` or 16 GiB.

### [Medium] Redirects not restricted to the download allow-list (fixed, downgraded to Low on review)
- **Where:** `core/src/resolve.rs` `http_client`
- **Impact in this app:** download URLs were allow-listed, but reqwest followed redirects anywhere. Content is hash-verified, so integrity was never at risk, but a redirect could point the client at internal hosts. Low in practice.
- **Fix (applied):** a custom redirect policy: at most 5 hops, https only, same host or an allow-listed download host.

## Low / Info
- *(fixed)* Auth rate limiter could lock out valid devices sharing an IP (everyone behind cloudflared). Valid tokens now always pass, and the limiter only throttles bad tokens (`server/src/auth.rs`, test in `server/tests/api.rs`).
- *(fixed)* A git token could be offered to a non-https remote; it is now only sent for `https://` URLs (`core/src/git.rs`).
- *(fixed)* No cap on decompressed world size; now 64 GiB / 2M entries (`core/src/world.rs`).
- *(fixed)* Unused Tauri permissions (`core:app:default`, `core:window:default`) removed; only `core:event:default` remains.
- `git2@0.20.4`: RUSTSEC-2026-0183 (`Remote::list`) and RUSTSEC-2026-0184 (`BlameHunk` signatures) are unsound APIs that mcsync never calls (grep confirmed). No newer git2 release yet.
- `glib@0.18.5` RUSTSEC-2024-0429: Linux GTK dependency of Tauri, unused API.
- Unmaintained transitive crates: `proc-macro-error` (build-time), `zbus_names`, `zvariant`, `zvariant_derive` (Linux D-Bus via keyring/Tauri). Watch for upstream replacements.
- Device tokens are stored as SHA-256. They are 256-bit random values, not passwords, so a slow hash isn't needed.
- Docker base images are pinned by tag (`rust:1.99-slim-bookworm`, `distroless/cc-debian12:nonroot`, `cloudflared:2026.10.6`), not by digest. Consider pinning digests for reproducible builds.
- Compose has no CPU/memory limits; add `deploy.resources.limits` if the home server is shared.
- The CLI reads `config secret` values from stdin with echo on; the desktop app uses a password field.
- The secret scanner flags `app/index.html:42,55`: these are `name="device-token"` / `name="curseforge-api-key"` input attributes, not values (false positive, verified).
- Security-relevant positives: strict CSP (no inline/eval, `connect-src` IPC only), `withGlobalTauri: false`, no fs/shell/opener permissions for the webview, secrets in the OS keychain and never returned to the UI, the frontend uses `textContent` only, all server SQL is parameterized, the Modrinth DB is opened read-only, tar entries are restricted to files/dirs and confined with `unpack_in`, the container runs non-root with a read-only rootfs, `cap_drop: ALL`, and a localhost-only port.

## Coverage
| Check | Tool / source | Ran? | Notes |
|---|---|---|---|
| Rust dependencies | cargo audit 0.22 | yes | Advisory DB updated 2026-10-30 (1395 advisories), 580 crates. Before: 6 vulnerabilities; after: 0 (7 allowed warnings) |
| Python dependencies | pip-audit | n.a. | No Python |
| Node dependencies | npm audit --package-lock-only | yes | 0 vulnerabilities (app/package-lock.json) |
| Secrets (working tree + history) | scan_secrets.py --history | yes | 2 false positives (form field names), nothing in history |
| Own code review | manual | yes | Server routes/auth/db, client, resolver, pack export/apply/install, world archive pack/unpack, git, launcher adapters, Tauri commands, frontend rendering |
| Docker / config | manual | yes | Dockerfile, docker-compose.yml, .dockerignore |
| Web app | manual | yes | Server API: auth, limits, headers (no cookies/CORS; JSON-only API) |
| Desktop (Tauri) | manual | yes | CSP, capabilities, all 17 commands, reveal_path scoping |
| CI / release | manual | n.a. | No CI workflows or updater configured yet |

## Not checked
- The Docker image was not built or scanned with trivy/grype: Docker isn't available on the build machine. Base images are current, pinned tags.
- The server was not probed behind a real Cloudflare Tunnel; tests ran against a local instance.
- macOS build: only the Windows Tauri bundle was built and smoke-tested.
