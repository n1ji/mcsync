//! Manifest repo: commit / pull / push via libgit2.
//!
//! Credentials, in order: SSH agent (ssh URLs), a token from the OS
//! keychain (HTTPS), then git's own credential helper.

use crate::error::{Error, Result};
use git2::{
    build::CheckoutBuilder, Cred, CredentialType, FetchOptions, IndexAddOption, PushOptions,
    RemoteCallbacks, Repository, Signature,
};
use std::cell::Cell;
use std::path::{Path, PathBuf};

pub const BRANCH: &str = "main";

pub struct PackRepo {
    repo: Repository,
    token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PullResult {
    UpToDate,
    NoRemoteBranch,
    FastForward,
    Merged,
    /// Merge aborted; local repo left at its previous commit.
    Conflict { paths: Vec<String> },
}

impl PackRepo {
    /// Open the repo at `path`, cloning `url` or initialising an empty repo
    /// if it doesn't exist yet. An existing repo gets its `origin` updated.
    pub fn open_or_init(path: &Path, url: Option<&str>, token: Option<String>) -> Result<Self> {
        let repo = if path.join(".git").is_dir() {
            Repository::open(path)?
        } else if let Some(url) = url {
            std::fs::create_dir_all(path).map_err(|e| Error::Io { path: path.into(), source: e })?;
            let mut fo = FetchOptions::new();
            fo.remote_callbacks(callbacks(token.clone()));
            match git2::build::RepoBuilder::new().fetch_options(fo).clone(url, path) {
                Ok(r) => r,
                // An empty remote can't be cloned; start locally and push later.
                Err(e) if e.code() == git2::ErrorCode::NotFound || e.message().contains("empty") => {
                    init_repo(path)?
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            init_repo(path)?
        };
        if repo.head().is_err() {
            // Unborn HEAD (fresh init, empty remote, or a remote whose HEAD
            // names another branch): use `main`, from origin if it exists.
            let remote_main = repo
                .find_reference(&format!("refs/remotes/origin/{BRANCH}"))
                .and_then(|r| r.peel_to_commit());
            if let Ok(commit) = remote_main {
                repo.branch(BRANCH, &commit, true)?;
            }
            repo.set_head(&format!("refs/heads/{BRANCH}"))?;
            if repo.head().is_ok() {
                repo.checkout_head(Some(CheckoutBuilder::default().force()))?;
            }
        }
        if let Some(url) = url {
            match repo.find_remote("origin") {
                Ok(r) if r.url() == Some(url) => {}
                Ok(_) => repo.remote_set_url("origin", url)?,
                Err(_) => {
                    repo.remote("origin", url)?;
                }
            }
        }
        Ok(PackRepo { repo, token })
    }

    pub fn workdir(&self) -> PathBuf {
        self.repo.workdir().expect("non-bare repo").to_path_buf()
    }

    pub fn packs_dir(&self) -> PathBuf {
        self.workdir().join("packs")
    }

    pub fn is_dirty(&self) -> Result<bool> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        Ok(!self.repo.statuses(Some(&mut opts))?.is_empty())
    }

    /// Stage everything and commit. Returns None if nothing changed.
    pub fn commit_all(&self, message: &str) -> Result<Option<git2::Oid>> {
        if !self.is_dirty()? {
            return Ok(None);
        }
        let mut index = self.repo.index()?;
        index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
        index.update_all(["*"].iter(), None)?;
        index.write()?;
        let tree = self.repo.find_tree(index.write_tree()?)?;
        let sig = self.signature()?;
        let parents: Vec<git2::Commit> = match self.repo.head() {
            Ok(h) => vec![h.peel_to_commit()?],
            Err(_) => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let oid = self.repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)?;
        Ok(Some(oid))
    }

    fn signature(&self) -> Result<Signature<'static>> {
        Ok(self
            .repo
            .signature()
            .or_else(|_| Signature::now("mcsync", "mcsync@localhost"))?)
    }

    pub fn has_remote(&self) -> bool {
        self.repo.find_remote("origin").is_ok()
    }

    pub fn pull(&self) -> Result<PullResult> {
        let mut remote = self.repo.find_remote("origin")?;
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(callbacks(self.token.clone()));
        remote.fetch(&[BRANCH], Some(&mut fo), None)?;

        let fetch_head = match self.repo.find_reference(&format!("refs/remotes/origin/{BRANCH}")) {
            Ok(r) => r,
            Err(_) => return Ok(PullResult::NoRemoteBranch),
        };
        let theirs = self.repo.reference_to_annotated_commit(&fetch_head)?;
        let (analysis, _) = self.repo.merge_analysis(&[&theirs])?;

        if analysis.is_up_to_date() {
            return Ok(PullResult::UpToDate);
        }
        if analysis.is_unborn() || analysis.is_fast_forward() {
            let refname = format!("refs/heads/{BRANCH}");
            match self.repo.find_reference(&refname) {
                Ok(mut r) => {
                    r.set_target(theirs.id(), "mcsync: fast-forward")?;
                }
                Err(_) => {
                    self.repo.reference(&refname, theirs.id(), true, "mcsync: init branch")?;
                }
            }
            self.repo.set_head(&refname)?;
            self.repo.checkout_head(Some(CheckoutBuilder::default().force()))?;
            return Ok(PullResult::FastForward);
        }

        // True merge.
        self.repo.merge(&[&theirs], None, Some(CheckoutBuilder::default().allow_conflicts(true)))?;
        let mut index = self.repo.index()?;
        if index.has_conflicts() {
            let paths = index
                .conflicts()?
                .filter_map(|c| c.ok())
                .filter_map(|c| c.our.or(c.their))
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect();
            // Abort: restore the pre-merge commit (all local work is committed).
            self.repo.cleanup_state()?;
            self.repo.checkout_head(Some(CheckoutBuilder::default().force()))?;
            return Ok(PullResult::Conflict { paths });
        }
        let tree = self.repo.find_tree(index.write_tree()?)?;
        let sig = self.signature()?;
        let ours = self.repo.head()?.peel_to_commit()?;
        let theirs_commit = self.repo.find_commit(theirs.id())?;
        self.repo.commit(Some("HEAD"), &sig, &sig, "mcsync: merge remote changes", &tree, &[&ours, &theirs_commit])?;
        self.repo.cleanup_state()?;
        Ok(PullResult::Merged)
    }

    pub fn push(&self) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let mut po = PushOptions::new();
        let rejected: std::rc::Rc<Cell<Option<String>>> = Default::default();
        let mut cb = callbacks(self.token.clone());
        let r2 = rejected.clone();
        cb.push_update_reference(move |refname, status| {
            if let Some(s) = status {
                r2.set(Some(format!("{refname}: {s}")));
            }
            Ok(())
        });
        po.remote_callbacks(cb);
        remote.push(&[format!("refs/heads/{BRANCH}:refs/heads/{BRANCH}")], Some(&mut po))?;
        if let Some(msg) = rejected.take() {
            return Err(Error::Invalid(format!("push rejected ({msg}); pull first")));
        }
        Ok(())
    }
}

fn init_repo(path: &Path) -> Result<Repository> {
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head(BRANCH);
    let repo = Repository::init_opts(path, &opts)?;
    std::fs::write(
        path.join(".gitignore"),
        "# mcsync pack repo: never commit secrets\n*.token\n.env\n",
    )
    .map_err(|e| Error::Io { path: path.into(), source: e })?;
    Ok(repo)
}

fn callbacks(token: Option<String>) -> RemoteCallbacks<'static> {
    let mut cb = RemoteCallbacks::new();
    let attempts = Cell::new(0u32);
    cb.credentials(move |url, username, allowed| {
        // libgit2 re-invokes this on auth failure; stop instead of looping.
        attempts.set(attempts.get() + 1);
        if attempts.get() > 4 {
            return Err(git2::Error::from_str("authentication failed"));
        }
        if allowed.contains(CredentialType::SSH_KEY) {
            return Cred::ssh_key_from_agent(username.unwrap_or("git"));
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            if let Some(t) = &token {
                if attempts.get() <= 2 {
                    return Cred::userpass_plaintext("x-access-token", t);
                }
            }
            if let Ok(cfg) = git2::Config::open_default() {
                if let Ok(c) = Cred::credential_helper(&cfg, url, username) {
                    return Ok(c);
                }
            }
        }
        if allowed.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }
        Err(git2::Error::from_str("no usable git credentials"))
    });
    cb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_push_pull_between_clones() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        Repository::init_bare(&bare).unwrap();
        let url = bare.to_string_lossy().replace('\\', "/");

        let a = PackRepo::open_or_init(&tmp.path().join("a"), Some(&url), None).unwrap();
        std::fs::create_dir_all(a.packs_dir().join("p")).unwrap();
        std::fs::write(a.packs_dir().join("p/modrinth.index.json"), "{}").unwrap();
        assert!(a.commit_all("add pack").unwrap().is_some());
        assert!(a.commit_all("noop").unwrap().is_none());
        a.push().unwrap();

        let b = PackRepo::open_or_init(&tmp.path().join("b"), Some(&url), None).unwrap();
        assert!(b.packs_dir().join("p/modrinth.index.json").is_file());

        std::fs::write(a.packs_dir().join("p/modrinth.index.json"), "{\"v\":2}").unwrap();
        a.commit_all("update").unwrap();
        a.push().unwrap();
        assert_eq!(b.pull().unwrap(), PullResult::FastForward);
        assert_eq!(std::fs::read_to_string(b.packs_dir().join("p/modrinth.index.json")).unwrap(), "{\"v\":2}");
        assert_eq!(b.pull().unwrap(), PullResult::UpToDate);

        // Diverging edits to the same file conflict and are aborted cleanly.
        std::fs::write(a.packs_dir().join("p/modrinth.index.json"), "{\"v\":\"a\"}").unwrap();
        a.commit_all("a").unwrap();
        a.push().unwrap();
        std::fs::write(b.packs_dir().join("p/modrinth.index.json"), "{\"v\":\"b\"}").unwrap();
        b.commit_all("b").unwrap();
        assert!(matches!(b.pull().unwrap(), PullResult::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(b.packs_dir().join("p/modrinth.index.json")).unwrap(), "{\"v\":\"b\"}");
        assert!(!b.is_dirty().unwrap());
    }
}
