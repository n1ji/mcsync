//! Resolve local files to download sources by hash.
//!
//! 1. Modrinth: `POST /v2/version_files` with sha512 hashes.
//! 2. CurseForge: `POST /v1/fingerprints/432` with murmur2 fingerprints
//!    (needs an API key) plus any IDs the launcher already recorded.
//! 3. Files with no usable download (CurseForge authors can block
//!    third-party downloads) are marked `manual`.

use crate::error::{Error, Result};
use crate::hashing::FileHashes;
use crate::launcher::FileHint;
use crate::manifest::{CurseForgeRef, Env, EnvSupport, Hashes, ModrinthRef, PackFile, Side};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub const USER_AGENT: &str = concat!("n1ji/mcsync/", env!("CARGO_PKG_VERSION"), " (github.com/n1ji/mcsync)");
pub const MODRINTH_API: &str = "https://api.modrinth.com";
pub const CURSEFORGE_API: &str = "https://api.curseforge.com";
const MINECRAFT_GAME_ID: u32 = 432;

/// Hosts a manifest may download from (Modrinth's `.mrpack` allow-list plus
/// CurseForge's CDN).
pub const ALLOWED_DOWNLOAD_HOSTS: &[&str] = &[
    "cdn.modrinth.com",
    "github.com",
    "raw.githubusercontent.com",
    "gitlab.com",
    "edge.forgecdn.net",
    "mediafilez.forgecdn.net",
];

pub fn is_allowed_download(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => u.scheme() == "https" && u.host_str().is_some_and(|h| ALLOWED_DOWNLOAD_HOSTS.contains(&h)),
        Err(_) => false,
    }
}

#[derive(Debug, Clone)]
pub struct LocalFile {
    /// Game-dir relative path with `/` separators.
    pub rel_path: String,
    pub hashes: FileHashes,
}

pub struct Resolver {
    http: reqwest::Client,
    modrinth_api: String,
    curseforge_api: String,
    curseforge_key: Option<String>,
}

impl Resolver {
    pub fn new(curseforge_key: Option<String>) -> Self {
        Self::with_endpoints(MODRINTH_API, CURSEFORGE_API, curseforge_key)
    }

    pub fn with_endpoints(modrinth: &str, curseforge: &str, curseforge_key: Option<String>) -> Self {
        Resolver {
            http: http_client(),
            modrinth_api: modrinth.trim_end_matches('/').into(),
            curseforge_api: curseforge.trim_end_matches('/').into(),
            curseforge_key: curseforge_key.filter(|k| !k.trim().is_empty()),
        }
    }

    /// Resolve every file. Unresolvable files come back with `manual = true`.
    pub async fn resolve(&self, files: &[LocalFile], hints: &[FileHint]) -> Result<Vec<PackFile>> {
        let mut out: Vec<PackFile> = files.iter().map(blank_entry).collect();
        if files.is_empty() {
            return Ok(out);
        }

        // Modrinth by sha512.
        let mr = self.modrinth_versions(files).await?;
        let project_ids: Vec<String> = mr
            .values()
            .map(|v| v.project_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let sides = self.modrinth_sides(&project_ids).await.unwrap_or_else(|e| {
            tracing::warn!("could not fetch Modrinth project sides: {e}");
            HashMap::new()
        });
        for (entry, file) in out.iter_mut().zip(files) {
            if let Some(v) = mr.get(&file.hashes.sha512) {
                apply_modrinth(entry, v, sides.get(&v.project_id));
            }
        }

        // CurseForge: launcher hints first (free), then the API if keyed.
        for (entry, file) in out.iter_mut().zip(files) {
            if let Some(cf) = hint_for(file, hints) {
                apply_curseforge(entry, cf, None);
            }
        }
        if self.curseforge_key.is_some() {
            match self.curseforge_matches(files).await {
                Ok(matches) => {
                    for (entry, file) in out.iter_mut().zip(files) {
                        if let Some(m) = matches.get(&file.hashes.murmur2) {
                            let cf = CurseForgeRef {
                                project_id: m.mod_id,
                                file_id: m.id,
                                distributable: m.download_url.is_some(),
                            };
                            apply_curseforge(entry, cf, m.download_url.clone());
                        }
                    }
                }
                Err(e) => tracing::warn!("CurseForge lookup failed: {e}"),
            }
        }

        for entry in &mut out {
            entry.downloads.retain(|u| is_allowed_download(u));
            entry.manual = entry.downloads.is_empty();
        }
        Ok(out)
    }

    async fn modrinth_versions(&self, files: &[LocalFile]) -> Result<HashMap<String, MrVersion>> {
        let mut all = HashMap::new();
        for chunk in files.chunks(500) {
            let hashes: Vec<&str> = chunk.iter().map(|f| f.hashes.sha512.as_str()).collect();
            let resp = self
                .http
                .post(format!("{}/v2/version_files", self.modrinth_api))
                .timeout(API_TIMEOUT)
                .json(&json!({ "hashes": hashes, "algorithm": "sha512" }))
                .send()
                .await?;
            let map: HashMap<String, MrVersion> = read_json(check(resp).await?, MAX_API_RESPONSE).await?;
            all.extend(map);
        }
        Ok(all)
    }

    async fn modrinth_sides(&self, ids: &[String]) -> Result<HashMap<String, MrProject>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(100) {
            let ids_json = serde_json::to_string(chunk)?;
            let resp = self
                .http
                .get(format!("{}/v2/projects", self.modrinth_api))
                .timeout(API_TIMEOUT)
                .query(&[("ids", ids_json)])
                .send()
                .await?;
            let projects: Vec<MrProject> = read_json(check(resp).await?, MAX_API_RESPONSE).await?;
            out.extend(projects.into_iter().map(|p| (p.id.clone(), p)));
        }
        Ok(out)
    }

    async fn curseforge_matches(&self, files: &[LocalFile]) -> Result<HashMap<u32, CfFile>> {
        let key = self.curseforge_key.as_deref().expect("checked by caller");
        let fps: Vec<u32> = files.iter().map(|f| f.hashes.murmur2).collect();
        let resp = self
            .http
            .post(format!("{}/v1/fingerprints/{MINECRAFT_GAME_ID}", self.curseforge_api))
            .timeout(API_TIMEOUT)
            .header("x-api-key", key)
            .json(&json!({ "fingerprints": fps }))
            .send()
            .await?;
        let body: CfFingerprintResponse = read_json(check(resp).await?, MAX_API_RESPONSE).await?;
        Ok(body
            .data
            .exact_matches
            .into_iter()
            .map(|m| (m.file.file_fingerprint, m.file))
            .collect())
    }
}

/// Upper bound for JSON API responses.
pub const MAX_API_RESPONSE: u64 = 32 * 1024 * 1024;
const MAX_ERROR_BODY: u64 = 64 * 1024;
pub const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;

/// Shared client. Redirects are followed only over https, and only to the
/// original host or an allow-listed download host; reqwest drops the
/// Authorization header on any cross-host redirect.
pub fn http_client() -> reqwest::Client {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let same_host = attempt.previous().first().and_then(|u| u.host_str()) == url.host_str();
        let allowed = url.scheme() == "https"
            && (same_host || url.host_str().is_some_and(|h| ALLOWED_DOWNLOAD_HOSTS.contains(&h)));
        if allowed {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(policy)
        .build()
        .expect("static client config")
}

/// Read a response body, failing once it exceeds `max` bytes.
pub async fn read_limited(resp: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    use futures_util::StreamExt;
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(Error::Invalid(format!("response larger than {max} bytes")));
    }
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() as u64 + chunk.len() as u64 > max {
            return Err(Error::Invalid(format!("response larger than {max} bytes")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

pub async fn read_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response, max: u64) -> Result<T> {
    Ok(serde_json::from_slice(&read_limited(resp, max).await?)?)
}

/// Short, size-capped error body for messages.
pub async fn error_body(resp: reqwest::Response) -> String {
    let bytes = read_limited(resp, MAX_ERROR_BODY).await.unwrap_or_default();
    String::from_utf8_lossy(&bytes).chars().take(500).collect()
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    Err(Error::Server { status, body: error_body(resp).await })
}

fn blank_entry(f: &LocalFile) -> PackFile {
    PackFile {
        path: f.rel_path.clone(),
        hashes: Hashes { sha1: f.hashes.sha1.clone(), sha512: f.hashes.sha512.clone() },
        env: None,
        downloads: Vec::new(),
        file_size: f.hashes.size,
        modrinth: None,
        curseforge: None,
        manual: true,
    }
}

fn hint_for(file: &LocalFile, hints: &[FileHint]) -> Option<CurseForgeRef> {
    let name = file.rel_path.rsplit('/').next().unwrap_or(&file.rel_path);
    hints
        .iter()
        .find(|h| h.murmur2 == Some(file.hashes.murmur2))
        .or_else(|| hints.iter().find(|h| h.murmur2.is_none() && h.file_name == name))
        .and_then(|h| h.curseforge.clone())
}

fn apply_modrinth(entry: &mut PackFile, v: &MrVersion, project: Option<&MrProject>) {
    let file = v
        .files
        .iter()
        .find(|f| f.hashes.sha512.as_deref() == Some(entry.hashes.sha512.as_str()))
        .or_else(|| v.files.iter().find(|f| f.primary));
    if let Some(f) = file {
        entry.downloads.insert(0, f.url.clone());
    }
    entry.modrinth = Some(ModrinthRef { project_id: v.project_id.clone(), version_id: v.id.clone() });
    entry.env = project.map(|p| Env { client: env_support(&p.client_side), server: env_support(&p.server_side) });
}

fn apply_curseforge(entry: &mut PackFile, cf: CurseForgeRef, url: Option<String>) {
    if cf.distributable {
        let url = url.unwrap_or_else(|| forgecdn_url(cf.file_id, entry.file_name()));
        if !entry.downloads.contains(&url) {
            entry.downloads.push(url);
        }
    }
    if entry.env.is_none() {
        entry.env = Some(Env::from_side(Side::Both));
    }
    entry.curseforge = Some(cf);
}

/// CurseForge CDN layout: `/files/<id / 1000>/<id % 1000>/<file name>`.
pub fn forgecdn_url(file_id: u64, file_name: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(file_name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("https://edge.forgecdn.net/files/{}/{}/{}", file_id / 1000, file_id % 1000, encoded)
}

fn env_support(s: &str) -> EnvSupport {
    match s {
        "optional" => EnvSupport::Optional,
        "unsupported" => EnvSupport::Unsupported,
        _ => EnvSupport::Required,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct MrVersion {
    pub id: String,
    pub project_id: String,
    pub files: Vec<MrFile>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MrFile {
    pub url: String,
    #[serde(default)]
    pub primary: bool,
    pub hashes: MrHashes,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MrHashes {
    pub sha512: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MrProject {
    pub id: String,
    #[serde(default)]
    pub client_side: String,
    #[serde(default)]
    pub server_side: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CfFingerprintResponse {
    data: CfFingerprintData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CfFingerprintData {
    #[serde(default)]
    exact_matches: Vec<CfMatch>,
}

#[derive(Debug, Deserialize)]
struct CfMatch {
    file: CfFile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CfFile {
    pub id: u64,
    pub mod_id: u64,
    pub download_url: Option<String>,
    pub file_fingerprint: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::hash_bytes;

    fn local(path: &str, bytes: &[u8]) -> LocalFile {
        LocalFile { rel_path: path.into(), hashes: hash_bytes(bytes) }
    }

    #[test]
    fn modrinth_response_applies() {
        let f = local("mods/sodium.jar", b"jar");
        let body = format!(
            r#"{{"{h}": {{"id":"ver1","project_id":"AANobbMI","files":[
                {{"url":"https://cdn.modrinth.com/data/AANobbMI/versions/ver1/sodium.jar",
                  "primary":true,"hashes":{{"sha512":"{h}","sha1":"x"}}}}]}}}}"#,
            h = f.hashes.sha512
        );
        let map: HashMap<String, MrVersion> = serde_json::from_str(&body).unwrap();
        let proj = MrProject { id: "AANobbMI".into(), client_side: "required".into(), server_side: "unsupported".into() };
        let mut e = blank_entry(&f);
        apply_modrinth(&mut e, &map[&f.hashes.sha512], Some(&proj));
        assert_eq!(e.modrinth.as_ref().unwrap().version_id, "ver1");
        assert_eq!(e.env.as_ref().unwrap().side(), Side::Client);
        assert!(e.downloads[0].starts_with("https://cdn.modrinth.com/"));
    }

    #[test]
    fn blocked_curseforge_file_is_manual() {
        let f = local("mods/blocked.jar", b"x");
        let mut e = blank_entry(&f);
        apply_curseforge(&mut e, CurseForgeRef { project_id: 1, file_id: 2, distributable: false }, None);
        assert!(e.downloads.is_empty());
        assert!(e.curseforge.is_some());
    }

    #[test]
    fn hints_match_by_fingerprint_then_name() {
        let f = local("mods/CNB-1.19-1.5.4.jar", b"data");
        let cf = CurseForgeRef { project_id: 414401, file_id: 4556106, distributable: true };
        let by_fp = vec![FileHint { file_name: "other.jar".into(), murmur2: Some(f.hashes.murmur2), curseforge: Some(cf.clone()) }];
        assert_eq!(hint_for(&f, &by_fp), Some(cf.clone()));
        let wrong_fp = vec![FileHint { file_name: "CNB-1.19-1.5.4.jar".into(), murmur2: Some(1), curseforge: Some(cf) }];
        assert_eq!(hint_for(&f, &wrong_fp), None);
    }

    #[test]
    fn cdn_url_and_allow_list() {
        assert_eq!(forgecdn_url(4556106, "CNB 1.19.jar"), "https://edge.forgecdn.net/files/4556/106/CNB%201.19.jar");
        assert!(is_allowed_download("https://cdn.modrinth.com/data/x.jar"));
        assert!(!is_allowed_download("http://cdn.modrinth.com/data/x.jar"));
        assert!(!is_allowed_download("https://evil.example/x.jar"));
    }

    #[test]
    fn curseforge_response_parses() {
        let body = r#"{"data":{"isCacheBuilt":true,"exactMatches":[{"id":414401,"file":{"id":4556106,
            "modId":414401,"fileName":"CNB.jar","downloadUrl":null,"fileFingerprint":3675948766}}]}}"#;
        let r: CfFingerprintResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.data.exact_matches[0].file.file_fingerprint, 3675948766);
        assert!(r.data.exact_matches[0].file.download_url.is_none());
    }
}
