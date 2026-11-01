//! `pr` — the session's landing object.
//!
//! A session's PR is a numbered, archived record of work landing from its
//! sandbox clone into the repository. The may backend merges into the
//! checkout through a serialized, preflight-checked land; conflicts reject
//! the PR and the session rebases inside its clone and retries.
//!
//! Ownership: writes are bound to the session that opened the PR.
//! One open PR per (session, repository) — the clone dies on merge, so a
//! second open PR on the same clone would lose its base.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use crate::db::ChatDb;
use crate::repo_clone::{CloneRecord, SessionRepos, uncommitted_changes};

/// Error type for `pr` operations.
#[derive(Debug, thiserror::Error)]
pub enum PrError {
    /// Database layer failure.
    #[error(transparent)]
    Db(#[from] crate::db::ChatDbError),
    /// Clone layer failure.
    #[error(transparent)]
    RepoClone(#[from] crate::repo_clone::RepoCloneError),
    /// The repo could not be resolved to a session clone.
    #[error("{0}")]
    Resolve(String),
    /// The PR is owned by another session or has no write path here.
    #[error("{0}")]
    Ownership(String),
    /// The PR is in a state that refuses the operation.
    #[error("{0}")]
    State(String),
    /// Merge conflicts or dirty checkout overlap — the file list.
    #[error("rejected: {0}")]
    Rejected(String),
    /// The clone holds uncommitted or untracked work — nothing is lost silently.
    #[error("uncommitted changes would be lost on merge: {0}")]
    DirtyClone(String),
    /// Local check command failed.
    #[error("check failed: {0}")]
    CheckFailed(String),
}

/// `pr`-side workspace fields the land path reads.
#[derive(Debug, Clone)]
pub struct PrContext {
    /// History DB handle — PRs live without it not at all.
    pub db: Option<ChatDb>,
    /// Session these writes belong to.
    pub session_id: String,
    /// Session sandbox root — clones live under `repos/` inside it.
    pub session_root: PathBuf,
    /// Session cwd — the repo `pr` falls back to outside a clone.
    pub cwd: PathBuf,
    /// Workspace config — `[[projects]] checks` are read off `projects`.
    pub workspace_config: crate::workspace::WorkspaceConfig,
}

/// Serialized per-repo merge: one in-flight land per repository.
static LAND_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn land_lock(repo_key: &str) -> Arc<Mutex<()>> {
    let mut locks = LAND_LOCKS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    locks
        .entry(repo_key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// The resolved clone a `pr` operates on.
struct Resolved {
    record: CloneRecord,
    dir: PathBuf,
}

/// May-backend `pr` engine for one session.
pub struct PrStore {
    context: PrContext,
    repos: SessionRepos,
}

impl std::fmt::Debug for PrStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrStore").finish_non_exhaustive()
    }
}

impl PrStore {
    /// Build the engine for one session.
    #[must_use]
    pub fn new(context: PrContext) -> Self {
        let repos = SessionRepos::new(&context.session_root, context.session_id.clone());
        Self { context, repos }
    }

    fn db(&self) -> Result<&ChatDb, PrError> {
        self.context
            .db
            .as_ref()
            .ok_or_else(|| PrError::Resolve("pr requires the history database".to_string()))
    }

    /// Which repo does `pr` mean: `--repo` wins, then the session's only
    /// clone, then the cwd's repository (cloned on demand).
    async fn resolve(&self, repo: Option<&str>) -> Result<Resolved, PrError> {
        let clones = self.repos.clones()?;
        if let Some(name) = repo {
            if let Some(record) = clones
                .into_iter()
                .find(|c| c.repo_key == name || c.dir == name)
            {
                return Ok(Resolved {
                    dir: self.repos.root().join(&record.dir),
                    record,
                });
            }
            return Err(PrError::Resolve(format!("no session clone named {name}")));
        }
        if let [record] = clones.as_slice() {
            return Ok(Resolved {
                dir: self.repos.root().join(&record.dir),
                record: record.clone(),
            });
        }
        if clones.len() > 1 {
            return Err(PrError::Resolve(
                "several session clones exist; pass --repo".to_string(),
            ));
        }
        // No clones yet: clone the cwd's repo on demand (local-bound).
        let worktree = crate::repo_search::worktree_root(&self.context.cwd)
            .ok_or_else(|| PrError::Resolve("not inside a repository".to_string()))?;
        let repo_key = crate::issue::repository_identity(&worktree)
            .map_err(|e| PrError::Resolve(format!("repository identity: {e}")))?;
        let dir = self.repos.ensure_local_clone(&worktree, &repo_key).await?;
        let record = self
            .repos
            .clones()?
            .into_iter()
            .find(|c| self.repos.root().join(&c.dir) == dir)
            .ok_or_else(|| PrError::Resolve("clone registry lost the new clone".to_string()))?;
        Ok(Resolved { record, dir })
    }

    /// `pr open` — record the session's PR; with `auto_merge` it runs the
    /// land immediately, with `--hold` it stops at `open`.
    ///
    /// # Errors
    /// Fails when the repo cannot be resolved, a PR is already open, the DB
    /// is unavailable, or the auto-merge run rejects.
    pub async fn open(
        &self,
        repo: Option<&str>,
        title: &str,
        issue_number: Option<i64>,
        hold: bool,
    ) -> Result<String, PrError> {
        let resolved = self.resolve(repo).await?;
        if self
            .db()?
            .open_pr_for(&resolved.record.repo_key, &self.context.session_id)
            .await?
            .is_some()
        {
            return Err(PrError::State(
                "this session already has an open PR on this repo — merge or close it first"
                    .to_string(),
            ));
        }
        let dirty = uncommitted_changes(&resolved.dir).await?;
        if !dirty.is_empty() {
            return Err(PrError::DirtyClone(dirty.join(", ")));
        }
        if resolved.record.local
            && self.head_sha(&resolved.dir).await? == self.base_sha(&resolved).await?
        {
            return Err(PrError::State("no committed work in the clone".to_string()));
        }
        let base_sha = self.base_sha(&resolved).await?;
        let branch = format!("refs/may/{}", self.context.session_id);
        let pr = self
            .db()?
            .create_pr(
                &resolved.record.repo_key,
                &self.context.session_id,
                title,
                "",
                &branch,
                &base_sha,
                "may",
                !hold,
                issue_number,
            )
            .await?;
        self.event(&pr, "opened", Some(&format!("base {base_sha}")))
            .await;
        if let Some(n) = issue_number {
            self.event(&pr, "issue_linked", Some(&format!("issue #{n}")))
                .await;
        }
        if hold {
            return Ok(format!("opened PR #{} (hold)", pr.number));
        }
        self.merge(pr.number, repo).await
    }

    /// `pr merge <n>` — checks, preflight, serialized land, cleanup. Also
    /// the retry path after a rejection.
    ///
    /// # Errors
    /// Fails on ownership, state, checks, or a rejected merge.
    pub async fn merge(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let resolved = self.resolve(repo).await?;
        let pr = self
            .db()?
            .get_pr(&resolved.record.repo_key, number)
            .await?
            .ok_or_else(|| PrError::State(format!("no PR #{number}")))?;
        if pr.session_id != self.context.session_id {
            return Err(PrError::Ownership(format!(
                "PR #{number} belongs to another session"
            )));
        }
        match pr.state.as_str() {
            "merged" | "closed" => {
                return Err(PrError::State(format!("PR #{number} is {}", pr.state)));
            }
            _ => {}
        }
        let dirty = uncommitted_changes(&resolved.dir).await?;
        if !dirty.is_empty() {
            return Err(PrError::DirtyClone(dirty.join(", ")));
        }
        let head_sha = self.head_sha(&resolved.dir).await?;
        self.db()?
            .update_pr(pr.id, "checks_pending", Some(&head_sha), None, None)
            .await?;
        self.event(&pr, "checks_pending", None).await;
        // `[[projects]] checks` — the local check command runs in the clone.
        if let Some(cmd) = self.check_command(&resolved.record) {
            if let Err(err) = run_check(&resolved.dir, &cmd).await {
                self.db()?
                    .update_pr(pr.id, "failed", Some(&head_sha), None, None)
                    .await?;
                self.event(&pr, "check_failed", Some(&err)).await;
                return Err(PrError::CheckFailed(err));
            }
        }
        // Serialized land: one in-flight merge per repository.
        let lock = land_lock(&resolved.record.repo_key);
        self.event(&pr, "merging", None).await;
        let result = blocking::unblock({
            let checkout = resolved.record.source.clone();
            let session = self.context.session_id.clone();
            let clone = resolved.dir.clone();
            let number = pr.number;
            move || land(&checkout, &clone, &session, number, lock)
        })
        .await;
        let now = jiff::Timestamp::now().to_string();
        match result {
            Ok(merge_sha) => {
                self.db()?
                    .update_pr(
                        pr.id,
                        "merged",
                        Some(&head_sha),
                        Some(&merge_sha),
                        Some(&now),
                    )
                    .await?;
                self.event(&pr, "merged", Some(&merge_sha)).await;
                self.repos.delete_clone(&resolved.dir).await?;
                Ok(format!("merged PR #{number} as {merge_sha}"))
            }
            Err(reason) => {
                self.db()?
                    .update_pr(pr.id, "rejected", Some(&head_sha), None, None)
                    .await?;
                self.event(&pr, "rejected", Some(&reason)).await;
                Err(PrError::Rejected(reason))
            }
        }
    }

    /// `pr close <n>` — abandon an own, unmerged PR.
    ///
    /// # Errors
    /// Fails on ownership or a terminal state.
    pub async fn close(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let resolved = self.resolve(repo).await?;
        let pr = self
            .db()?
            .get_pr(&resolved.record.repo_key, number)
            .await?
            .ok_or_else(|| PrError::State(format!("no PR #{number}")))?;
        if pr.session_id != self.context.session_id {
            return Err(PrError::Ownership(format!(
                "PR #{number} belongs to another session"
            )));
        }
        self.db()?
            .update_pr(pr.id, "closed", None, None, None)
            .await?;
        self.event(&pr, "closed", None).await;
        Ok(format!("closed PR #{number}"))
    }

    /// `pr list` — every PR on the repo (or all repos with `--repo all`-less
    /// call), newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list(&self, repo: Option<&str>, state: Option<&str>) -> Result<String, PrError> {
        let key = match repo {
            Some(name) => {
                let resolved = self.resolve(Some(name)).await?;
                Some(resolved.record.repo_key)
            }
            None => {
                let clones = self.repos.clones()?;
                if clones.len() == 1 {
                    Some(clones[0].repo_key.clone())
                } else {
                    None
                }
            }
        };
        let rows = self.db()?.list_prs(key.as_deref(), state).await?;
        if rows.is_empty() {
            return Ok("no PRs".to_string());
        }
        let mut out = String::new();
        for pr in &rows {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "PR #{} [{}] {}{} (session {})\n",
                    pr.number,
                    pr.state,
                    pr.title,
                    pr.merge_commit_sha
                        .as_deref()
                        .map(|s| format!(" → {s}"))
                        .unwrap_or_default(),
                    pr.session_id
                ),
            );
        }
        Ok(out.trim_end().to_string())
    }

    /// `pr show <n>` — one PR with its event trail.
    ///
    /// # Errors
    /// Fails when the PR does not exist.
    pub async fn show(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let resolved = self.resolve(repo).await?;
        let pr = self
            .db()?
            .get_pr(&resolved.record.repo_key, number)
            .await?
            .ok_or_else(|| PrError::State(format!("no PR #{number}")))?;
        let events = self.db()?.pr_events(pr.id).await?;
        let mut out = format!(
            "PR #{} [{}] {}\nsession {} · backend {} · base {} · head {}",
            pr.number,
            pr.state,
            pr.title,
            pr.session_id,
            pr.backend,
            pr.base_sha,
            pr.head_sha.as_deref().unwrap_or("-")
        );
        if let Some(sha) = &pr.merge_commit_sha {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\nmerge {sha}"));
        }
        for e in events {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "\n  {} {} {}{}",
                    e.created_at,
                    e.kind,
                    e.agent_id,
                    e.detail.map(|d| format!(" — {d}")).unwrap_or_default()
                ),
            );
        }
        Ok(out)
    }

    fn check_command(&self, record: &CloneRecord) -> Option<String> {
        self.context
            .workspace_config
            .projects
            .iter()
            .find(|p| {
                crate::issue::repository_identity(&p.path)
                    .map(|k| k.to_string_lossy() == record.repo_key)
                    .unwrap_or(false)
            })
            .and_then(|p| p.checks.clone())
    }

    async fn base_sha(&self, resolved: &Resolved) -> Result<String, PrError> {
        if resolved.record.local {
            blocking::unblock({
                let src = resolved.record.source.clone();
                move || git(Some(Path::new(&src)), &["rev-parse", "HEAD"])
            })
            .await
            .map_err(PrError::RepoClone)
        } else {
            blocking::unblock({
                let dir = resolved.dir.clone();
                move || git(Some(&dir), &["rev-parse", "upstream/HEAD"])
            })
            .await
            .map_err(PrError::RepoClone)
        }
    }

    async fn head_sha(&self, dir: &Path) -> Result<String, PrError> {
        blocking::unblock({
            let dir = dir.to_path_buf();
            move || git(Some(&dir), &["rev-parse", "HEAD"])
        })
        .await
        .map_err(PrError::RepoClone)
    }

    async fn event(&self, pr: &crate::db::PrRow, kind: &str, detail: Option<&str>) {
        if let Some(db) = self.context.db.as_ref() {
            let _ = db
                .add_pr_event(pr.id, kind, &self.context.session_id, "main", None, detail)
                .await;
        }
    }
}

/// The serialized land: fetch the session branch, merge-tree preflight,
/// checkout-overlap check, merge. Runs inside `blocking::unblock` under the
/// per-repo lock.
fn land(
    checkout: &str,
    clone: &Path,
    session: &str,
    number: i64,
    lock: Arc<Mutex<()>>,
) -> Result<String, String> {
    let _guard = lock.lock().map_err(|_| "land lock poisoned".to_string())?;
    let checkout_path = Path::new(checkout);
    let refname = format!("refs/may/{session}");
    git(
        Some(checkout_path),
        &[
            "fetch",
            &clone.to_string_lossy(),
            &format!("HEAD:{refname}"),
        ],
    )
    .map_err(|e| e.to_string())?;
    let head = git(Some(checkout_path), &["rev-parse", "HEAD"]).map_err(|e| e.to_string())?;
    // merge-tree preflight (git ≥ 2.38): a conflict exits nonzero with the
    // conflicted paths after the tree oid.
    match git(
        Some(checkout_path),
        &["merge-tree", "--write-tree", "--name-only", &head, &refname],
    ) {
        Ok(_) => {}
        Err(err) => return Err(format!("merge conflicts: {err}")),
    }
    // Do not overwrite the user's uncommitted files: dirty checkout paths
    // must not intersect the PR's touched paths.
    let dirty = git(Some(checkout_path), &["status", "--porcelain"])
        .map_err(|e| e.to_string())?
        .lines()
        .map(|l| l[3..].trim().to_string())
        .collect::<std::collections::HashSet<_>>();
    let touched = git(
        Some(checkout_path),
        &["diff", "--name-only", &format!("{head}..{refname}")],
    )
    .map_err(|e| e.to_string())?
    .lines()
    .map(str::to_string)
    .collect::<Vec<_>>();
    let overlap: Vec<String> = touched.into_iter().filter(|f| dirty.contains(f)).collect();
    if !overlap.is_empty() {
        return Err(format!(
            "checkout has uncommitted changes in PR paths: {}",
            overlap.join(", ")
        ));
    }
    git(
        Some(checkout_path),
        &[
            "merge",
            "--no-ff",
            "-m",
            &format!("Merge may PR {number}"),
            &refname,
        ],
    )
    .map_err(|e| e.to_string())?;
    git(Some(checkout_path), &["rev-parse", "HEAD"]).map_err(|e| e.to_string())
}

async fn run_check(dir: &Path, cmd: &str) -> Result<(), String> {
    let dir = dir.to_path_buf();
    let cmd = cmd.to_string();
    blocking::unblock(move || {
        let output = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .current_dir(&dir)
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(2000)
                .collect())
        }
    })
    .await
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, crate::repo_clone::RepoCloneError> {
    let mut command = std::process::Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command.args(args).output()?;
    if !output.status.success() {
        return Err(crate::repo_clone::RepoCloneError::Git {
            args: format!("{command:?}"),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
