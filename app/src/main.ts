import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------- types (mirror the Rust command views) ----------

interface InstanceView {
  key: string;
  launcher: string;
  launcher_label: string;
  name: string;
  mc_version: string | null;
  loader: string;
  loader_version: string | null;
  game_dir: string;
  pack: string | null;
  pack_version: string | null;
}

interface LauncherView {
  label: string;
  kind: string;
  data_dir: string;
  error: string | null;
}

interface Overview {
  launchers: LauncherView[];
  instances: InstanceView[];
  repo_dirty: boolean | null;
}

type WorldStatus = "synced" | "ahead" | "behind" | "conflict" | "local_only" | "server_only" | "missing";

interface WorldView {
  name: string;
  id: string;
  local: boolean;
  base_version: number | null;
  local_changed: boolean;
  server_version: number | null;
  lock_holder: string | null;
  status: WorldStatus;
}

interface Detail {
  pack: string;
  pack_exists: boolean;
  manual: string[];
  running: boolean;
  worlds: WorldView[];
  server_error: string | null;
}

type Outcome =
  | { type: "up_to_date"; version: number; local_changed: boolean }
  | { type: "pulled"; version: number; backup: string | null }
  | { type: "pushed"; version: number; server_backup: string | null }
  | { type: "kept_local"; server_version: number; server_backup: string }
  | { type: "nothing_on_server" }
  | { type: "no_local_world" }
  | { type: "conflict"; server_version: number; base_version: number | null };

type Resolution = "keep_server" | "keep_local";

interface SessionConflict {
  world: string;
  server_version: number;
  base_version: number | null;
  stage: "pull" | "push";
}

interface SessionView {
  key: string;
  name: string;
  phase: "pulling" | "waiting_for_game" | "playing" | "pushing" | "needs_attention" | "done" | "failed";
  worlds: string[];
  conflicts: SessionConflict[];
  message: string | null;
  auto_detected: boolean;
}

interface Settings {
  server_url: string | null;
  git_repo_url: string | null;
  repo_dir: string;
  export_dir: string;
  launchers: { prism: string[]; modrinth: string | null; curseforge: string | null };
  auto_mode: boolean;
}

interface SettingsView {
  settings: Settings;
  device_token_set: boolean;
  curseforge_key_set: boolean;
  git_token_set: boolean;
  settings_path: string;
}

// ---------- tiny DOM helpers (textContent only: names are untrusted) ----------

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: { class?: string; text?: string; title?: string } = {},
  ...children: (Node | null | false)[]
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (props.class) e.className = props.class;
  if (props.text !== undefined) e.textContent = props.text;
  if (props.title) e.title = props.title;
  for (const c of children) if (c) e.append(c);
  return e;
}

function button(text: string, onClick: () => void, cls = "ghost"): HTMLButtonElement {
  const b = el("button", { class: cls, text });
  b.addEventListener("click", onClick);
  return b;
}

let toastTimer: number | undefined;
function toast(msg: string, kind: "ok" | "error" = "ok") {
  const t = $("#toast");
  t.textContent = msg;
  t.className = `toast ${kind}`;
  t.hidden = false;
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (t.hidden = true), kind === "error" ? 9000 : 4500);
}

async function run<T>(label: string, f: () => Promise<T>): Promise<T | undefined> {
  document.body.classList.add("busy");
  try {
    return await f();
  } catch (e) {
    toast(`${label} failed: ${e}`, "error");
    return undefined;
  } finally {
    document.body.classList.remove("busy");
  }
}

// ---------- state ----------

let overview: Overview = { launchers: [], instances: [], repo_dirty: null };
let selected: string | null = null;
let sessions: SessionView[] = [];

// ---------- instance list ----------

async function refresh() {
  const o = await run("Loading instances", () => invoke<Overview>("get_overview"));
  if (!o) return;
  overview = o;
  renderList();
  if (selected) await showDetail(selected);
}

function renderList() {
  const list = $("#instance-list");
  const q = ($("#filter") as HTMLInputElement).value.toLowerCase();
  list.replaceChildren();
  if (overview.launchers.length === 0) {
    list.append(el("p", { class: "muted", text: "No launchers detected. Add paths in Settings." }));
  }
  for (const l of overview.launchers) {
    const items = overview.instances.filter(
      (i) => i.launcher_label === l.label && i.name.toLowerCase().includes(q),
    );
    const group = el("div", { class: "group" }, el("h3", { text: l.label, title: l.data_dir }));
    if (l.error) group.append(el("p", { class: "error-text", text: l.error }));
    for (const i of items) {
      const row = el(
        "div",
        { class: `instance${i.key === selected ? " selected" : ""}` },
        el("div", { class: "name", text: i.name }),
        el("div", {
          class: "meta",
          text: `${i.mc_version ?? "?"} · ${i.loader}${i.loader_version ? " " + i.loader_version : ""}`,
        }),
        i.pack ? el("span", { class: "badge", text: `pack v${i.pack_version ?? "?"}` }) : null,
      );
      row.addEventListener("click", () => {
        selected = i.key;
        renderList();
        showDetail(i.key);
      });
      group.append(row);
    }
    list.append(group);
  }
}

// ---------- detail ----------

const STATUS_TEXT: Record<WorldStatus, string> = {
  synced: "In sync",
  ahead: "Local changes",
  behind: "Server newer",
  conflict: "Conflict",
  local_only: "Not on server",
  server_only: "Only on server",
  missing: "Missing",
};

async function showDetail(key: string) {
  const inst = overview.instances.find((i) => i.key === key);
  if (!inst) return;
  const box = $("#detail");
  box.className = "detail";
  box.replaceChildren(el("p", { class: "muted", text: "Loading…" }));
  const d = await run("Loading instance", () => invoke<Detail>("instance_detail", { key }));
  if (!d || selected !== key) return;

  const actions = el(
    "div",
    { class: "actions" },
    button("Play (auto sync)", () => startPlay(key), "primary"),
    button("Export to pack repo", () => exportPack(key)),
    d.pack_exists ? button("Pull pack into instance", () => applyPack(key)) : null,
    d.pack_exists ? button("Move pack to launcher…", () => movePack(inst)) : null,
  );

  const header = el(
    "div",
    { class: "detail-header" },
    el("h2", { text: inst.name }),
    el("p", {
      class: "muted",
      text: `${inst.launcher_label} · Minecraft ${inst.mc_version ?? "?"} · ${inst.loader} ${inst.loader_version ?? ""}`,
    }),
    el("p", { class: "muted small", text: inst.game_dir }),
    el("p", {
      class: "small",
      text: d.pack_exists ? `Pack: ${d.pack}` : `Not exported yet (pack name will be "${d.pack}")`,
    }),
    d.running ? el("p", { class: "warn-text", text: "Minecraft is running from this instance." }) : null,
  );

  const worlds = el("table", { class: "worlds" });
  worlds.append(
    el(
      "thead",
      {},
      el("tr", {}, ...["World", "Status", "Local base", "Server", "Lock", ""].map((h) => el("th", { text: h }))),
    ),
  );
  const tbody = el("tbody");
  for (const w of d.worlds) {
    tbody.append(
      el(
        "tr",
        {},
        el("td", { text: w.name, title: w.id }),
        el("td", {}, el("span", { class: `status ${w.status}`, text: STATUS_TEXT[w.status] })),
        el("td", { text: w.base_version ? `v${w.base_version}` : "-" }),
        el("td", { text: w.server_version ? `v${w.server_version}` : "-" }),
        el("td", { text: w.lock_holder ?? "" }),
        el(
          "td",
          { class: "row-actions" },
          button("Pull", () => worldOp("pull_world", key, w.name)),
          w.local ? button("Push", () => worldOp("push_world", key, w.name)) : null,
        ),
      ),
    );
  }
  if (d.worlds.length === 0) {
    tbody.append(el("tr", {}, el("td", { class: "muted", text: "No worlds yet." })));
  }
  worlds.append(tbody);

  const parts: (Node | null)[] = [
    header,
    actions,
    d.server_error ? el("p", { class: "error-text", text: `Server: ${d.server_error}` }) : null,
    el("h3", { text: "Worlds" }),
    worlds,
    d.manual.length
      ? el(
          "div",
          { class: "manual" },
          el("h3", { text: "Needs manual download" }),
          el("p", {
            class: "muted small",
            text: "These mods block third-party downloads and aren't on Modrinth. Download them yourself into mods/ on each PC.",
          }),
          el("ul", {}, ...d.manual.map((m) => el("li", { text: m }))),
        )
      : null,
  ];
  box.replaceChildren(...parts.filter((p): p is Node => p !== null));
}

// ---------- actions ----------

async function exportPack(key: string) {
  const r = await run("Export", () =>
    invoke<{ files: number; overrides: number; manual: string[]; skipped: string[] }>("export_pack", { key }),
  );
  if (!r) return;
  toast(
    `Exported ${r.files} files, ${r.overrides} overrides` +
      (r.manual.length ? `; ${r.manual.length} need manual download` : "") +
      ". Use “Sync pack repo” to push.",
  );
  await refresh();
}

async function applyPack(key: string) {
  const r = await run("Pull pack", () =>
    invoke<{ downloaded: string[]; overrides_written: number; backed_up: string[]; manual_missing: string[] }>(
      "apply_pack",
      { key },
    ),
  );
  if (!r) return;
  toast(
    `Downloaded ${r.downloaded.length}, wrote ${r.overrides_written} overrides` +
      (r.backed_up.length ? `, backed up ${r.backed_up.length} replaced files` : "") +
      (r.manual_missing.length ? `. ${r.manual_missing.length} manual mods missing.` : "."),
  );
}

async function syncRepo() {
  const r = await run("Repo sync", () => invoke<{ pull: { type: string } | null; pushed: boolean; remote: boolean }>("sync_repo"));
  if (!r) return;
  toast(r.remote ? `Pack repo synced (${r.pull?.type ?? "ok"}).` : "Committed locally. Set a repo URL to push.");
  await refresh();
}

function describe(o: Outcome): string {
  switch (o.type) {
    case "up_to_date":
      return o.local_changed ? `Server is at v${o.version}; you have local changes to push.` : `Up to date (v${o.version}).`;
    case "pulled":
      return `Pulled v${o.version}.` + (o.backup ? " Previous local copy backed up." : "");
    case "pushed":
      return `Pushed as v${o.version}.` + (o.server_backup ? " Server copy backed up locally." : "");
    case "kept_local":
      return `Kept local copy; server v${o.server_version} saved to backups. Push to upload yours.`;
    case "nothing_on_server":
      return "Nothing on the server yet.";
    case "no_local_world":
      return "No local world to push.";
    case "conflict":
      return "Conflict.";
  }
}

async function worldOp(cmd: "pull_world" | "push_world", key: string, world: string, resolution?: Resolution) {
  const o = await run(cmd === "pull_world" ? "Pull" : "Push", () =>
    invoke<Outcome>(cmd, { key, world, resolution: resolution ?? null }),
  );
  if (!o) return;
  if (o.type === "conflict") {
    const choice = await askConflict(world, o.server_version, o.base_version);
    if (choice) await worldOp(cmd, key, world, choice);
    return;
  }
  toast(`${world}: ${describe(o)}`);
  if (selected === key) await showDetail(key);
}

function askConflict(world: string, server: number, base: number | null): Promise<Resolution | null> {
  const dlg = $("#dlg-conflict") as HTMLDialogElement;
  $("#conflict-text").textContent =
    `"${world}" changed on another device (server is at v${server}) and also changed here ` +
    `(based on ${base ? "v" + base : "nothing synced yet"}). Nothing has been overwritten.`;
  return new Promise((resolve) => {
    const onClick = (ev: Event) => {
      const choice = (ev.currentTarget as HTMLElement).dataset.choice;
      dlg.querySelectorAll("button").forEach((b) => b.removeEventListener("click", onClick));
      dlg.close();
      resolve(choice === "keep_server" || choice === "keep_local" ? choice : null);
    };
    dlg.querySelectorAll("button").forEach((b) => b.addEventListener("click", onClick));
    dlg.showModal();
  });
}

async function movePack(inst: InstanceView) {
  const dlg = $("#dlg-move") as HTMLDialogElement;
  const form = $("#move-form") as HTMLFormElement;
  const select = form.elements.namedItem("target") as HTMLSelectElement;
  select.replaceChildren(
    ...overview.launchers.filter((l) => l.label !== inst.launcher_label).map((l) => {
      const o = el("option", { text: `${l.label} (${l.kind})` });
      o.value = l.label;
      return o;
    }),
  );
  (form.elements.namedItem("name") as HTMLInputElement).value = inst.name;
  dlg.returnValue = "";
  dlg.showModal();
  await new Promise((r) => dlg.addEventListener("close", r, { once: true }));
  if (dlg.returnValue !== "go" || !select.value) return;
  const name = (form.elements.namedItem("name") as HTMLInputElement).value;
  const r = await run("Move pack", () =>
    invoke<{ outcome: { type: "created" | "import_file"; path?: string; name?: string }; manual: string[] }>(
      "move_pack",
      { key: inst.key, targetLabel: select.value, name },
    ),
  );
  if (!r) return;
  if (r.outcome.type === "import_file" && r.outcome.path) {
    toast(`Import file ready. Import it in ${select.value}.`);
    await run("Reveal", () => invoke("reveal_path", { path: r.outcome.path }));
  } else {
    toast(`Created instance in ${select.value}.` + (r.manual.length ? ` ${r.manual.length} mods need manual download.` : ""));
  }
  await refresh();
}

async function startPlay(key: string) {
  await run("Play", () => invoke("start_play", { key }));
  sessions = await invoke<SessionView[]>("get_sessions");
  renderSessions();
}

// ---------- sessions ----------

const PHASE_TEXT: Record<SessionView["phase"], string> = {
  pulling: "Pulling worlds and taking locks…",
  waiting_for_game: "Ready. Launch the game from your launcher.",
  playing: "Playing. Worlds are locked.",
  pushing: "Game closed. Pushing worlds…",
  needs_attention: "Needs your decision",
  done: "Done",
  failed: "Failed",
};

function renderSessions() {
  const box = $("#sessions");
  box.replaceChildren(
    ...sessions.map((s) => {
      const card = el(
        "div",
        { class: `session ${s.phase}` },
        el("div", { class: "session-title", text: `${s.name}: ${PHASE_TEXT[s.phase]}` }),
        s.message ? el("div", { class: "small", text: s.message }) : null,
      );
      for (const c of s.conflicts) {
        card.append(
          el(
            "div",
            { class: "session-conflict" },
            el("span", { text: `${c.world}: server v${c.server_version}, yours based on ${c.base_version ? "v" + c.base_version : "nothing"}` }),
            button("Resolve…", async () => {
              const choice = await askConflict(c.world, c.server_version, c.base_version);
              if (choice) await run("Resolve", () => invoke("resolve_session_conflict", { key: s.key, world: c.world, resolution: choice }));
            }),
          ),
        );
      }
      if (["waiting_for_game", "needs_attention", "pulling"].includes(s.phase)) {
        card.append(button("Cancel and unlock", () => run("Cancel", () => invoke("stop_session", { key: s.key }))));
      }
      if (["done", "failed"].includes(s.phase)) {
        card.append(
          button("Dismiss", () => {
            sessions = sessions.filter((x) => x !== s);
            renderSessions();
          }),
        );
      }
      return card;
    }),
  );
}

// ---------- settings ----------

async function openSettings() {
  const v = await run("Loading settings", () => invoke<SettingsView>("get_settings"));
  if (!v) return;
  const f = $("#settings-form") as HTMLFormElement;
  const set = (name: string, value: string) => ((f.elements.namedItem(name) as HTMLInputElement).value = value);
  set("server_url", v.settings.server_url ?? "");
  set("git_repo_url", v.settings.git_repo_url ?? "");
  set("repo_dir", v.settings.repo_dir);
  set("export_dir", v.settings.export_dir);
  set("modrinth_dir", v.settings.launchers.modrinth ?? "");
  set("curseforge_dir", v.settings.launchers.curseforge ?? "");
  (f.elements.namedItem("prism_dirs") as HTMLTextAreaElement).value = v.settings.launchers.prism.join("\n");
  (f.elements.namedItem("auto_mode") as HTMLInputElement).checked = v.settings.auto_mode;
  for (const name of ["device-token", "git-token", "curseforge-api-key"]) set(name, "");
  const states: Record<string, boolean> = {
    "device-token": v.device_token_set,
    "git-token": v.git_token_set,
    "curseforge-api-key": v.curseforge_key_set,
  };
  f.querySelectorAll<HTMLElement>(".secret-state").forEach((s) => {
    const on = states[s.dataset.secret!];
    s.textContent = on ? "(set)" : "(not set)";
    s.className = `secret-state ${on ? "on" : "off"}`;
  });
  const dlg = $("#dlg-settings") as HTMLDialogElement;
  dlg.returnValue = "";
  dlg.showModal();
  await new Promise((r) => dlg.addEventListener("close", r, { once: true }));
  if (dlg.returnValue !== "save") return;

  const get = (name: string) => (f.elements.namedItem(name) as HTMLInputElement).value.trim();
  const opt = (s: string) => (s === "" ? null : s);
  const next: Settings = {
    server_url: opt(get("server_url")),
    git_repo_url: opt(get("git_repo_url")),
    repo_dir: get("repo_dir"),
    export_dir: get("export_dir"),
    launchers: {
      prism: (f.elements.namedItem("prism_dirs") as HTMLTextAreaElement).value
        .split("\n")
        .map((s) => s.trim())
        .filter(Boolean),
      modrinth: opt(get("modrinth_dir")),
      curseforge: opt(get("curseforge_dir")),
    },
    auto_mode: (f.elements.namedItem("auto_mode") as HTMLInputElement).checked,
  };
  const ok = await run("Save settings", async () => {
    await invoke("save_settings", { new: next });
    for (const name of ["device-token", "git-token", "curseforge-api-key"]) {
      const value = get(name);
      if (value) await invoke("set_secret", { name, value });
    }
    return true;
  });
  if (ok) {
    toast("Settings saved.");
    await refresh();
  }
}

async function testConnection() {
  // Save a newly typed URL/token first so the test uses them.
  const f = $("#settings-form") as HTMLFormElement;
  const url = (f.elements.namedItem("server_url") as HTMLInputElement).value.trim();
  const token = (f.elements.namedItem("device-token") as HTMLInputElement).value.trim();
  const msg = await run("Connection test", async () => {
    if (url) {
      const v = await invoke<SettingsView>("get_settings");
      await invoke("save_settings", { new: { ...v.settings, server_url: url } });
    }
    if (token) await invoke("set_secret", { name: "device-token", value: token });
    return invoke<string>("test_connection");
  });
  if (msg) toast(msg);
}

// ---------- boot ----------

window.addEventListener("DOMContentLoaded", async () => {
  $("#btn-refresh").addEventListener("click", refresh);
  $("#btn-sync-repo").addEventListener("click", syncRepo);
  $("#btn-settings").addEventListener("click", openSettings);
  $("#btn-test").addEventListener("click", testConnection);
  $("#filter").addEventListener("input", renderList);
  await listen<SessionView[]>("sessions-changed", (e) => {
    sessions = e.payload;
    renderSessions();
    if (selected) showDetail(selected);
  });
  sessions = await invoke<SessionView[]>("get_sessions");
  renderSessions();
  await refresh();
});
