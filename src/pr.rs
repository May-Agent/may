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
    /// The outbound screen blocked published text.
    #[error("outbound screen blocked: {0}")]
    Screened(String),
    /// A GitHub CLI call failed.
    #[error("gh {0}")]
    Gh(String),
}

/// `pr`-side workspace fields the land path reads.
#[derive(Clone)]
pub struct PrContext {
    /// History DB handle — PRs live without it not at all.
    pub db: Option<ChatDb>,
    /// Session these writes belong to.
    pub session_id: String,
    /// Session sandbox root — clones live under `repos/` inside it.
    pub session_root: PathBuf,
    /// Session cwd — the repo `pr` falls back to outside a clone.
    pub cwd: PathBuf,
    /// Workspace config — `[[projects]]` backends and checks.
    pub workspace_config: crate::workspace::WorkspaceConfig,
    /// Outbound screen: `Ok(true)` publishes, anything else blocks. Wired
    /// to the fast model by the command registration.
    pub screen: Option<std::sync::Arc<ScreenFn>>,
}

/// Screen callback signature: `(title, body)` → publish verdict.
pub type ScreenFn = dyn Fn(
        &str,
        &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send>>
    + Send
    + Sync;

impl std::fmt::Debug for PrContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrContext")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// Serialized per-repo merge: one in-flight land per repository.
static LAND_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn land_lock(repo_key: &str) -> Arc<Mutex<()>> {
    let mut locks = LAND_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.entry(repo_key.to_string()).or_default().clone()
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
        // No clones yet: clone the cwd's repo on demand. A project pinned
        // to the GitHub backend binds `owner/repo` and clones the remote —
        // the cloud is the source of truth and no checkout is required.
        let worktree = crate::repo_search::worktree_root(&self.context.cwd)
            .ok_or_else(|| PrError::Resolve("not inside a repository".to_string()))?;
        if self
            .project_for_path(&worktree)
            .and_then(|p| p.pr.backend.as_deref())
            == Some("github")
        {
            let url = blocking::unblock({
                let wt = worktree.clone();
                move || git(Some(&wt), &["remote", "get-url", "origin"])
            })
            .await?;
            let Some(owner_repo) = github_owner_repo(&url) else {
                return Err(PrError::Resolve(format!(
                    "pr.backend is github but origin is not GitHub: {url}"
                )));
            };
            let dir = self.repos.ensure_remote_clone(&url, &owner_repo).await?;
            let record = self
                .repos
                .clones()?
                .into_iter()
                .find(|c| self.repos.root().join(&c.dir) == dir)
                .ok_or_else(|| PrError::Resolve("clone registry lost the new clone".to_string()))?;
            return Ok(Resolved { record, dir });
        }
        let repo_key = crate::issue::repository_identity(&worktree)
            .map_err(|e| PrError::Resolve(format!("repository identity: {e}")))?;
        // First-contact gate: no binding before a persisted trust decision.
        if crate::trust::trust_for(&self.context.workspace_config, &repo_key, &worktree).is_none() {
            return Err(PrError::Resolve(
                "repository trust undecided — answer the repo-trust question, or run `repo trust set <full|prs-only|local>`".to_string(),
            ));
        }
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
        let (backend, github_repo) = Self::backend_for(&resolved.record);
        let branch = if resolved.record.local {
            format!("refs/may/{}", self.context.session_id)
        } else {
            format!("may/{}", self.context.session_id)
        };
        // The outbound screen vets every published text before it leaves —
        // the "PRs only" trust mode's guard.
        if backend == "github"
            && let Some(reason) = self.screen_outbound(&resolved.record, title).await?
        {
            return Err(PrError::Screened(reason));
        }
        // GitHub-bound: push the session branch before recording, so the
        // row never points at a PR that failed to exist upstream.
        let external_number = if backend == "github" {
            let owner_repo = github_repo
                .as_deref()
                .ok_or_else(|| PrError::Resolve("github backend without a repo".to_string()))?;
            Some(
                self.push_and_create(&resolved, &branch, title, owner_repo)
                    .await?,
            )
        } else {
            None
        };
        let pr = self
            .db()?
            .create_pr(
                &resolved.record.repo_key,
                &self.context.session_id,
                title,
                "",
                &branch,
                &base_sha,
                backend,
                !hold,
                issue_number,
                external_number,
            )
            .await?;
        self.event(&pr, "opened", Some(&format!("base {base_sha}")))
            .await;
        if let Some(n) = external_number {
            self.event(&pr, "opened", Some(&format!("github PR #{n}")))
                .await;
        }
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
                "PR #{number} belongs to another session — adopt it with `pr adopt {number}`"
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
        if pr.backend == "github" {
            return self.merge_github(&resolved, &pr, &head_sha).await;
        }
        // `[[projects]] checks` — the local check command runs in the clone.
        if let Some(cmd) = self.check_command(&resolved.record)
            && let Err(err) = run_check(&resolved.dir, &cmd).await
        {
            self.db()?
                .update_pr(pr.id, "failed", Some(&head_sha), None, None)
                .await?;
            self.event(&pr, "check_failed", Some(&err)).await;
            return Err(PrError::CheckFailed(err));
        }
        // Serialized land: one in-flight merge per repository.
        let lock = land_lock(&resolved.record.repo_key);
        self.event(&pr, "merging", None).await;
        let result = blocking::unblock({
            let checkout = resolved.record.source.clone();
            let session = self.context.session_id.clone();
            let clone = resolved.dir.clone();
            let number = pr.number;
            move || land(&checkout, &clone, &session, number, &lock)
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
                "PR #{number} belongs to another session — adopt it with `pr adopt {number}`"
            )));
        }
        self.db()?
            .update_pr(pr.id, "closed", None, None, None)
            .await?;
        self.event(&pr, "closed", None).await;
        Ok(format!("closed PR #{number}"))
    }

    /// Take over a PR whose owning session is gone — or still another's.
    /// Adoption transfers `session_id` so this session may merge or close
    /// it, and the event log records the handover.
    pub async fn adopt(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let resolved = self.resolve(repo).await?;
        let pr = self
            .db()?
            .get_pr(&resolved.record.repo_key, number)
            .await?
            .ok_or_else(|| PrError::State(format!("no PR #{number}")))?;
        if pr.session_id == self.context.session_id {
            return Ok(format!("PR #{number} is already this session's"));
        }
        let previous = pr.session_id.clone();
        self.db()?.adopt_pr(pr.id, &self.context.session_id).await?;
        let detail = format!("from session {previous}");
        self.event(&pr, "adopted", Some(&detail)).await;
        Ok(format!("adopted PR #{number} (was session {previous})"))
    }

    /// `pr list` — every PR on the repo (or all repos with `--repo all`-less
    /// call), newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list(&self, repo: Option<&str>, state: Option<&str>) -> Result<String, PrError> {
        let key = if let Some(name) = repo {
            let resolved = self.resolve(Some(name)).await?;
            Some(resolved.record.repo_key)
        } else {
            let clones = self.repos.clones()?;
            if clones.len() == 1 {
                Some(clones[0].repo_key.clone())
            } else {
                None
            }
        };
        let rows = self.db()?.list_prs(key.as_deref(), state).await?;
        if rows.is_empty() {
            return Ok("no PRs".to_string());
        }
        // A PR whose owning session was deleted shows an `orphaned` marker —
        // another session adopts it with `pr adopt` before merging.
        let mut dead = std::collections::HashSet::<&str>::new();
        for pr in &rows {
            if dead.contains(pr.session_id.as_str()) {
                continue;
            }
            if self.db()?.get_session(&pr.session_id).await?.is_none() {
                dead.insert(&pr.session_id);
            }
        }
        let mut out = String::new();
        for pr in &rows {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "PR #{} [{}] {}{}{} (session {})\n",
                    pr.number,
                    pr.state,
                    pr.title,
                    pr.merge_commit_sha
                        .as_deref()
                        .map(|s| format!(" → {s}"))
                        .unwrap_or_default(),
                    if dead.contains(pr.session_id.as_str()) {
                        " — orphaned"
                    } else {
                        ""
                    },
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
        let orphaned = self.db()?.get_session(&pr.session_id).await?.is_none();
        let mut out = format!(
            "PR #{} [{}] {}\nsession {}{} · backend {} · base {} · head {}",
            pr.number,
            pr.state,
            pr.title,
            pr.session_id,
            if orphaned { " (orphaned)" } else { "" },
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

    /// Which backend lands this record: local-bound clones merge into the
    /// checkout through the may backend; remote clones publish through
    /// GitHub. `[[projects]] pr.backend = "may"` pins a remote clone to
    /// local landing — rejected, there is no checkout to land into.
    fn backend_for(record: &CloneRecord) -> (&'static str, Option<String>) {
        if record.local {
            return ("may", None);
        }
        github_owner_repo(&record.source).map_or(("may", None), |repo| ("github", Some(repo)))
    }

    /// The `[[projects]]` entry covering this record, by repository
    /// identity for local clones and by `owner/repo` for remote ones.
    fn project_for_record(&self, record: &CloneRecord) -> Option<&crate::workspace::ProjectConfig> {
        self.context.workspace_config.projects.iter().find(|p| {
            if record.local {
                crate::issue::repository_identity(&p.path).is_ok_and(|k| k == record.repo_key)
            } else {
                github_owner_repo(&record.source)
                    == p.github_repository()
                        .map(|r| format!("{}/{}", r.owner(), r.repo()))
            }
        })
    }

    /// The `[[projects]]` entry covering a filesystem path.
    fn project_for_path(&self, path: &Path) -> Option<&crate::workspace::ProjectConfig> {
        let key = crate::issue::repository_identity(path).ok()?;
        self.context
            .workspace_config
            .projects
            .iter()
            .find(|p| crate::issue::repository_identity(&p.path).is_ok_and(|k| k == key))
    }

    /// Run the outbound screen when the project enables it. `Some(reason)`
    /// means the published text was blocked.
    async fn screen_outbound(
        &self,
        record: &CloneRecord,
        title: &str,
    ) -> Result<Option<String>, PrError> {
        if !self
            .project_for_record(record)
            .and_then(|p| p.pr.screen)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let Some(screen) = self.context.screen.as_ref() else {
            tracing::warn!("pr.screen is set but no screen is wired");
            return Ok(None);
        };
        Ok(match screen(title, "").await {
            Ok(true) => None,
            Ok(false) => Some("withheld by the outbound screen".to_string()),
            Err(reason) => Some(reason),
        })
    }

    /// Push the session branch to the GitHub remote and open a real PR.
    /// Returns the upstream PR number.
    async fn push_and_create(
        &self,
        resolved: &Resolved,
        branch: &str,
        title: &str,
        owner_repo: &str,
    ) -> Result<i64, PrError> {
        blocking::unblock({
            let dir = resolved.dir.clone();
            let branch = branch.to_string();
            move || {
                git(
                    Some(&dir),
                    &["push", "upstream", &format!("HEAD:refs/heads/{branch}")],
                )
            }
        })
        .await
        .map_err(|e| PrError::Gh(e.to_string()))?;
        let url = blocking::unblock({
            let owner_repo = owner_repo.to_string();
            let branch = branch.to_string();
            let title = title.to_string();
            move || {
                gh(&[
                    "pr",
                    "create",
                    "--repo",
                    &owner_repo,
                    "--head",
                    &branch,
                    "--title",
                    &title,
                    "--body",
                    "",
                ])
            }
        })
        .await?;
        url.rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| PrError::Gh(format!("unexpected gh pr create output: {url}")))
    }

    /// GitHub landing: push the branch, `gh pr merge --auto`, watch until
    /// the upstream state is terminal, then best-effort pull-sync the
    /// recorded checkout and delete the clone.
    async fn merge_github(
        &self,
        resolved: &Resolved,
        pr: &crate::db::PrRow,
        head_sha: &str,
    ) -> Result<String, PrError> {
        let owner_repo = github_owner_repo(&resolved.record.source)
            .ok_or_else(|| PrError::Resolve("github PR without a repo".to_string()))?;
        let external = pr
            .external_number
            .ok_or_else(|| PrError::State("github PR missing its number".to_string()))?;
        let lock = land_lock(&resolved.record.repo_key);
        self.event(pr, "queued", None).await;
        // Serialized land: push + queue + watch run inside one blocking
        // call so the mutex guard never crosses an await.
        let state = blocking::unblock({
            let clone = resolved.dir.clone();
            let branch = pr.branch.clone();
            let owner_repo = owner_repo.clone();
            move || -> Result<String, String> {
                let _guard = lock.lock().map_err(|_| "land lock poisoned".to_string())?;
                git(
                    Some(&clone),
                    &["push", "upstream", &format!("HEAD:refs/heads/{branch}")],
                )
                .map_err(|e| e.to_string())?;
                gh(&[
                    "pr",
                    "merge",
                    &external.to_string(),
                    "--repo",
                    &owner_repo,
                    "--auto",
                    "--squash",
                ])
                .map_err(|e| e.to_string())?;
                // Watch upstream until terminal: merged, closed, or timeout.
                for _ in 0..60 {
                    let out = gh(&[
                        "pr",
                        "view",
                        &external.to_string(),
                        "--repo",
                        &owner_repo,
                        "--json",
                        "state",
                    ])
                    .map_err(|e| e.to_string())?;
                    if let Some(state) = out
                        .split('"')
                        .nth(3)
                        .map(str::to_string)
                        .filter(|s| s != "OPEN")
                    {
                        return Ok(state);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(10));
                }
                Err("still open after watching".to_string())
            }
        })
        .await;
        let now = jiff::Timestamp::now().to_string();
        match state.as_deref() {
            Ok("MERGED") => {
                self.db()?
                    .update_pr(pr.id, "merged", Some(head_sha), None, Some(now.as_str()))
                    .await?;
                self.event(pr, "merged", Some(head_sha)).await;
                self.pull_sync(&owner_repo).await;
                self.repos.delete_clone(&resolved.dir).await?;
                Ok(format!("merged PR #{} upstream", pr.number))
            }
            Ok(other) => {
                self.db()?
                    .update_pr(pr.id, "closed", Some(head_sha), None, None)
                    .await?;
                self.event(pr, "closed", Some(other)).await;
                Err(PrError::State(format!("upstream PR {other}")))
            }
            Err(reason) => {
                self.db()?
                    .update_pr(pr.id, "failed", Some(head_sha), None, None)
                    .await?;
                self.event(pr, "check_failed", Some(reason)).await;
                Err(PrError::Gh(reason.clone()))
            }
        }
    }

    /// Best-effort `git pull --ff-only` on the cwd's checkout when it
    /// tracks the merged remote — a sync target, never a blocker.
    async fn pull_sync(&self, owner_repo: &str) {
        let Some(worktree) = crate::repo_search::worktree_root(&self.context.cwd) else {
            return;
        };
        let owner_repo = owner_repo.to_string();
        let synced = blocking::unblock(move || {
            let url = git(Some(&worktree), &["remote", "get-url", "origin"]).ok()?;
            if github_owner_repo(&url).as_deref() != Some(owner_repo.as_str()) {
                return None;
            }
            git(Some(&worktree), &["pull", "--ff-only"]).ok()
        })
        .await;
        if synced.is_none() {
            tracing::warn!("post-merge pull sync skipped or failed");
        }
    }

    fn check_command(&self, record: &CloneRecord) -> Option<String> {
        self.context
            .workspace_config
            .projects
            .iter()
            .find(|p| {
                crate::issue::repository_identity(&p.path)
                    .is_ok_and(|k| k.to_string_lossy() == record.repo_key)
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
    lock: &Mutex<()>,
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
    .collect::<std::collections::HashSet<_>>();
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

/// `gh` with the same error contract as [`git`].
fn gh(args: &[&str]) -> Result<String, PrError> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| PrError::Gh(e.to_string()))?;
    if !output.status.success() {
        return Err(PrError::Gh(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `owner/repo` from a GitHub remote URL (https or ssh), or `None`.
fn github_owner_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(".git");
    let tail = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = tail.split('/');
    Some(format!("{}/{}", parts.next()?, parts.next()?))
}

/// Screen published text through the fast model: `true` publishes. The
/// registration site wraps this in the session's screen closure.
///
/// # Errors
/// Returns the model's error as a string when the call fails.
pub async fn screen_with_llm<LLM: aither_core::LanguageModel>(
    llm: &LLM,
    title: &str,
    body: &str,
) -> Result<bool, String> {
    use aither_core::llm::{Event, oneshot};
    use futures_lite::StreamExt;
    let input = format!("TITLE: {title}\n\n{body}");
    let mut stream = Box::pin(llm.respond(oneshot(SCREEN_PROMPT, &input)));
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Text(chunk)) => text.push_str(&chunk),
            Ok(_) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(!text.trim_start().to_ascii_uppercase().starts_with("BLOCK"))
}

const SCREEN_PROMPT: &str = "You screen pull request text before it is published to GitHub. \
The project tracks issues and plans in an internal tool — published text must not mention \
'may', internal issue numbers, agent or session details, or local filesystem paths. \
Reply with exactly ALLOW if the text is safe to publish, or BLOCK: <one-line reason>.";

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

#[cfg(test)]
mod tests {
    use super::github_owner_repo;

    #[test]
    fn github_owner_repo_parses_https_and_ssh() {
        assert_eq!(
            github_owner_repo("https://github.com/May-Agent/may.git").as_deref(),
            Some("May-Agent/may")
        );
        assert_eq!(
            github_owner_repo("git@github.com:May-Agent/may.git").as_deref(),
            Some("May-Agent/may")
        );
        assert_eq!(
            github_owner_repo("ssh://git@github.com/May-Agent/may").as_deref(),
            Some("May-Agent/may")
        );
        assert!(github_owner_repo("https://gitlab.com/a/b.git").is_none());
    }
}
