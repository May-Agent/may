//! Per-session repository clones inside the session sandbox.
//!
//! A session's only writable view of a repository is a private clone under
//! `<session>/repos/<dir>`: the user's checkout stays a read mount and the
//! host `.git` is never mounted writable. Local-bound repos clone with
//! `git clone --local --no-hardlinks` (objects are copied, so the clone shares
//! no writable file with its source); GitHub-bound repos clone from
//! the remote URL. The host's clone registry lives in the history database
//! (`session_repo_clones`), never in the agent-writable session directory:
//! it records which repository identity each clone serves so clones are
//! reused across agent rebuilds and session-owned refs can be dropped on
//! cleanup. A clone's directory is a [`CloneDir`], a single path component,
//! so deleting a clone can only ever target a child of `repos/`.
//!
//! Subagents get child clones of a session clone via [`SessionRepos::child_clone`];
//! their base is always a commit, so a run's work is `git diff base..head`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::{ChatDb, ChatDbError, RepoCloneRow};

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
    /// The clone registry could not be read or written.
    #[error("clone registry error: {0}")]
    Registry(#[from] ChatDbError),
    /// A registered clone directory is not a single path component.
    #[error("registered clone directory {0:?} is not a single path component")]
    InvalidDir(String),
    /// The clone path exists but is not a real directory.
    #[error("clone path {0} exists and is not a directory")]
    Conflict(PathBuf),
}

/// A clone's directory name inside `<session>/repos/`: exactly one normal
/// path component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneDir(String);

impl CloneDir {
    /// The directory a clone of `repo_key` gets: `<basename>-<8hex>`,
    /// sanitized for directory use.
    fn for_repo_key(repo_key: &str) -> Self {
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
        Self(format!("{base}-{hash}"))
    }

    /// The directory name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CloneDir {
    type Error = RepoCloneError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let mut components = Path::new(&name).components();
        match (components.next(), components.next()) {
            (Some(std::path::Component::Normal(only)), None) if only == name.as_str() => {
                Ok(Self(name))
            }
            _ => Err(RepoCloneError::InvalidDir(name)),
        }
    }
}

impl std::fmt::Display for CloneDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One recorded clone: which repository identity it serves and where the
/// source objects live.
#[derive(Debug, Clone)]
pub struct CloneRecord {
    /// Repository identity this clone serves (canonicalized git common dir
    /// for local clones, the remote URL for remote clones).
    pub repo_key: String,
    /// Clone directory inside `<session>/repos/`.
    pub dir: CloneDir,
    /// Where the clone's objects came from: the local checkout path or the
    /// remote URL.
    pub source: String,
    /// Whether the clone came from a local checkout.
    pub local: bool,
}

impl TryFrom<RepoCloneRow> for CloneRecord {
    type Error = RepoCloneError;

    fn try_from(row: RepoCloneRow) -> Result<Self, Self::Error> {
        Ok(Self {
            repo_key: row.repo_key,
            dir: CloneDir::try_from(row.dir)?,
            source: row.source,
            local: row.local,
        })
    }
}

/// A clone as a client sees it: absolute location and origin.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
pub struct CloneView {
    /// Absolute path of the clone.
    pub path: String,
    /// Local checkout path or remote URL the clone came from.
    pub source: String,
    /// Whether the clone came from a local checkout.
    pub local: bool,
}

/// Clone directories live at `<session root>/repos/`; the registry of them
/// lives in the history database.
#[derive(Debug, Clone)]
pub struct SessionRepos {
    session_root: PathBuf,
    root: PathBuf,
    session_id: String,
    db: ChatDb,
}

impl SessionRepos {
    /// Manage the clone area of the session rooted at `session_root`.
    #[must_use]
    pub fn new(session_root: &Path, session_id: impl Into<String>, db: ChatDb) -> Self {
        Self {
            session_root: session_root.to_path_buf(),
            root: session_root.join("repos"),
            session_id: session_id.into(),
            db,
        }
    }

    /// The directory all clones live under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the clone in `dir` lives.
    #[must_use]
    pub fn path(&self, dir: &CloneDir) -> PathBuf {
        self.root.join(&dir.0)
    }

    /// Records of every clone this session holds.
    ///
    /// # Errors
    /// Fails when the registry cannot be read or holds an invalid directory.
    pub async fn clones(&self) -> Result<Vec<CloneRecord>, RepoCloneError> {
        self.db
            .list_repo_clones(&self.session_id)
            .await?
            .into_iter()
            .map(CloneRecord::try_from)
            .collect()
    }

    /// Every clone as a client sees it.
    ///
    /// # Errors
    /// Fails when the registry cannot be read.
    pub async fn views(&self) -> Result<Vec<CloneView>, RepoCloneError> {
        Ok(self
            .clones()
            .await?
            .into_iter()
            .map(|c| CloneView {
                path: self.path(&c.dir).to_string_lossy().into_owned(),
                source: c.source,
                local: c.local,
            })
            .collect())
    }

    /// The clone directory serving `repo_key`, when one exists.
    ///
    /// # Errors
    /// Fails when the registry cannot be read.
    pub async fn clone_for(&self, repo_key: &str) -> Result<Option<PathBuf>, RepoCloneError> {
        Ok(self
            .clones()
            .await?
            .iter()
            .find(|c| c.repo_key == repo_key)
            .map(|c| self.path(&c.dir)))
    }

    /// Register `dir` as the clone serving `repo_key` from `source`.
    async fn register(
        &self,
        repo_key: String,
        dir: &CloneDir,
        source: String,
        local: bool,
    ) -> Result<(), RepoCloneError> {
        self.db
            .insert_repo_clone(
                &self.session_id,
                &RepoCloneRow {
                    repo_key,
                    dir: dir.0.clone(),
                    source,
                    local,
                },
            )
            .await?;
        Ok(())
    }

    /// Ensure a local clone of `checkout` exists and return its path.
    ///
    /// `repo_key` is the repository identity (canonicalized git common dir);
    /// objects are copied, never hardlinked, and no network credential is
    /// reachable inside the clone.
    ///
    /// # Errors
    /// Fails when git fails, the registry cannot be written, or the clone
    /// path is taken by something that is not a directory.
    pub async fn ensure_local_clone(
        &self,
        checkout: &Path,
        repo_key: &Path,
    ) -> Result<PathBuf, RepoCloneError> {
        let key = repo_key.to_string_lossy().into_owned();
        if let Some(dir) = self.clone_for(&key).await? {
            return Ok(dir);
        }
        let clone_dir = CloneDir::for_repo_key(&key);
        let dir = self.path(&clone_dir);
        if real_dir_exists(&dir)? {
            // A clone from before the registry was host-owned: adopt it
            // under the source this call is about to clone from.
            self.register(
                key,
                &clone_dir,
                checkout.to_string_lossy().into_owned(),
                true,
            )
            .await?;
            return Ok(dir);
        }
        local_clone(checkout, &dir).await?;
        self.register(
            key,
            &clone_dir,
            checkout.to_string_lossy().into_owned(),
            true,
        )
        .await?;
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
        if let Some(dir) = self.clone_for(repo_key).await? {
            return Ok(dir);
        }
        let clone_dir = CloneDir::for_repo_key(repo_key);
        let dir = self.path(&clone_dir);
        if real_dir_exists(&dir)? {
            self.register(repo_key.to_string(), &clone_dir, url.to_string(), false)
                .await?;
            return Ok(dir);
        }
        let url = url.to_string();
        let target = dir.to_string_lossy().into_owned();
        let source = url.clone();
        blocking::unblock(move || git(None, &["clone", "--origin", "upstream", &url, &target]))
            .await?;
        self.register(repo_key.to_string(), &clone_dir, source, false)
            .await?;
        Ok(dir)
    }

    /// Clone `parent` into `child_dir` for a subagent run: a local clone
    /// of the session clone that shares no file with it, so the run's base is always a commit
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
        local_clone(parent, child_dir).await?;
        Ok(child_dir.to_path_buf())
    }

    /// Remove the registered clone in `dir`. The `refs/may/pr/*` refs in the
    /// source checkout belong to their PRs and are left to them.
    ///
    /// # Errors
    /// Fails on registry or filesystem errors; an unregistered or missing
    /// clone is a no-op.
    pub async fn delete_clone(&self, dir: &CloneDir) -> Result<(), RepoCloneError> {
        if !self.clones().await?.iter().any(|c| &c.dir == dir) {
            return Ok(());
        }
        self.db
            .delete_repo_clone(&self.session_id, dir.as_str())
            .await?;
        let session_root = self.session_root.clone();
        let dir = dir.clone();
        blocking::unblock(move || confined::remove_clone(&session_root, &dir)).await?;
        Ok(())
    }

    /// Drop every clone; PR refs in the source checkouts stay. Called on
    /// session deletion.
    ///
    /// # Errors
    /// Fails on registry or filesystem errors.
    pub async fn delete_all(&self) -> Result<(), RepoCloneError> {
        for record in self.clones().await? {
            self.delete_clone(&record.dir).await?;
        }
        Ok(())
    }
}

/// Whether `path` is a real directory; a symlink or file there is a conflict.
fn real_dir_exists(path: &Path) -> Result<bool, RepoCloneError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(true),
        Ok(_) => Err(RepoCloneError::Conflict(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Deletion confined beneath the session directory. The session directory
/// is host-owned but everything in it is agent-writable, so each step runs
/// relative to an open handle and never follows a symlink: a `repos` or
/// clone directory swapped for a link is refused or unlinked, never entered.
mod confined {
    use std::ffi::CStr;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, open, openat, statat, unlinkat};
    use rustix::io::Errno;

    use super::CloneDir;

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// Remove `repos/<dir>` beneath `session_root`; a missing one is a no-op,
    /// a `repos` that is not a real directory is an error.
    pub(super) fn remove_clone(session_root: &Path, dir: &CloneDir) -> io::Result<()> {
        let name = std::ffi::CString::new(dir.as_str())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let session = open(
            session_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let repos = match openat(&session, c"repos", DIRECTORY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        remove_entry(&repos, &name)
    }

    /// Remove `name` inside `parent`: a directory with its contents, any
    /// other entry (a symlink included) by unlinking it.
    fn remove_entry(parent: &OwnedFd, name: &CStr) -> io::Result<()> {
        let stat = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Ok(unlinkat(parent, name, AtFlags::empty())?);
        }
        let child = openat(parent, name, DIRECTORY, Mode::empty())?;
        for entry in Dir::read_from(&child)? {
            let entry = entry?;
            let entry_name = entry.file_name();
            if entry_name != c"." && entry_name != c".." {
                remove_entry(&child, entry_name)?;
            }
        }
        Ok(unlinkat(parent, name, AtFlags::REMOVEDIR)?)
    }
}

/// Clone `source` into `target` copying its objects: the sandbox runs as the
/// user's uid, so a hardlinked object made writable would change the source's
/// inode too.
async fn local_clone(source: &Path, target: &Path) -> Result<(), RepoCloneError> {
    let source = source.to_string_lossy().into_owned();
    let target = target.to_string_lossy().into_owned();
    blocking::unblock(move || {
        git(
            None,
            &["clone", "--local", "--no-hardlinks", &source, &target],
        )
    })
    .await?;
    Ok(())
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

    async fn session_repos(tmp: &Path) -> SessionRepos {
        let db = ChatDb::open(&tmp.join("history.db")).await.unwrap();
        SessionRepos::new(&tmp.join("session"), "s1", db)
    }

    #[tokio::test]
    async fn local_clone_is_created_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = session_repos(tmp.path()).await;

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

    /// Make every loose object file of `repo` writable and empty it, as a
    /// sandboxed command running as the user could.
    fn corrupt_objects(repo: &Path) -> usize {
        use std::os::unix::fs::PermissionsExt as _;
        let mut corrupted = 0;
        let mut stack = vec![repo.join(".git/objects")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let mut perms = std::fs::metadata(&path).unwrap().permissions();
                    perms.set_mode(0o644);
                    std::fs::set_permissions(&path, perms).unwrap();
                    std::fs::write(&path, b"").unwrap();
                    corrupted += 1;
                }
            }
        }
        corrupted
    }

    #[tokio::test]
    async fn corrupting_a_clone_leaves_its_source_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = session_repos(tmp.path()).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        assert!(corrupt_objects(&clone) > 0);
        git(Some(&checkout), &["fsck", "--strict"]).unwrap();

        // A subagent's child clone shares nothing with the session clone.
        let fresh = tmp.path().join("fresh");
        init_repo(&fresh);
        let parent = repos
            .ensure_local_clone(&fresh, &repo_key(&fresh))
            .await
            .unwrap();
        let child = repos
            .child_clone(&parent, &tmp.path().join("session/subagents/runs/1/repo"))
            .await
            .unwrap();
        assert!(corrupt_objects(&child) > 0);
        git(Some(&parent), &["fsck", "--strict"]).unwrap();
        git(Some(&fresh), &["fsck", "--strict"]).unwrap();
    }

    #[tokio::test]
    async fn child_clone_stacks_on_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = session_repos(tmp.path()).await;
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
        let repos = session_repos(tmp.path()).await;
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
        let repos = session_repos(tmp.path()).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        git(Some(&checkout), &["update-ref", "refs/may/pr/1", "HEAD"]).unwrap();
        let record = repos.clones().await.unwrap().remove(0);
        repos.delete_clone(&record.dir).await.unwrap();
        assert!(!clone.exists());
        let refs = git(
            Some(&checkout),
            &["for-each-ref", "refs/may", "--format=%(refname)"],
        )
        .unwrap();
        assert_eq!(refs.trim(), "refs/may/pr/1");
        assert_eq!(repos.clones().await.unwrap().len(), 0);
    }

    #[test]
    fn clone_dir_is_one_normal_component() {
        for ok in ["repo-1a2b3c4d", "a.b", "..."] {
            assert!(CloneDir::try_from(ok.to_string()).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "../x", "a/b", "/abs", "a/", "/"] {
            assert!(CloneDir::try_from(bad.to_string()).is_err(), "{bad}");
        }
    }

    /// Everything the agent can write — `repos/repos.toml` naming absolute
    /// and `..` directories, the clone replaced by a symlink to a victim —
    /// leaves paths outside `repos/` alone, and no ref is touched in an
    /// agent-named source.
    #[tokio::test]
    async fn tampered_session_directory_deletes_nothing_outside_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let decoy = tmp.path().join("decoy");
        init_repo(&decoy);
        git(Some(&decoy), &["update-ref", "refs/may/s1", "HEAD"]).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("precious"), "keep").unwrap();
        let sibling = tmp.path().join("session/sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("precious"), "keep").unwrap();

        let repos = session_repos(tmp.path()).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();

        // The legacy registry file, rewritten by the agent.
        let forged = format!(
            "[[clones]]\nrepo_key = \"x\"\ndir = \"{}\"\nsource = \"{}\"\nlocal = true\n\n\
             [[clones]]\nrepo_key = \"y\"\ndir = \"../sibling\"\nsource = \"{}\"\nlocal = true\n",
            victim.display(),
            decoy.display(),
            decoy.display(),
        );
        std::fs::write(repos.root().join("repos.toml"), forged).unwrap();
        // The clone itself swapped for a symlink to the victim.
        std::fs::remove_dir_all(&clone).unwrap();
        std::os::unix::fs::symlink(&victim, &clone).unwrap();

        repos.delete_all().await.unwrap();

        assert!(victim.join("precious").exists());
        assert!(sibling.join("precious").exists());
        assert!(!clone.exists() && clone.symlink_metadata().is_err());
        let decoy_refs = git(
            Some(&decoy),
            &["for-each-ref", "refs/may", "--format=%(refname)"],
        )
        .unwrap();
        assert_eq!(decoy_refs.trim(), "refs/may/s1");
    }

    /// `repos` swapped for a symlink to a directory holding a same-named
    /// entry: nothing is deleted through the link.
    #[tokio::test]
    async fn a_symlinked_repos_dir_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = session_repos(tmp.path()).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        let victim = tmp.path().join("victim");
        let victim_clone = victim.join(clone.file_name().unwrap());
        std::fs::create_dir_all(&victim_clone).unwrap();
        std::fs::write(victim_clone.join("precious"), "keep").unwrap();
        std::fs::remove_dir_all(repos.root()).unwrap();
        std::os::unix::fs::symlink(&victim, repos.root()).unwrap();

        assert!(repos.delete_all().await.is_err());
        assert!(victim_clone.join("precious").exists());
    }

    /// A clone from before the registry was host-owned is adopted under the
    /// source of the call, whatever the session directory says.
    #[tokio::test]
    async fn an_unregistered_clone_directory_is_adopted_with_the_host_source() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = session_repos(tmp.path()).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();
        std::fs::write(clone.join("work.txt"), "in progress").unwrap();
        let dir = repos.clones().await.unwrap().remove(0).dir;
        repos
            .db
            .delete_repo_clone("s1", dir.as_str())
            .await
            .unwrap();
        std::fs::write(
            repos.root().join("repos.toml"),
            "[[clones]]\nrepo_key = \"k\"\ndir = \"x\"\nsource = \"/evil\"\nlocal = true\n",
        )
        .unwrap();

        let again = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout))
            .await
            .unwrap();

        assert_eq!(again, clone);
        assert!(clone.join("work.txt").exists());
        let records = repos.clones().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, checkout.to_string_lossy());
        assert!(records[0].local);
    }

    /// A symlink at the clone's computed path is no clone to adopt.
    #[tokio::test]
    async fn a_symlink_at_the_clone_path_is_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        init_repo(&checkout);
        let repos = session_repos(tmp.path()).await;
        std::fs::create_dir_all(repos.root()).unwrap();
        let dir = repos.path(&CloneDir::for_repo_key(
            &repo_key(&checkout).to_string_lossy(),
        ));
        std::os::unix::fs::symlink(tmp.path(), &dir).unwrap();
        assert!(matches!(
            repos
                .ensure_local_clone(&checkout, &repo_key(&checkout))
                .await,
            Err(RepoCloneError::Conflict(_))
        ));
    }

    /// A registry row that is not a single component is refused when read.
    #[tokio::test]
    async fn invalid_registered_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = session_repos(tmp.path()).await;
        repos
            .db
            .insert_repo_clone(
                "s1",
                &RepoCloneRow {
                    repo_key: "k".into(),
                    dir: "../escape".into(),
                    source: "s".into(),
                    local: true,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            repos.clones().await,
            Err(RepoCloneError::InvalidDir(_))
        ));
    }
}
