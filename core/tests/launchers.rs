//! Launcher adapter tests against fixture instance folders. Fixtures are
//! copied to a temp dir first so tests never modify the checked-in copies.

use mcsync_core::launcher::curseforge::CurseForge;
use mcsync_core::launcher::modrinth::ModrinthApp;
use mcsync_core::launcher::prism::PrismLauncher;
use mcsync_core::launcher::{Launcher, LauncherKind, WriteOutcome};
use mcsync_core::manifest::{LoaderKind, Manifest};
use mcsync_core::pack;
use mcsync_core::resolve::Resolver;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join(name);
    for e in walkdir::WalkDir::new(&src) {
        let e = e.unwrap();
        let target = dst.join(e.path().strip_prefix(&src).unwrap());
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else if e.file_name() != ".gitkeep" {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
    (tmp, dst)
}

/// Minimal stand-in for the Modrinth API that knows no files: `{}` for
/// POST /v2/version_files, `[]` for GET /v2/projects.
fn mock_modrinth() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let reply = if request_line.starts_with("POST") { "{}" } else { "[]" };
            let mut s = stream;
            write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        }
    });
    format!("http://{addr}")
}

fn make_modrinth_db(dir: &Path) {
    let c = rusqlite::Connection::open(dir.join("app.db")).unwrap();
    c.execute_batch(
        "CREATE TABLE instances (id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
            applied_content_set_id TEXT, install_stage TEXT, name TEXT NOT NULL);
         CREATE TABLE instance_content_sets (id TEXT PRIMARY KEY, instance_id TEXT,
            name TEXT, game_version TEXT NOT NULL, loader TEXT NOT NULL, loader_version TEXT);
         INSERT INTO instances VALUES ('legacy:create','create','legacy:create:default','installed','Create Mod');
         INSERT INTO instance_content_sets VALUES
            ('legacy:create:default','legacy:create','Default','1.20.1','fabric','0.18.4');",
    )
    .unwrap();
}

#[test]
fn prism_lists_and_reads_instances() {
    let (_t, dir) = fixture("prism");
    let l = PrismLauncher::open(&dir).unwrap();
    assert_eq!(l.label(), "Prism Launcher");
    let list = l.list_instances().unwrap();
    // _MMC_TEMP (hidden) and NoCfg (no instance.cfg) are skipped.
    assert_eq!(list.len(), 1);
    let i = &list[0];
    assert_eq!(i.name, "Fabric Pack");
    assert_eq!(i.mc_version.as_deref(), Some("1.20.1"));
    assert_eq!(i.loader.kind, LoaderKind::Fabric);
    assert_eq!(i.loader.version.as_deref(), Some("0.15.11"));
    assert!(i.game_dir.ends_with("minecraft"));
    assert_eq!(l.read_instance("Fabric Pack").unwrap(), *i);
    assert!(l.read_instance("../prism").is_err());
}

#[test]
fn pinecone_uses_relocated_instance_dir_and_dot_minecraft() {
    let (_t, dir) = fixture("pinecone");
    let renamed = dir.with_file_name("PineconeMC");
    std::fs::rename(&dir, &renamed).unwrap();
    let l = PrismLauncher::open(&renamed).unwrap();
    assert_eq!(l.label(), "PineconeMC");
    let list = l.list_instances().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].loader.kind, LoaderKind::Forge);
    assert!(list[0].game_dir.ends_with(".minecraft"));
    assert!(list[0].root.starts_with(renamed.join("custom-instances")));
}

#[test]
fn modrinth_reads_db_and_falls_back_to_folders() {
    let (_t, dir) = fixture("modrinth");
    let l = ModrinthApp::open(&dir).unwrap();
    // Without a DB: only folders that look like instances.
    let names: Vec<_> = l.list_instances().unwrap().into_iter().map(|i| i.id).collect();
    assert_eq!(names, vec!["create", "no-db-pack"]);

    make_modrinth_db(&dir);
    let i = l.read_instance("create").unwrap();
    assert_eq!(i.name, "Create Mod");
    assert_eq!(i.mc_version.as_deref(), Some("1.20.1"));
    assert_eq!(i.loader.kind, LoaderKind::Fabric);
    assert_eq!(i.game_dir, dir.join("profiles/create"));
}

#[test]
fn curseforge_reads_instance_and_hints() {
    let (_t, dir) = fixture("curseforge");
    let l = CurseForge::open(&dir.join("Instances")).unwrap();
    let i = l.read_instance("Eternal Lite").unwrap();
    assert_eq!(i.mc_version.as_deref(), Some("1.19.2"));
    assert_eq!(i.loader.kind, LoaderKind::Forge);
    assert_eq!(i.loader.version.as_deref(), Some("43.3.5"));
    let hint = &i.hints[0];
    assert_eq!(hint.murmur2, Some(3675948766));
    let cf = hint.curseforge.as_ref().unwrap();
    assert_eq!((cf.project_id, cf.file_id, cf.distributable), (414401, 4556106, false));
}

#[tokio::test]
async fn export_then_build_in_every_launcher() {
    let (_t, prism_dir) = fixture("prism");
    let prism = PrismLauncher::open(&prism_dir).unwrap();
    let inst = prism.read_instance("Fabric Pack").unwrap();
    let work = tempfile::tempdir().unwrap();
    let pack_dir = work.path().join("packs/fabric-pack");

    let resolver = Resolver::with_endpoints(&mock_modrinth(), "http://127.0.0.1:9", None);
    let report = pack::export_instance(&inst, &pack_dir, &resolver).await.unwrap();
    assert_eq!(report.manual, vec!["mods/examplemod-1.0.jar"]);

    let m = Manifest::load(&pack_dir).unwrap();
    assert_eq!(m.mc_version(), Some("1.20.1"));
    assert_eq!(m.loader().kind, LoaderKind::Fabric);
    assert_eq!(m.files.len(), 1, "disabled mods are not exported");
    let ov = pack_dir.join("overrides");
    assert!(ov.join("config/sodium/options.json").is_file());
    assert!(ov.join("options.txt").is_file());
    assert!(ov.join("resourcepacks/MyTextures.zip").is_file(), "unresolved resource pack ships as override");
    assert!(!ov.join("logs").exists());
    assert!(!ov.join("saves").exists());

    // Re-export without changes keeps the version; a change bumps it.
    pack::export_instance(&inst, &pack_dir, &resolver).await.unwrap();
    assert_eq!(Manifest::load(&pack_dir).unwrap().version_id, "1");
    std::fs::write(inst.game_dir.join("mods/new.jar"), b"new").unwrap();
    pack::export_instance(&inst, &pack_dir, &resolver).await.unwrap();
    assert_eq!(Manifest::load(&pack_dir).unwrap().version_id, "2");

    let http = mcsync_core::resolve::http_client();
    let exports = work.path().join("exports");

    // Prism-format target (e.g. PineconeMC on the other PC).
    let (_t2, target_dir) = fixture("pinecone");
    let target = PrismLauncher::open(&target_dir).unwrap();
    let r = pack::install_pack(&pack_dir, &target, "Fabric Pack", &exports, &http).await.unwrap();
    let WriteOutcome::Created(new) = &r.outcome else { panic!("expected instance") };
    assert_eq!(new.launcher, LauncherKind::Prism);
    assert_eq!(new.loader.kind, LoaderKind::Fabric);
    assert!(new.game_dir.join("config/sodium/options.json").is_file());
    assert!(new.game_dir.join("resourcepacks/MyTextures.zip").is_file());
    let apply = r.apply.unwrap();
    assert_eq!(apply.manual_missing.len(), 2);
    // A second install never overwrites the first.
    let r2 = pack::install_pack(&pack_dir, &target, "Fabric Pack", &exports, &http).await.unwrap();
    let WriteOutcome::Created(second) = &r2.outcome else { panic!() };
    assert_ne!(second.root, new.root);

    // Modrinth App target: .mrpack import file.
    let (_t3, mr_dir) = fixture("modrinth");
    let mr = ModrinthApp::open(&mr_dir).unwrap();
    let r = pack::install_pack(&pack_dir, &mr, "Fabric Pack", &exports, &http).await.unwrap();
    let WriteOutcome::ImportFile { path } = &r.outcome else { panic!() };
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    assert!(zip.by_name("modrinth.index.json").is_ok());
    assert!(zip.by_name("overrides/options.txt").is_ok());
    drop(zip);
    assert!(!mr_dir.join("app.db").exists(), "Modrinth DB is never written");

    // CurseForge target: modpack zip with manifest.json.
    let (_t4, cf_dir) = fixture("curseforge");
    let cf = CurseForge::open(&cf_dir.join("Instances")).unwrap();
    let r = pack::install_pack(&pack_dir, &cf, "Fabric Pack", &exports, &http).await.unwrap();
    let WriteOutcome::ImportFile { path } = &r.outcome else { panic!() };
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name("manifest.json").unwrap().read_to_string(&mut text).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["minecraft"]["modLoaders"][0]["id"], "fabric-0.15.11");
}

#[tokio::test]
async fn apply_backs_up_instead_of_deleting() {
    let (_t, prism_dir) = fixture("prism");
    let inst = PrismLauncher::open(&prism_dir).unwrap().read_instance("Fabric Pack").unwrap();
    let work = tempfile::tempdir().unwrap();
    let pack_dir = work.path().join("pack");
    let resolver = Resolver::with_endpoints(&mock_modrinth(), "http://127.0.0.1:9", None);
    pack::export_instance(&inst, &pack_dir, &resolver).await.unwrap();

    // Local drift: an extra mod and an edited config.
    std::fs::write(inst.game_dir.join("mods/extra.jar"), b"extra").unwrap();
    std::fs::write(inst.game_dir.join("config/sodium/options.json"), b"{}").unwrap();
    let report = pack::apply_pack(&inst.game_dir, &pack_dir, &mcsync_core::resolve::http_client()).await.unwrap();

    let backup = report.backup_dir.unwrap();
    assert!(backup.join("mods/extra.jar").is_file());
    assert_eq!(std::fs::read(backup.join("config/sodium/options.json")).unwrap(), b"{}");
    assert!(!inst.game_dir.join("mods/extra.jar").exists());
    assert_eq!(
        std::fs::read_to_string(inst.game_dir.join("config/sodium/options.json")).unwrap(),
        "{\"quality\": \"high\"}\n"
    );
}
