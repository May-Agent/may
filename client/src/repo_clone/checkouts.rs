//! Daemon-held default-branch checkouts and copy-on-write generation management.
//!
//! Each GitHub repository used by any session gets a single daemon-managed
//! checkout under `<may_dir>/checkouts/<slug>/`:
//! - `.repo_key` records the canonical `owner/repo` key.
//! - `generations/<n>/` holds immutable (to sessions) default-branch snapshots.
//! - `current` is a symlink atomically swapped (`symlink` + `rename`) to the
//!   latest `generations/<n>`.
//!
//! Sessions mount `<may_dir>/checkouts/<slug>` read-only. Whenever a GitHub
//! repository changes (when a PR merges, or before deriving a session
//! workspace when the remote has moved), the daemon clones the current
//! generation copy-on-write into `generations/<n+1>`, updates it cleanly to
//! remote HEAD, swaps `current` atomically, and prunes older generations that
//! no active reader holds.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_lock::Mutex as AsyncMutex;
use dashmap::DashMap;
use walkdir::WalkDir;

use super::{CloneDir, RepoCloneError};
use may_config::host_git::HostGit;

/// Directory name holding versioned generations inside `<checkouts>/<slug>/`.
const GENERATIONS_DIR: &str = "generations";
/// Symlink pointing at the active `generations/<n>` inside `<checkouts>/<slug>/`.
const CURRENT_LINK: &str = "current";
/// Metadata file recording the repository key inside `<checkouts>/<slug>/`.
const REPO_KEY_FILE: &str = ".repo_key";

/// A point-in-time snapshot of a daemon default-branch checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSnapshot {
    /// Repository key (`owner/repo`).
    pub repo_key: String,
    /// `<may_dir>/checkouts/<slug>` root granted read-only to sessions.
    pub root_dir: PathBuf,
    /// `<may_dir>/checkouts/<slug>/current` symlink path.
    pub current_dir: PathBuf,
    /// Resolved `generations/<n>` path for this snapshot.
    pub generation_dir: PathBuf,
    /// Generation number `n`.
    pub generation: u64,
    /// Current HEAD commit SHA on the default branch.
    pub head_sha: String,
    /// Default branch name (for example `main`).
    pub default_branch: String,
}

/// Outcome of refreshing a daemon checkout against its remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRefresh {
    /// Resulting active checkout snapshot.
    pub snapshot: CheckoutSnapshot,
    /// Previous HEAD commit SHA before the update (`None` when freshly cloned).
    pub previous_sha: Option<String>,
    /// Whether `current` moved to a newly created generation.
    pub updated: bool,
}

#[derive(Debug, Default)]
struct RepoEntry {
    lock: AsyncMutex<()>,
    leases: Mutex<HashMap<u64, usize>>,
}

impl RepoEntry {
    fn acquire(&self, generation: u64) {
        let mut guard = self
            .leases
            .lock()
            .expect("checkout lease table is never poisoned");
        *guard.entry(generation).or_insert(0) += 1;
    }

    fn release(&self, generation: u64) -> usize {
        let mut guard = self
            .leases
            .lock()
            .expect("checkout lease table is never poisoned");
        let count = guard.entry(generation).or_insert(0);
        *count = count.saturating_sub(1);
        let remaining = *count;
        if remaining == 0 {
            guard.remove(&generation);
        }
        remaining
    }

    fn is_leased(&self, generation: u64) -> bool {
        let guard = self
            .leases
            .lock()
            .expect("checkout lease table is never poisoned");
        guard.get(&generation).copied().unwrap_or(0) > 0
    }
}

/// Active reader hold on a specific checkout generation.
///
/// While at least one [`GenerationLease`] is live on generation `n`, cleanup
/// skips `generations/<n>`. Dropping or explicitly releasing the last lease on
/// a superseded generation removes it.
#[derive(Debug)]
pub struct GenerationLease {
    repo_dir: PathBuf,
    generation: u64,
    generation_dir: PathBuf,
    entry: Arc<RepoEntry>,
}

impl GenerationLease {
    /// Directory path of the pinned generation.
    #[must_use]
    pub fn generation_dir(&self) -> &Path {
        &self.generation_dir
    }

    /// Generation number pinned by this lease.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Explicitly release this lease and prune its generation asynchronously
    /// when it is no longer current and no other reader holds it.
    pub async fn release(self) {
        let repo_dir = self.repo_dir.clone();
        let entry = Arc::clone(&self.entry);
        let generation = self.generation;
        std::mem::forget(self);
        let remaining = entry.release(generation);
        if remaining == 0 {
            blocking::unblock(move || {
                prune_generation_if_unreferenced(&repo_dir, &entry, generation);
            })
            .await;
        }
    }
}

impl Drop for GenerationLease {
    fn drop(&mut self) {
        let remaining = self.entry.release(self.generation);
        if remaining == 0 {
            let repo_dir = self.repo_dir.clone();
            let entry = Arc::clone(&self.entry);
            let generation = self.generation;
            blocking::unblock(move || {
                prune_generation_if_unreferenced(&repo_dir, &entry, generation);
            })
            .detach();
        }
    }
}

/// Shared daemon manager for per-repository default-branch checkouts.
#[derive(Debug, Clone)]
pub struct DaemonCheckouts {
    root: PathBuf,
    host: HostGit,
    /// The `[forge]` credentials HTTPS clones and fetches authenticate with.
    forge: Option<may_config::forge::LiveForgeAuth>,
    entries: Arc<DashMap<String, Arc<RepoEntry>>>,
}

impl DaemonCheckouts {
    /// Create a checkout registry rooted at `root` (`<may_dir>/checkouts`).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            host: HostGit::inherit(),
            forge: None,
            entries: Arc::new(DashMap::new()),
        }
    }

    /// Configure host git configuration used for remote queries and updates.
    #[must_use]
    pub fn with_host(mut self, host: HostGit) -> Self {
        self.host = host;
        self
    }

    /// Authenticate HTTPS clones and fetches with the `[forge]` credentials
    /// `forge` holds.
    #[must_use]
    pub fn with_forge_auth(mut self, forge: may_config::forge::LiveForgeAuth) -> Self {
        self.forge = Some(forge);
        self
    }

    /// The git `remote_url` is cloned or fetched with: the host git
    /// carrying the URL host's `[forge]` credential when there is one, the
    /// plain host otherwise so public repositories still work.
    async fn host_for_remote(&self, remote_url: &str) -> Result<HostGit, RepoCloneError> {
        let (Some(forge), Some(host)) = (&self.forge, super::https_url_host(remote_url)) else {
            return Ok(self.host.clone());
        };
        match forge.git_https_auth(host).await {
            Ok(auth) => Ok(self.host.clone().with_https_auth(auth)),
            Err(may_config::forge::ForgeAuthError::Unauthenticated { .. }) => Ok(self.host.clone()),
            Err(error) => Err(RepoCloneError::ForgeAuth(error)),
        }
    }

    /// Base directory under which all repository checkouts live.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding `.repo_key`, `generations/`, and `current` for
    /// `repo_key`.
    #[must_use]
    pub fn repo_dir(&self, repo_key: &str) -> PathBuf {
        self.root.join(CloneDir::for_repo_key(repo_key).as_str())
    }

    /// Symlink pointing to the active default-branch generation for `repo_key`.
    #[must_use]
    pub fn current_link(&self, repo_key: &str) -> PathBuf {
        self.repo_dir(repo_key).join(CURRENT_LINK)
    }

    fn entry_for(&self, repo_key: &str) -> Arc<RepoEntry> {
        Arc::clone(
            self.entries
                .entry(repo_key.to_string())
                .or_insert_with(|| Arc::new(RepoEntry::default()))
                .value(),
        )
    }

    /// Ensure a read-only default-branch checkout exists for `repo_key`.
    ///
    /// Reuses an existing valid `current` generation without hitting the network
    /// when one is already present, so session startup never spawns an eager
    /// clone when the checkout is warm.
    ///
    /// # Errors
    /// Fails when cloning the initial checkout or inspecting `HEAD` fails.
    pub async fn ensure_present(
        &self,
        repo_key: &str,
        remote_url: &str,
    ) -> Result<CheckoutSnapshot, RepoCloneError> {
        let entry = self.entry_for(repo_key);
        let _guard = entry.lock.lock().await;
        let repo_dir = self.repo_dir(repo_key);
        let host = self.host_for_remote(remote_url).await?;
        let repo_key_owned = repo_key.to_string();
        let remote_url_owned = remote_url.to_string();

        blocking::unblock(move || {
            write_repo_key_file(&repo_dir, &repo_key_owned)?;
            if let Some(snapshot) = read_current_snapshot(&host, &repo_dir, &repo_key_owned)? {
                return Ok(snapshot);
            }
            initial_clone(&host, &repo_dir, &repo_key_owned, &remote_url_owned)
        })
        .await
    }

    /// Check `remote_url`'s default branch `HEAD` and update `repo_key` via
    /// copy-on-write + atomic `current` swap when the remote has moved (or
    /// perform the initial clone when none exists).
    ///
    /// # Errors
    /// Fails when `ls-remote`, copy-on-write cloning, fetching, or swapping fails.
    pub async fn refresh(
        &self,
        repo_key: &str,
        remote_url: &str,
    ) -> Result<CheckoutRefresh, RepoCloneError> {
        let entry = self.entry_for(repo_key);
        let _guard = entry.lock.lock().await;
        self.refresh_locked(repo_key, remote_url, &entry).await
    }

    /// [`Self::refresh`] for a caller already holding `entry`'s lock: a
    /// generation is made, swapped and pruned under the same lock, so a
    /// reader that keeps the lock can lease the result with no prune window.
    async fn refresh_locked(
        &self,
        repo_key: &str,
        remote_url: &str,
        entry: &Arc<RepoEntry>,
    ) -> Result<CheckoutRefresh, RepoCloneError> {
        let repo_dir = self.repo_dir(repo_key);
        let host = self.host_for_remote(remote_url).await?;
        let repo_key_owned = repo_key.to_string();
        let remote_url_owned = remote_url.to_string();
        let entry_ref = Arc::clone(entry);

        blocking::unblock(move || {
            write_repo_key_file(&repo_dir, &repo_key_owned)?;
            let Some(current) = read_current_snapshot(&host, &repo_dir, &repo_key_owned)? else {
                let snapshot = initial_clone(&host, &repo_dir, &repo_key_owned, &remote_url_owned)?;
                return Ok(CheckoutRefresh {
                    snapshot,
                    previous_sha: None,
                    updated: true,
                });
            };
            let remote = ls_remote_head(&host, &remote_url_owned)?;
            if remote.sha == current.head_sha {
                return Ok(CheckoutRefresh {
                    snapshot: current,
                    previous_sha: Some(remote.sha),
                    updated: false,
                });
            }
            let next_gen = next_generation_number(&repo_dir, current.generation)?;
            let generations_dir = repo_dir.join(GENERATIONS_DIR);
            std::fs::create_dir_all(&generations_dir)?;
            let staging_dir =
                generations_dir.join(format!("staging-{next_gen}-{}", may_protocol::id::new()));
            if staging_dir.exists() {
                std::fs::remove_dir_all(&staging_dir)?;
            }
            cow_copy_dir(&current.generation_dir, &staging_dir)?;
            let updated_res = update_generation_cleanly(
                &host,
                &staging_dir,
                &remote_url_owned,
                remote.branch.as_deref().unwrap_or(&current.default_branch),
                &remote.sha,
            );
            if let Err(error) = updated_res {
                let _ = std::fs::remove_dir_all(&staging_dir);
                return Err(error);
            }
            let final_dir = generations_dir.join(next_gen.to_string());
            if final_dir.exists() {
                std::fs::remove_dir_all(&final_dir)?;
            }
            std::fs::rename(&staging_dir, &final_dir)?;
            swap_current_symlink(&repo_dir, next_gen)?;
            prune_stale_generations(&repo_dir, &entry_ref, next_gen);
            let snapshot =
                snapshot_from_generation(&host, &repo_dir, &repo_key_owned, next_gen, final_dir)?;
            Ok(CheckoutRefresh {
                snapshot,
                previous_sha: Some(current.head_sha),
                updated: true,
            })
        })
        .await
    }

    /// Best-effort refresh reading the `origin` URL from the active checkout,
    /// used when upstream changes land (such as after merging a PR).
    ///
    /// # Errors
    /// Fails if git metadata or remote update fails.
    pub async fn refresh_existing(
        &self,
        repo_key: &str,
    ) -> Result<Option<CheckoutRefresh>, RepoCloneError> {
        let Some(remote_url) = self.origin_url(repo_key).await? else {
            return Ok(None);
        };
        Ok(Some(self.refresh(repo_key, &remote_url).await?))
    }

    /// Derive a fresh copy-on-write workspace at `dst` after checking `remote_url`
    /// and advancing the daemon checkout if behind.
    ///
    /// # Errors
    /// Fails when refreshing the checkout or copy-on-write cloning into `dst` fails.
    pub async fn derive_workspace(
        &self,
        repo_key: &str,
        remote_url: &str,
        dst: &Path,
    ) -> Result<CheckoutSnapshot, RepoCloneError> {
        let entry = self.entry_for(repo_key);
        // The snapshot's generation is leased before the lock is released:
        // a generation is pruned only under this lock (or while leased out
        // from under it never), so the copy below cannot race a deletion.
        let guard = entry.lock.lock().await;
        let refresh = self.refresh_locked(repo_key, remote_url, &entry).await?;
        let snapshot = refresh.snapshot;
        let Some(lease) = self.acquire_lease_for_snapshot(repo_key, &snapshot) else {
            // The generation that was just made current is gone: something
            // outside the lease removed it. Fail rather than copy a
            // half-pruned checkout.
            return Err(RepoCloneError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "the checkout generation {} of {repo_key} was removed before it could be leased",
                    snapshot.generation
                ),
            )));
        };
        drop(guard);
        let src_dir = snapshot.generation_dir.clone();
        let dst_buf = dst.to_path_buf();
        let copy_res = blocking::unblock(move || cow_copy_dir(&src_dir, &dst_buf)).await;
        lease.release().await;
        copy_res?;
        Ok(snapshot)
    }

    /// Acquire a reader lease on the currently active generation of `repo_key`,
    /// returning `None` if no generation exists yet.
    pub async fn acquire_lease(&self, repo_key: &str) -> Option<GenerationLease> {
        let repo_dir = self.repo_dir(repo_key);
        let entry = self.entry_for(repo_key);
        // Reading `current` and recording the lease happen under the lock
        // that pruning also takes, so the generation cannot be deleted
        // between the read and the lease.
        let _guard = entry.lock.lock().await;
        let generation_dir = blocking::unblock({
            let repo_dir = repo_dir.clone();
            move || {
                read_current_generation_num_and_dir(&repo_dir)
                    .ok()
                    .flatten()
            }
        })
        .await?;
        let (generation, generation_dir) = generation_dir;
        entry.acquire(generation);
        // Re-verify `current` after recording the lease so we never hold a race
        // against a concurrent prune of a superseded generation.
        let still_current = blocking::unblock({
            let repo_dir = repo_dir.clone();
            move || read_current_generation_num(&repo_dir).ok().flatten()
        })
        .await;
        if still_current != Some(generation) && !generation_dir.exists() {
            let _ = entry.release(generation);
            return None;
        }
        Some(GenerationLease {
            repo_dir,
            generation,
            generation_dir,
            entry: Arc::clone(&entry),
        })
    }

    fn acquire_lease_for_snapshot(
        &self,
        repo_key: &str,
        snapshot: &CheckoutSnapshot,
    ) -> Option<GenerationLease> {
        let entry = self.entry_for(repo_key);
        entry.acquire(snapshot.generation);
        if !snapshot.generation_dir.exists() {
            let _ = entry.release(snapshot.generation);
            return None;
        }
        Some(GenerationLease {
            repo_dir: self.repo_dir(repo_key),
            generation: snapshot.generation,
            generation_dir: snapshot.generation_dir.clone(),
            entry,
        })
    }

    /// Current HEAD SHA of `repo_key`'s active daemon checkout, if present.
    ///
    /// # Errors
    /// Fails when git fails on an existing checkout directory.
    pub async fn current_head(&self, repo_key: &str) -> Result<Option<String>, RepoCloneError> {
        let repo_dir = self.repo_dir(repo_key);
        let host = self.host.clone();
        let repo_key_owned = repo_key.to_string();
        blocking::unblock(move || {
            Ok(read_current_snapshot(&host, &repo_dir, &repo_key_owned)?.map(|snap| snap.head_sha))
        })
        .await
    }

    /// Upstream remote URL configured on `repo_key`'s active checkout, if
    /// present.
    ///
    /// # Errors
    /// Fails when reading remote configuration fails.
    pub async fn origin_url(&self, repo_key: &str) -> Result<Option<String>, RepoCloneError> {
        let current = self.current_link(repo_key);
        let host = self.host.clone();
        blocking::unblock(move || {
            if !current.exists() {
                return Ok(None);
            }
            let out = host
                .output(Some(&current), &["remote", "get-url", "upstream"])
                .or_else(|_| host.output(Some(&current), &["remote", "get-url", "origin"]))
                .map_err(RepoCloneError::from)?;
            Ok(Some(out.trim().to_string()))
        })
        .await
    }

    /// Files changed between `old_sha` and `new_sha` in `repo_key`'s daemon
    /// checkout (`git diff --name-only old..new`).
    ///
    /// # Errors
    /// Fails when the checkout is absent or `git diff` fails.
    pub async fn diff_files(
        &self,
        repo_key: &str,
        old_sha: &str,
        new_sha: &str,
    ) -> Result<Vec<String>, RepoCloneError> {
        if old_sha == new_sha {
            return Ok(Vec::new());
        }
        let Some(lease) = self.acquire_lease(repo_key).await else {
            return Ok(Vec::new());
        };
        let host = self.host.clone();
        let gen_dir = lease.generation_dir.clone();
        let range = format!("{old_sha}..{new_sha}");
        let res = blocking::unblock(move || {
            let output = host
                .output(Some(&gen_dir), &["diff", "--name-only", &range])
                .map_err(RepoCloneError::from)?;
            Ok(output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToString::to_string)
                .collect())
        })
        .await;
        lease.release().await;
        res
    }

    /// Map a host path inside `<checkouts>/<slug>/(current|generations/<n>)/<rel>`
    /// back to `(repo_key, rel_path)`. Returns `None` for paths outside daemon
    /// checkouts or inside `.git` metadata.
    #[must_use]
    pub fn identify_path(&self, path: &Path) -> Option<(String, String)> {
        let rel = strip_root_with_canonical(&self.root, path)?;
        let mut comps = rel.components();
        let Component::Normal(slug) = comps.next()? else {
            return None;
        };
        let repo_dir = self.root.join(slug);
        let key = std::fs::read_to_string(repo_dir.join(REPO_KEY_FILE))
            .ok()?
            .trim()
            .to_string();
        if key.is_empty() {
            return None;
        }
        let after_slug: PathBuf = comps.collect();
        let mut sub = after_slug.components();
        let head = match sub.next()? {
            Component::Normal(c) => c.to_string_lossy(),
            _ => return None,
        };
        let repo_rel: PathBuf = if head == CURRENT_LINK {
            sub.collect()
        } else if head == GENERATIONS_DIR {
            let _gen = sub.next()?;
            sub.collect()
        } else {
            return None;
        };
        let rel_str = normalize_repo_rel(&repo_rel)?;
        if rel_str == ".git" || rel_str.starts_with(".git/") {
            return None;
        }
        Some((key, rel_str))
    }

    /// Number of existing `generations/<n>` directories on disk for `repo_key`
    /// (for inspection & tests).
    ///
    /// # Errors
    /// Fails on I/O errors when listing `generations/`.
    pub fn generation_numbers(&self, repo_key: &str) -> std::io::Result<Vec<u64>> {
        list_generation_numbers(&self.repo_dir(repo_key))
    }
}

/// Copy directory `src` to `dst` using filesystem copy-on-write (`reflink-copy`).
///
/// Uses directory-level `reflink` when supported (such as APFS `clonefile`),
/// falling back to preserving subdirectories/symlinks and `reflink_or_copy` per
/// regular file on filesystems where directory cloning is unsupported.
///
/// # Errors
/// Fails when source entries cannot be read or created at `dst`.
pub fn cow_copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if reflink_copy::reflink(src, dst).is_ok() {
        return Ok(());
    }
    // Clean up any partial target before walking entry-by-entry.
    if dst.exists() {
        let _ = std::fs::remove_dir_all(dst);
    }
    std::fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src) {
        let entry = entry.map_err(std::io::Error::other)?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(std::io::Error::other)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if file_type.is_symlink() {
            let link_target = std::fs::read_link(entry.path())?;
            create_symlink(&link_target, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            reflink_copy::reflink_or_copy(entry.path(), &target)?;
            if let Ok(meta) = std::fs::symlink_metadata(entry.path()) {
                let _ = std::fs::set_permissions(&target, meta.permissions());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link_path)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link_path: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link_path)
    } else {
        std::os::windows::fs::symlink_file(target, link_path)
    }
}

fn write_repo_key_file(repo_dir: &Path, repo_key: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(repo_dir)?;
    let path = repo_dir.join(REPO_KEY_FILE);
    if !path.exists() {
        std::fs::write(path, format!("{repo_key}\n"))?;
    }
    Ok(())
}

fn read_current_generation_num(repo_dir: &Path) -> std::io::Result<Option<u64>> {
    let link = repo_dir.join(CURRENT_LINK);
    let target = match std::fs::read_link(&link) {
        Ok(t) => t,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|s| s.parse::<u64>().ok());
    Ok(name)
}

fn read_current_generation_num_and_dir(repo_dir: &Path) -> std::io::Result<Option<(u64, PathBuf)>> {
    let Some(gen_num) = read_current_generation_num(repo_dir)? else {
        return Ok(None);
    };
    let gen_dir = repo_dir.join(GENERATIONS_DIR).join(gen_num.to_string());
    if !gen_dir.is_dir() {
        return Ok(None);
    }
    Ok(Some((gen_num, gen_dir)))
}

fn read_current_snapshot(
    host: &HostGit,
    repo_dir: &Path,
    repo_key: &str,
) -> Result<Option<CheckoutSnapshot>, RepoCloneError> {
    let Some((gen_num, gen_dir)) = read_current_generation_num_and_dir(repo_dir)? else {
        return Ok(None);
    };
    if !gen_dir.join(".git").exists() {
        return Ok(None);
    }
    snapshot_from_generation(host, repo_dir, repo_key, gen_num, gen_dir).map(Some)
}

fn snapshot_from_generation(
    host: &HostGit,
    repo_dir: &Path,
    repo_key: &str,
    generation: u64,
    generation_dir: PathBuf,
) -> Result<CheckoutSnapshot, RepoCloneError> {
    let head_sha = host
        .output(Some(&generation_dir), &["rev-parse", "HEAD"])
        .map_err(RepoCloneError::from)?
        .trim()
        .to_string();
    let default_branch = host
        .output(Some(&generation_dir), &["symbolic-ref", "--short", "HEAD"])
        .map_or_else(|_| "main".to_string(), |s| s.trim().to_string());
    Ok(CheckoutSnapshot {
        repo_key: repo_key.to_string(),
        root_dir: repo_dir.to_path_buf(),
        current_dir: repo_dir.join(CURRENT_LINK),
        generation_dir,
        generation,
        head_sha,
        default_branch,
    })
}

fn initial_clone(
    host: &HostGit,
    repo_dir: &Path,
    repo_key: &str,
    remote_url: &str,
) -> Result<CheckoutSnapshot, RepoCloneError> {
    let generations_dir = repo_dir.join(GENERATIONS_DIR);
    std::fs::create_dir_all(&generations_dir)?;
    let next_gen = next_generation_number(repo_dir, 0)?;
    let staging = generations_dir.join(format!("staging-{next_gen}-{}", may_protocol::id::new()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let staging_str = staging.to_string_lossy().into_owned();
    if let Err(error) = host.output(
        None,
        &["clone", "--origin", "upstream", remote_url, &staging_str],
    ) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    let final_dir = generations_dir.join(next_gen.to_string());
    if final_dir.exists() {
        std::fs::remove_dir_all(&final_dir)?;
    }
    std::fs::rename(&staging, &final_dir)?;
    swap_current_symlink(repo_dir, next_gen)?;
    snapshot_from_generation(host, repo_dir, repo_key, next_gen, final_dir)
}

#[derive(Debug)]
struct RemoteHead {
    sha: String,
    branch: Option<String>,
}

fn ls_remote_head(host: &HostGit, remote_url: &str) -> Result<RemoteHead, RepoCloneError> {
    let out = host
        .output(None, &["ls-remote", "--symref", remote_url, "HEAD"])
        .map_err(RepoCloneError::from)?;
    let mut sha = None;
    let mut branch = None;
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let Some(first) = parts.next() else {
            continue;
        };
        let Some(second) = parts.next() else {
            continue;
        };
        if first == "ref:" && second.starts_with("refs/heads/") {
            branch = second.strip_prefix("refs/heads/").map(ToString::to_string);
        } else if parts.next().is_none() && second == "HEAD" {
            sha = Some(first.to_string());
        }
    }
    let sha = sha.ok_or_else(|| RepoCloneError::Git {
        args: format!("ls-remote --symref {remote_url} HEAD"),
        stderr: format!("remote `{remote_url}` has no HEAD commit"),
    })?;
    Ok(RemoteHead { sha, branch })
}

fn update_generation_cleanly(
    host: &HostGit,
    dir: &Path,
    remote_url: &str,
    default_branch: &str,
    target_sha: &str,
) -> Result<(), RepoCloneError> {
    if host
        .output(Some(dir), &["remote", "set-url", "upstream", remote_url])
        .is_err()
    {
        host.output(Some(dir), &["remote", "add", "upstream", remote_url])
            .map_err(RepoCloneError::from)?;
    }
    host.output(Some(dir), &["fetch", "--prune", "upstream"])
        .map_err(RepoCloneError::from)?;
    let _ = host.output(Some(dir), &["remote", "set-head", "upstream", "--auto"]);
    host.output(Some(dir), &["checkout", "-B", default_branch, target_sha])
        .map_err(RepoCloneError::from)?;
    host.output(Some(dir), &["clean", "-fdx"])
        .map_err(RepoCloneError::from)?;
    Ok(())
}

fn swap_current_symlink(repo_dir: &Path, next_gen: u64) -> std::io::Result<()> {
    let rel_target = PathBuf::from(GENERATIONS_DIR).join(next_gen.to_string());
    let tmp_link = repo_dir.join(format!("current.tmp.{}", may_protocol::id::new()));
    if tmp_link.exists() {
        let _ = std::fs::remove_file(&tmp_link);
    }
    create_symlink(&rel_target, &tmp_link)?;
    std::fs::rename(&tmp_link, repo_dir.join(CURRENT_LINK))?;
    Ok(())
}

fn list_generation_numbers(repo_dir: &Path) -> std::io::Result<Vec<u64>> {
    let dir = repo_dir.join(GENERATIONS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut nums = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
            && let Ok(num) = name.parse::<u64>()
        {
            nums.push(num);
        }
    }
    nums.sort_unstable();
    Ok(nums)
}

fn next_generation_number(repo_dir: &Path, current_gen: u64) -> std::io::Result<u64> {
    let max_on_disk = list_generation_numbers(repo_dir)?
        .into_iter()
        .max()
        .unwrap_or(0);
    Ok(max_on_disk.max(current_gen) + 1)
}

fn prune_stale_generations(repo_dir: &Path, entry: &RepoEntry, current_gen: u64) {
    let Ok(nums) = list_generation_numbers(repo_dir) else {
        return;
    };
    for num in nums {
        if num == current_gen || entry.is_leased(num) {
            continue;
        }
        let _ = std::fs::remove_dir_all(repo_dir.join(GENERATIONS_DIR).join(num.to_string()));
    }
}

fn prune_generation_if_unreferenced(repo_dir: &Path, entry: &RepoEntry, generation: u64) {
    if read_current_generation_num(repo_dir).ok().flatten() == Some(generation) {
        return;
    }
    if entry.is_leased(generation) {
        return;
    }
    let _ = std::fs::remove_dir_all(repo_dir.join(GENERATIONS_DIR).join(generation.to_string()));
}

fn strip_root_with_canonical(root: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(stripped) = path.strip_prefix(root) {
        return Some(stripped.to_path_buf());
    }
    let canon_root = std::fs::canonicalize(root).ok()?;
    if let Ok(stripped) = path.strip_prefix(&canon_root) {
        return Some(stripped.to_path_buf());
    }
    let canon_path = std::fs::canonicalize(path).ok()?;
    canon_path
        .strip_prefix(&canon_root)
        .ok()
        .map(Path::to_path_buf)
}

fn normalize_repo_rel(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(seg) => parts.push(seg.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

impl may_command_policy::CheckoutLease for GenerationLease {
    async fn release(self) {
        Self::release(self).await;
    }
}

impl may_command_policy::CheckoutRegistry for DaemonCheckouts {
    type Lease = GenerationLease;

    fn identify_path(&self, path: &Path) -> Option<(String, String)> {
        Self::identify_path(self, path)
    }

    async fn acquire_lease(&self, repo_key: &str) -> Option<GenerationLease> {
        Self::acquire_lease(self, repo_key).await
    }
}
