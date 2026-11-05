//! Per-session repository clones inside the session sandbox.
//!
//! A session's only writable view of a repository is a private clone under
//! `<session>/repos/<slug>`: the user's checkout stays a read mount and the
//! host `.git` is never mounted writable. Local-bound repos clone with
//! `git clone --local` (hardlinked objects); GitHub-bound repos clone from
//! the remote URL. The registry at `<session>/repos/repos.toml` records
//! which repository identity each clone serves so clones are reused across
//! agent rebuilds and session-owned refs can be dropped on cleanup.
//!
//! Subagents get child clones of a session clone via [`SessionRepos::child_clone`];
//! their base is always a commit, so a run's work is `git diff base..head`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Error type for repository clone operations.
#[derive(Debug, thiserror::Error)]
pub enum RepoCloneError {
    /// Filesystem error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A `git` invocation failed.
    #[error("git {args} failed: {stderr}")]
    Git {
        /// The full command line attempted.
        args: String,
        /// Captured stderr.
        stderr: String,
    },
    /// The registry file could not be parsed or written.
    #[error("clone registry error: {0}")]
    Registry(String),
    /// The clone directory exists but is not a session clone.
    #[error("clone path {0} exists and is not registered")]
    Conflict(PathBuf),
}

/// One recorded clone: which repository identity it serves and where the
/// source objects live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneRecord {
    /// Repository identity this clone serves (canonicalized git common dir
    /// for local clones, the remote URL for remote clones).
    pub repo_key: String,
    /// Clone directory name inside `<session>/repos/`.
    pub dir: String,
    /// Where the clone's objects came from: the local checkout path or the
    /// remote URL.
    pub source: String,
    /// Whether the clone came from a local checkout.
    pub local: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CloneRegistry {
    #[serde(default)]
    clones: Vec<CloneRecord>,
}

/// Clone directories live at `<session root>/repos/`, registry beside them.
#[derive(Debug, Clone)]
pub struct SessionRepos {
    root: PathBuf,
}

impl SessionRepos {
    /// Manage the clone area of the session rooted at `session_root`.
    #[must_use]
    pub fn new(session_root: &Path) -> Self {
        Self {
            root: session_root.join("repos"),
        }
    }

    /// The directory all clones live under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("repos.toml")
    }

    fn load_registry(&self) -> Result<CloneRegistry, RepoCloneError> {
        match std::fs::read_to_string(self.registry_path()) {
            Ok(content) => toml::from_str(&content)
                .map_err(|e| RepoCloneError::Registry(format!("parse: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CloneRegistry::default()),
            Err(e) => Err(RepoCloneError::Io(e)),
        }
    }

    fn save_registry(&self, registry: &CloneRegistry) -> Result<(), RepoCloneError> {
        std::fs::create_dir_all(&self.root)?;
        let content = toml::to_string_pretty(registry)
            .map_err(|e| RepoCloneError::Registry(e.to_string()))?;
        std::fs::write(self.registry_path(), content)?;
        Ok(())
    }

    /// Records of every clone this session holds.
    ///
    /// # Errors
    /// Fails when the registry cannot be read.
    pub fn clones(&self) -> Result<Vec<CloneRecord>, RepoCloneError> {
        Ok(self.load_registry()?.clones)
    }

    /// The clone directory serving `repo_key`, when one exists.
    ///
    /// # Errors
    /// Fails when the registry cannot be read.
    pub fn clone_for(&self, repo_key: &str) -> Result<Option<PathBuf>, RepoCloneError> {
        Ok(self
            .load_registry()?
            .clones
            .iter()
            .find(|c| c.repo_key == repo_key)
            .map(|c| self.root.join(&c.dir)))
    }

    /// Register `dir` as the clone serving `repo_key` from `source`.
    fn register(
        &self,
        repo_key: String,
        dir: &Path,
        source: String,
        local: bool,
    ) -> Result<(), RepoCloneError> {
        let mut registry = self.load_registry()?;
        registry.clones.push(CloneRecord {
            repo_key,
            dir: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            source,
            local,
        });
        self.save_registry(&registry)
    }

    /// Ensure a local clone of `checkout` exists and return its path.
    ///
    /// `repo_key` is the repository identity (canonicalized git common dir);
    /// `git clone --local` hardlinks objects and no network credential is
    /// reachable inside the clone.
    ///
    /// # Errors
    /// Fails when git fails, the registry cannot be written, or the clone
    /// path is taken by an unregistered directory.
    pub async fn ensure_local_clone(
        &self,
        checkout: &Path,
        repo_key: &Path,
    ) -> Result<PathBuf, RepoCloneError> {
        let key = repo_key.to_string_lossy().into_owned();
        if let Some(dir) = self.clone_for(&key)? {
            return Ok(dir);
        }
        let dir = self.root.join(slug_for(&key));
        if dir.exists() {
            return Err(RepoCloneError::Conflict(dir));
        }
        let source = checkout.to_string_lossy().into_owned();
        let target = dir.to_string_lossy().into_owned();
        blocking::unblock(move || git(None, &["clone", "--local", &source, &target])).await?;
        self.register(key, &dir, checkout.to_string_lossy().into_owned(), true)?;
        Ok(dir)
    }

    /// Ensure a clone of remote `url` exists and return its path. Used by
    /// GitHub-bound sessions: the remote is the source of truth.
    ///
    /// # Errors
    /// Fails when git fails or the registry cannot be written.
    pub async fn ensure_remote_clone(
        &self,
        url: &str,
        repo_key: &str,
    ) -> Result<PathBuf, RepoCloneError> {
        if let Some(dir) = self.clone_for(repo_key)? {
            return Ok(dir);
        }
        let dir = self.root.join(slug_for(repo_key));
        if dir.exists() {
            return Err(RepoCloneError::Conflict(dir));
        }
        let url = url.to_string();
        let target = dir.to_string_lossy().into_owned();
        let source = url.clone();
        blocking::unblock(move || git(None, &["clone", "--origin", "upstream", &url, &target]))
            .await?;
        self.register(repo_key.to_string(), &dir, source, false)?;
        Ok(dir)
    }

    /// Clone `parent` into `child_dir` for a subagent run: a `git clone
    /// --local` of the session clone, so the run's base is always a commit
    /// and its work is `git diff base..head`. Not registered — the parent
    /// session owns it and removes it with the run directory.
    ///
    /// # Errors
    /// Fails when git fails.
    pub async fn child_clone(
        &self,
        parent: &Path,
        child_dir: &Path,
    ) -> Result<PathBuf, RepoCloneError> {
        if let Some(parent_dir) = child_dir.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }
        let source = parent.to_string_lossy().into_owned();
        let target = child_dir.to_string_lossy().into_owned();
        blocking::unblock(move || git(None, &["clone", "--local", &source, &target])).await?;
        Ok(child_dir.to_path_buf())
    }

    /// Remove `dir`. The `refs/may/pr/*` refs in the source checkout belong
    /// to their PRs and are left to them.
    ///
    /// # Errors
    /// Fails on filesystem errors; a missing directory is a no-op.
    pub async fn delete_clone(&self, dir: &Path) -> Result<(), RepoCloneError> {
        let mut registry = self.load_registry()?;
        registry.clones.retain(|c| self.root.join(&c.dir) != dir);
        self.save_registry(&registry)?;
        if dir.exists() {
            let dir = dir.to_path_buf();
            blocking::unblock(move || std::fs::remove_dir_all(dir)).await?;
        }
        Ok(())
    }

    /// Drop every clone; PR refs in the source checkouts stay. Called on
    /// session deletion.
    ///
    /// # Errors
    /// Fails on filesystem errors; missing dirs are skipped.
    pub async fn delete_all(&self) -> Result<(), RepoCloneError> {
        for record in self.clones()? {
            self.delete_clone(&self.root.join(&record.dir)).await?;
        }
        Ok(())
    }
}

/// Uncommitted changes and non-ignored untracked files in `clone`.
///
/// Paths come from `git status --porcelain`. `pr` refuses to merge a clone
/// that returns anything here — nothing is lost silently when the clone is
/// deleted at merge.
///
/// # Errors
/// Fails when git fails.
pub async fn uncommitted_changes(clone: &Path) -> Result<Vec<String>, RepoCloneError> {
    let output = blocking::unblock({
        let clone = clone.to_path_buf();
        move || git(Some(&clone), &["status", "--porcelain"])
    })
    .await?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// `<basename>-<8hex>` from the repository key, sanitized for directory use.
fn slug_for(repo_key: &str) -> String {
    let raw = Path::new(repo_key)
        .file_stem()
        .or_else(|| Path::new(repo_key).file_name())
        .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
    let base: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "repo" } else { base };
    let digest = Sha256::digest(repo_key.as_bytes());
    let mut hash = String::with_capacity(8);
    for b in &digest[..4] {
        let _ = std::fmt::Write::write_fmt(&mut hash, format_args!("{b:02x}"));
    }
    format!("{base}-{hash}")
}

/// Run `git` (optionally `-C dir`) with `args`; return stdout on success.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, RepoCloneError> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command.args(args).output()?;
    if !output.status.success() {
        return Err(RepoCloneError::Git {
            args: format!("{command:?}"),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        git(None, &["init", &dir.to_string_lossy()]).unwrap();
        git(Some(dir), &["config", "user.email", "t@t"]).unwrap();
        git(Some(dir), &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        git(Some(dir), &["add", "."]).unwrap();
        git(Some(dir), &["commit", "-m", "init"]).unwrap();
    }

    fn repo_key(dir: &Path) -> PathBuf {
        dunce::canonicalize(dir.join(".git")).unwrap()
    }

    #[tokio::test]
    async fn local_clone_is_created_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let session = tmp.path().join("session");
        let repos = SessionRepos::new(&session);

        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        assert!(clone.join("file.txt").exists());
        assert!(clone.join(".git").exists());
        // Second call returns the same path without recloning.
        let again = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        assert_eq!(clone, again);
        // The checkout itself is untouched.
        let status = git(Some(&checkout), &["status", "--porcelain"]).unwrap();
        assert_eq!(status.trim(), "");
    }

    #[tokio::test]
    async fn child_clone_stacks_on_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = SessionRepos::new(&tmp.path().join("session"));
        let parent = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();

        let child = repos
            .child_clone(&parent, &tmp.path().join("session/subagents/runs/1/repo"))
            .await
            .unwrap();
        git(Some(&child), &["config", "user.email", "t@t"]).unwrap();
        git(Some(&child), &["config", "user.name", "t"]).unwrap();
        std::fs::write(child.join("child.txt"), "sub").unwrap();
        git(Some(&child), &["add", "."]).unwrap();
        git(Some(&child), &["commit", "-m", "sub work"]).unwrap();
        // The run's work is exactly its own commits on top of the parent base.
        let log = git(Some(&child), &["log", "--format=%s", "-1"]).unwrap();
        assert!(log.contains("sub work"));
        assert_eq!(uncommitted_changes(&parent).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn uncommitted_changes_lists_dirty_and_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = SessionRepos::new(&tmp.path().join("session"));
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        std::fs::write(clone.join("file.txt"), "dirty").unwrap();
        std::fs::write(clone.join("new.txt"), "untracked").unwrap();
        let changes = uncommitted_changes(&clone).await.unwrap();
        assert!(changes.iter().any(|l| l.contains("file.txt")));
        assert!(changes.iter().any(|l| l.contains("new.txt")));
    }

    #[tokio::test]
    async fn delete_clone_removes_dir_and_keeps_pr_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = SessionRepos::new(&tmp.path().join("session"));
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        git(Some(&checkout), &["update-ref", "refs/may/pr/1", "HEAD"]).unwrap();
        repos.delete_clone(&clone).await.unwrap();
        assert!(!clone.exists());
        let refs = git(
            Some(&checkout),
            &["for-each-ref", "refs/may", "--format=%(refname)"],
        )
        .unwrap();
        assert_eq!(refs.trim(), "refs/may/pr/1");
        assert_eq!(repos.clones().unwrap().len(), 0);
    }
}
