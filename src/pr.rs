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
use crate::issue::IssueError;
use crate::issue::github::{GhCliTokenSource, GitHubRest, GitHubTokenSource};
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
    /// Host-owned scratch space outside every sandbox — throwaway check
    /// clones live here.
    pub scratch_root: PathBuf,
    /// Session cwd — the repo `pr` falls back to outside a clone.
    pub cwd: PathBuf,
    /// Workspace config — `[[projects]]` backends and checks.
    pub workspace_config: crate::workspace::WorkspaceConfig,
    /// Outbound screen: `Ok(true)` publishes, anything else blocks. Wired
    /// to the fast model by the command registration.
    pub screen: Option<std::sync::Arc<ScreenFn>>,
    /// GitHub REST client the GitHub backend's direct calls go through.
    pub github_api: GitHubRest<PrTokenSource>,
}

/// Token source for the GitHub backend's REST client.
#[derive(Debug, Clone)]
pub enum PrTokenSource {
    /// The `gh` CLI login.
    Gh(GhCliTokenSource),
    /// A fixed token, for tests.
    #[cfg(test)]
    Fixed(String),
}

impl GitHubTokenSource for PrTokenSource {
    async fn token(&self) -> Result<String, IssueError> {
        match self {
            Self::Gh(source) => source.token().await,
            #[cfg(test)]
            Self::Fixed(token) => Ok(token.clone()),
        }
    }
}

impl PrContext {
    /// The production REST client: `api.github.com` with the `gh` login.
    #[must_use]
    pub fn default_github_api() -> GitHubRest<PrTokenSource> {
        GitHubRest::new(PrTokenSource::Gh(GhCliTokenSource))
    }
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
    repos: Option<SessionRepos>,
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
        let repos = context
            .db
            .clone()
            .map(|db| SessionRepos::new(&context.session_root, context.session_id.clone(), db));
        Self { context, repos }
    }

    fn repos(&self) -> Result<&SessionRepos, PrError> {
        self.repos
            .as_ref()
            .ok_or_else(|| PrError::Resolve("pr requires the history database".to_string()))
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
        let repos = self.repos()?;
        let clones = repos.clones().await?;
        if let Some(name) = repo {
            if let Some(record) = clones
                .into_iter()
                .find(|c| c.repo_key == name || c.dir.as_str() == name)
            {
                return Ok(Resolved {
                    dir: repos.path(&record.dir),
                    record,
                });
            }
            return Err(PrError::Resolve(format!("no session clone named {name}")));
        }
        if let [record] = clones.as_slice() {
            return Ok(Resolved {
                dir: repos.path(&record.dir),
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
            let dir = repos.ensure_remote_clone(&url, &owner_repo).await?;
            let record = repos
                .clones()
                .await?
                .into_iter()
                .find(|c| repos.path(&c.dir) == dir)
                .ok_or_else(|| PrError::Resolve("clone registry lost the new clone".to_string()))?;
            return Ok(Resolved { record, dir });
        }
        let repo_key = crate::issue::repository_identity(&worktree)
            .map_err(|e| PrError::Resolve(format!("repository identity: {e}")))?;
        // First-contact gate: no binding before a persisted trust decision.
        match crate::trust::trust_for(&self.context.workspace_config, &repo_key, &worktree) {
            None => {
                return Err(PrError::Resolve(
                    "repository trust undecided — the launch gate asks on startup, or run `repo trust set <full|prs-only|local-branch|local>`".to_string(),
                ));
            }
            Some(level) if !level.clones_work() => {
                return Err(PrError::Resolve(
                    "local trust writes the checkout; `pr` is for full, prs-only and local-branch"
                        .to_string(),
                ));
            }
            Some(_) => {}
        }
        let dir = repos.ensure_clone_of(&worktree).await?;
        let record = repos
            .clones()
            .await?
            .into_iter()
            .find(|c| repos.path(&c.dir) == dir)
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
        let head_sha = self.head_sha(&resolved).await?;
        let base_sha = self.base_sha(&resolved).await?;
        if resolved.record.local && head_sha == base_sha {
            return Err(PrError::State("no committed work in the clone".to_string()));
        }
        let (backend, github_repo) = Self::backend_for(&resolved.record);
        // A local PR's branch is its durable ref, named once its number is known.
        let branch = if resolved.record.local {
            String::new()
        } else {
            github_branch(&self.context.session_id, &head_sha)
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
                self.push_and_create(&resolved, &branch, &head_sha, title, owner_repo)
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
        // The PR owns its commit from here on: pin it once, land only it.
        let db = self.db()?;
        if resolved.record.local {
            db.set_pr_branch(pr.id, &pin_ref(pr.number)).await?;
            self.pin(&resolved, pr.number, &head_sha).await?;
        }
        let pr = db
            .update_pr(pr.id, "open", Some(&head_sha), None, None)
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
        // The commit the PR pinned when it was opened or last updated; the
        // clone may have moved on, and what lands is exactly this one.
        let head_sha = pr.head_sha.clone().ok_or_else(|| {
            PrError::State(format!(
                "PR #{number} has no pinned commit — run `pr update {number}`"
            ))
        })?;
        self.db()?
            .update_pr(pr.id, "checks_pending", Some(&head_sha), None, None)
            .await?;
        self.event(&pr, "checks_pending", None).await;
        if pr.backend == "github" {
            return self.merge_github(&resolved, &pr, &head_sha).await;
        }
        // `[[projects]] checks` — the local check command runs on a detached
        // checkout of the pinned commit.
        if let Some(cmd) = self.check_command(&resolved.record)
            && let Err(err) = self.check_pinned(&resolved, &pr, &head_sha, &cmd).await
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
            let refname = pr.branch.clone();
            let sha = head_sha.clone();
            let number = pr.number;
            move || land(&checkout, &refname, &sha, number, &lock)
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
                let note = self.retire_clone(&resolved, &head_sha).await?;
                Ok(format!("merged PR #{number} as {merge_sha}{note}"))
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
        // A terminal PR is refused before anything leaves the machine; the
        // transition below re-checks atomically.
        if matches!(pr.state.as_str(), "merged" | "closed") {
            return Err(PrError::State(format!(
                "PR #{number} is already {}",
                pr.state
            )));
        }
        // Closing upstream first disables auto-merge; if it fails the local
        // row stays as it was.
        if pr.backend == "github" {
            let owner_repo = github_owner_repo(&resolved.record.source)
                .ok_or_else(|| PrError::Resolve("github PR without a repo".to_string()))?;
            let external = pr
                .external_number
                .ok_or_else(|| PrError::State("github PR missing its number".to_string()))?;
            self.close_github(&owner_repo, external).await?;
        }
        self.db()?.close_pr(pr.id).await?;
        self.event(&pr, "closed", None).await;
        if resolved.record.local {
            drop_ref(&resolved.record.source, &pr.branch).await;
        }
        Ok(format!("closed PR #{number}"))
    }

    /// `pr update <n>` — re-pin an own, unlanded PR to the clone's current
    /// HEAD. The only way a PR's commit changes after it was opened.
    ///
    /// # Errors
    /// Fails on ownership, a terminal state, or a dirty clone.
    pub async fn update(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
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
        if matches!(pr.state.as_str(), "merged" | "closed") {
            return Err(PrError::State(format!("PR #{number} is {}", pr.state)));
        }
        let dirty = uncommitted_changes(&resolved.dir).await?;
        if !dirty.is_empty() {
            return Err(PrError::DirtyClone(dirty.join(", ")));
        }
        let head_sha = self.head_sha(&resolved).await?;
        if pr.backend == "github" {
            push_pinned(&resolved.dir, &head_sha, &pr.branch)
                .await
                .map_err(|e| PrError::Gh(e.to_string()))?;
        } else {
            self.pin(&resolved, pr.number, &head_sha).await?;
        }
        self.db()?
            .update_pr(pr.id, "open", Some(&head_sha), None, None)
            .await?;
        self.event(&pr, "updated", Some(&head_sha)).await;
        Ok(format!("PR #{number} now holds {head_sha}"))
    }

    /// Take over a PR whose owning session is gone — or still another's.
    /// Adoption transfers `session_id` so this session may merge or close
    /// it, and the event log records the handover.
    ///
    /// # Errors
    /// Fails when the PR does not exist.
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
            let clones = self.repos()?.clones().await?;
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
                p.has_identity(Path::new(&record.repo_key))
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
        self.context.workspace_config.project_for_identity(&key)
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
        head_sha: &str,
        title: &str,
        owner_repo: &str,
    ) -> Result<i64, PrError> {
        push_pinned(&resolved.dir, head_sha, branch)
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

    /// GitHub landing: the pinned commit is already on the upstream branch
    /// (pushed at open or update); `gh pr merge --auto`, watch until the
    /// upstream state is terminal, then best-effort pull-sync the recorded
    /// checkout and retire the clone.
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
        // Serialized land: queue + watch run inside one blocking call so
        // the mutex guard never crosses an await.
        let state = blocking::unblock({
            let owner_repo = owner_repo.clone();
            move || -> Result<String, String> {
                let _guard = lock.lock().map_err(|_| "land lock poisoned".to_string())?;
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
                let note = self.retire_clone(resolved, head_sha).await?;
                Ok(format!("merged PR #{} upstream{note}", pr.number))
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

    /// Close the upstream pull request through the REST API, authenticated
    /// with the `gh` login's token.
    async fn close_github(&self, owner_repo: &str, external: i64) -> Result<(), PrError> {
        let api = &self.context.github_api;
        let url = format!("{}/repos/{owner_repo}/pulls/{external}", api.api_base());
        api.request(
            zenwave::Method::PATCH,
            url,
            Some(serde_json::json!({ "state": "closed" })),
        )
        .await
        .map_err(|error| match error {
            IssueError::NotFound(_) => {
                PrError::Gh(format!("close PR #{external}: not found on {owner_repo}"))
            }
            other => PrError::Gh(format!("close PR #{external}: {other}")),
        })?;
        Ok(())
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
            .project_for_identity(Path::new(&record.repo_key))
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

    /// The clone's HEAD, read from the trusted side: `ls-remote` run in the
    /// checkout (or nowhere, for a remote clone), never a command in the clone.
    async fn head_sha(&self, resolved: &Resolved) -> Result<String, PrError> {
        blocking::unblock({
            let checkout = resolved
                .record
                .local
                .then(|| resolved.record.source.clone());
            let clone = resolved.dir.to_string_lossy().into_owned();
            move || clone_head(checkout.as_deref().map(Path::new), &clone)
        })
        .await
        .map_err(PrError::RepoClone)
    }

    /// Fetch exactly `sha` from the clone into the PR's durable ref.
    async fn pin(&self, resolved: &Resolved, number: i64, sha: &str) -> Result<(), PrError> {
        blocking::unblock({
            let checkout = resolved.record.source.clone();
            let clone = resolved.dir.to_string_lossy().into_owned();
            let sha = sha.to_string();
            move || {
                git(
                    Some(Path::new(&checkout)),
                    &["fetch", &clone, &format!("+{sha}:{}", pin_ref(number))],
                )
                .map(drop)
            }
        })
        .await
        .map_err(PrError::RepoClone)
    }

    /// Run the check command on a detached checkout of `sha`, so what is
    /// checked is exactly what lands whatever the clone does meanwhile.
    async fn check_pinned(
        &self,
        resolved: &Resolved,
        pr: &crate::db::PrRow,
        sha: &str,
        cmd: &str,
    ) -> Result<(), String> {
        // A throwaway clone sharing nothing with the user's checkout, in
        // host scratch space outside the sandbox; the guard removes it
        // whatever the outcome.
        let (scratch, dir) = blocking::unblock({
            let checkout = resolved.record.source.clone();
            let root = self.context.scratch_root.clone();
            let prefix = format!("{}-{}-", self.context.session_id, pr.number);
            let sha = sha.to_string();
            move || -> Result<_, String> {
                std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
                let scratch = tempfile::Builder::new()
                    .prefix(&prefix)
                    .tempdir_in(&root)
                    .map_err(|e| e.to_string())?;
                let dir = scratch.path().join("repo");
                git(
                    None,
                    &[
                        "clone",
                        "--local",
                        "--no-hardlinks",
                        "--no-checkout",
                        &checkout,
                        &dir.to_string_lossy(),
                    ],
                )
                .map_err(|e| e.to_string())?;
                git(Some(&dir), &["checkout", "--detach", &sha]).map_err(|e| e.to_string())?;
                Ok((scratch, dir))
            }
        })
        .await?;
        let result = run_check(&dir, cmd).await;
        blocking::unblock(move || drop(scratch)).await;
        result
    }

    /// After landing, delete the clone when it holds exactly the landed
    /// work; otherwise keep it and say so, so nothing newer is lost.
    async fn retire_clone(&self, resolved: &Resolved, landed: &str) -> Result<String, PrError> {
        let clean = uncommitted_changes(&resolved.dir).await?.is_empty();
        let same = self.head_sha(resolved).await? == landed;
        if clean && same {
            self.repos()?.delete_clone(&resolved.record.dir).await?;
            Ok(String::new())
        } else {
            Ok(" (the clone was kept: it holds other work)".to_string())
        }
    }

    async fn event(&self, pr: &crate::db::PrRow, kind: &str, detail: Option<&str>) {
        if let Some(db) = self.context.db.as_ref() {
            let _ = db
                .add_pr_event(pr.id, kind, &self.context.session_id, "main", None, detail)
                .await;
        }
    }
}

/// The remote branch of a GitHub PR, named from its session and the commit it
/// is first opened at, so two PRs of one session never share a branch and a
/// retried open of the same work reuses its branch. The name is stored in the
/// PR row; `update`, close and adoption read it from there.
fn github_branch(session_id: &str, head_sha: &str) -> String {
    let short = head_sha.get(..12).unwrap_or(head_sha);
    format!("may/{session_id}-{short}")
}

/// The durable ref in the source checkout that holds PR `number`'s commit.
fn pin_ref(number: i64) -> String {
    format!("refs/may/pr/{number}")
}

/// HEAD of `clone`, resolved by `ls-remote` so no command runs inside it.
fn clone_head(
    checkout: Option<&Path>,
    clone: &str,
) -> Result<String, crate::repo_clone::RepoCloneError> {
    let out = git(checkout, &["ls-remote", clone, "HEAD"])?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| crate::repo_clone::RepoCloneError::Git {
            args: format!("ls-remote {clone} HEAD"),
            stderr: "the clone has no HEAD".to_string(),
        })
}

/// Push the pinned commit to the PR's branch on `upstream`, never forced.
async fn push_pinned(
    clone: &Path,
    sha: &str,
    branch: &str,
) -> Result<(), crate::repo_clone::RepoCloneError> {
    let clone = clone.to_path_buf();
    let spec = format!("{sha}:refs/heads/{branch}");
    blocking::unblock(move || git(Some(&clone), &["push", "upstream", &spec]))
        .await
        .map(drop)
}

/// Remove a PR's durable ref once the PR is terminal.
async fn drop_ref(checkout: &str, refname: &str) {
    let checkout = checkout.to_string();
    let refname = refname.to_string();
    let _ =
        blocking::unblock(move || git(Some(Path::new(&checkout)), &["update-ref", "-d", &refname]))
            .await;
}

/// The serialized land of the PR's pinned commit `sha`: merge-tree
/// preflight, checkout-overlap check, merge, then the ref is dropped. Runs
/// inside `blocking::unblock` under the per-repo lock.
fn land(
    checkout: &str,
    refname: &str,
    sha: &str,
    number: i64,
    lock: &Mutex<()>,
) -> Result<String, String> {
    let _guard = lock.lock().map_err(|_| "land lock poisoned".to_string())?;
    let checkout_path = Path::new(checkout);
    let pinned = git(
        Some(checkout_path),
        &["rev-parse", "--verify", &format!("{refname}^{{commit}}")],
    )
    .map_err(|e| e.to_string())?;
    if pinned != sha {
        return Err(format!("{refname} holds {pinned}, not the checked {sha}"));
    }
    let head = git(Some(checkout_path), &["rev-parse", "HEAD"]).map_err(|e| e.to_string())?;
    // merge-tree preflight (git ≥ 2.38): a conflict exits nonzero with the
    // conflicted paths after the tree oid.
    match git(
        Some(checkout_path),
        &["merge-tree", "--write-tree", "--name-only", &head, sha],
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
        &["diff", "--name-only", &format!("{head}..{sha}")],
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
            sha,
        ],
    )
    .map_err(|e| e.to_string())?;
    let merged = git(Some(checkout_path), &["rev-parse", "HEAD"]).map_err(|e| e.to_string())?;
    git(Some(checkout_path), &["update-ref", "-d", refname]).map_err(|e| e.to_string())?;
    Ok(merged)
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
    use super::{
        GitHubRest, PrContext, PrStore, PrTokenSource, git, github_branch, github_owner_repo,
        push_pinned,
    };
    use crate::repo_clone::SessionRepos;
    use crate::workspace::{ProjectConfig, ProjectIssues, ProjectPr, WorkspaceConfig};
    use hyper::body::Bytes;
    use hyper::header::AUTHORIZATION;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    struct Fixture {
        tmp: tempfile::TempDir,
        checkout: PathBuf,
        db: crate::db::ChatDb,
        repo_key: PathBuf,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let checkout = tmp.path().join("checkout");
            git(None, &["init", &checkout.to_string_lossy()]).unwrap();
            identify(&checkout);
            std::fs::write(checkout.join("file.txt"), "hello").unwrap();
            git(Some(&checkout), &["add", "."]).unwrap();
            git(Some(&checkout), &["commit", "-m", "init"]).unwrap();
            let db = crate::db::ChatDb::open(&tmp.path().join("history.db"))
                .await
                .unwrap();
            let repo_key = dunce::canonicalize(checkout.join(".git")).unwrap();
            Self {
                tmp,
                checkout,
                db,
                repo_key,
            }
        }

        fn session_root(&self, id: &str) -> PathBuf {
            self.tmp.path().join(id)
        }

        /// A session's store with a clone of the checkout already made.
        async fn store(&self, id: &str, checks: Option<&str>) -> (PrStore, PathBuf) {
            let root = self.session_root(id);
            let clone = SessionRepos::new(&root, id, self.db.clone())
                .ensure_local_clone(&self.checkout, &self.repo_key)
                .await
                .unwrap();
            identify(&clone);
            let projects = checks
                .map(|c| ProjectConfig {
                    path: self.checkout.clone(),
                    issues: ProjectIssues::default(),
                    checks: Some(c.to_string()),
                    pr: ProjectPr::default(),
                })
                .into_iter()
                .collect();
            let store = PrStore::new(PrContext {
                db: Some(self.db.clone()),
                session_id: id.to_string(),
                session_root: root,
                scratch_root: self.tmp.path().join("scratch"),
                cwd: self.checkout.clone(),
                workspace_config: WorkspaceConfig {
                    workdirs: Vec::new(),
                    projects,
                },
                screen: None,
                github_api: PrContext::default_github_api(),
            });
            (store, clone)
        }

        /// A store whose only clone is a GitHub remote clone of `o/r`, with
        /// a GitHub-backed PR #1 (upstream #7) in `state`, and its API at `base`.
        async fn github_store(&self, id: &str, base: &str, state: &str) -> PrStore {
            let url = "https://github.com/o/r.git";
            self.db
                .insert_repo_clone(
                    id,
                    &crate::db::RepoCloneRow {
                        repo_key: url.to_string(),
                        dir: "r-00000000".to_string(),
                        source: url.to_string(),
                        local: false,
                    },
                )
                .await
                .unwrap();
            let pr = self
                .db
                .create_pr(
                    url,
                    id,
                    "t",
                    "",
                    "may/s",
                    "base",
                    "github",
                    true,
                    None,
                    Some(7),
                )
                .await
                .unwrap();
            if state != "open" {
                self.db
                    .update_pr(pr.id, state, None, None, None)
                    .await
                    .unwrap();
            }
            PrStore::new(PrContext {
                db: Some(self.db.clone()),
                session_id: id.to_string(),
                session_root: self.session_root(id),
                scratch_root: self.tmp.path().join("scratch"),
                cwd: self.checkout.clone(),
                workspace_config: WorkspaceConfig {
                    workdirs: Vec::new(),
                    projects: Vec::new(),
                },
                screen: None,
                github_api: GitHubRest::with_api_base(
                    PrTokenSource::Fixed("test-token".to_string()),
                    base,
                ),
            })
        }

        async fn pr_state(&self) -> String {
            self.db
                .get_pr("https://github.com/o/r.git", 1)
                .await
                .unwrap()
                .unwrap()
                .state
        }
    }

    type Requests = Arc<Mutex<Vec<(String, String, String, String)>>>;

    /// A loopback GitHub API answering every request with `status`;
    /// records `(method, path, authorization, body)`.
    async fn mock_api(status: u16) -> (String, Requests) {
        let requests: Requests = Arc::default();
        let seen = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let seen = seen.clone();
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let seen = seen.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body = http_body_util::BodyExt::collect(body)
                            .await
                            .map(http_body_util::Collected::to_bytes)
                            .unwrap_or_default();
                        seen.lock().unwrap().push((
                            parts.method.to_string(),
                            parts.uri.path().to_string(),
                            auth,
                            String::from_utf8_lossy(&body).to_string(),
                        ));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(http_body_util::Full::new(Bytes::from(
                                    "{\"message\":\"m\"}",
                                )))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    fn identify(dir: &Path) {
        git(Some(dir), &["config", "user.email", "t@t"]).unwrap();
        git(Some(dir), &["config", "user.name", "t"]).unwrap();
    }

    fn commit_file(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), name).unwrap();
        git(Some(dir), &["add", "."]).unwrap();
        git(Some(dir), &["commit", "-m", name]).unwrap();
    }

    #[tokio::test]
    async fn adopted_pr_lands_after_the_original_session_is_deleted() {
        let fx = Fixture::new().await;
        let (first, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        first.open(None, "work", None, true).await.unwrap();
        let pinned = git(Some(&fx.checkout), &["rev-parse", "refs/may/pr/1"]).unwrap();
        // Session deletion drops the clone; the PR's ref survives it.
        SessionRepos::new(&fx.session_root("s1"), "s1", fx.db.clone())
            .delete_all()
            .await
            .unwrap();
        assert!(!clone.exists());
        git(
            Some(&fx.checkout),
            &["rev-parse", "--verify", "refs/may/pr/1"],
        )
        .unwrap();

        let (second, _) = fx.store("s2", None).await;
        second.adopt(1, None).await.unwrap();
        second.merge(1, None).await.unwrap();
        assert!(fx.checkout.join("work.txt").exists());
        let parents = git(Some(&fx.checkout), &["rev-parse", "HEAD^2"]).unwrap();
        assert_eq!(parents, pinned);
        // Landing drops the ref.
        assert!(
            git(
                Some(&fx.checkout),
                &["rev-parse", "--verify", "refs/may/pr/1"]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_commit_made_while_checks_run_does_not_land() {
        let fx = Fixture::new().await;
        let marker = fx.tmp.path().join("checked");
        let (_, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        // The check records the tree it ran on, then commits in the clone.
        let cmd = format!(
            "test -f work.txt && ! test -f late.txt && git rev-parse HEAD > {marker} && \
             echo late > {clone}/late.txt && git -C {clone} add . && \
             git -C {clone} commit -q -m late",
            marker = marker.display(),
            clone = clone.display(),
        );
        // Same session and clone, now with the check configured.
        let (store, _) = fx.store("s1", Some(&cmd)).await;
        store.open(None, "work", None, false).await.unwrap();
        let checked = std::fs::read_to_string(&marker).unwrap().trim().to_string();
        let landed = git(Some(&fx.checkout), &["rev-parse", "HEAD^2"]).unwrap();
        assert_eq!(landed, checked);
        assert!(!fx.checkout.join("late.txt").exists());
        // The throwaway check clone is gone and the checkout has no worktrees.
        assert_eq!(
            std::fs::read_dir(fx.tmp.path().join("scratch"))
                .unwrap()
                .count(),
            0
        );
        assert!(
            !git(Some(&fx.checkout), &["worktree", "list"])
                .unwrap()
                .contains("pr-check")
        );
        // The clone moved on, so it is kept rather than deleted.
        assert!(clone.join("late.txt").exists());
    }

    #[tokio::test]
    async fn close_drops_the_ref() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        store.open(None, "work", None, true).await.unwrap();
        store.close(1, None).await.unwrap();
        assert!(
            git(
                Some(&fx.checkout),
                &["rev-parse", "--verify", "refs/may/pr/1"]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn closing_a_github_pr_patches_upstream_then_records_closed() {
        let fx = Fixture::new().await;
        let (base, requests) = mock_api(200).await;
        let store = fx.github_store("s1", &base, "open").await;
        store.close(1, None).await.unwrap();
        let seen = requests.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![(
                "PATCH".to_string(),
                "/repos/o/r/pulls/7".to_string(),
                "Bearer test-token".to_string(),
                "{\"state\":\"closed\"}".to_string(),
            )]
        );
        assert_eq!(fx.pr_state().await, "closed");
    }

    #[tokio::test]
    async fn a_failed_upstream_close_leaves_the_row_unchanged() {
        let fx = Fixture::new().await;
        let (base, requests) = mock_api(422).await;
        let store = fx.github_store("s1", &base, "open").await;
        let err = store.close(1, None).await.unwrap_err();
        assert!(err.to_string().contains("422"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(fx.pr_state().await, "open");
    }

    #[tokio::test]
    async fn a_merged_or_closed_github_pr_cannot_be_closed_again() {
        for state in ["merged", "closed"] {
            let fx = Fixture::new().await;
            let (base, requests) = mock_api(200).await;
            let store = fx.github_store("s1", &base, state).await;
            let err = store.close(1, None).await.unwrap_err();
            assert!(err.to_string().contains(state), "{err}");
            assert!(requests.lock().unwrap().is_empty());
            assert_eq!(fx.pr_state().await, state);
        }
    }

    #[tokio::test]
    async fn a_terminal_local_pr_cannot_be_closed_again() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        store.open(None, "work", None, true).await.unwrap();
        let key = fx.repo_key.to_str().unwrap();
        let pr = fx.db.get_pr(key, 1).await.unwrap().unwrap();
        fx.db
            .update_pr(pr.id, "merged", None, None, None)
            .await
            .unwrap();
        let err = store.close(1, None).await.unwrap_err();
        assert!(err.to_string().contains("merged"), "{err}");
        let pr = fx.db.get_pr(key, 1).await.unwrap().unwrap();
        assert_eq!(pr.state, "merged");
        // The transition itself refuses too, not just `close`.
        assert!(matches!(
            fx.db.close_pr(pr.id).await,
            Err(crate::db::ChatDbError::PrTerminal { .. })
        ));
    }

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

    #[tokio::test]
    async fn a_second_github_pr_pushes_after_the_first_was_squash_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        git(
            None,
            &["init", "--bare", "-b", "main", &remote.to_string_lossy()],
        )
        .unwrap();
        let clone = tmp.path().join("clone");
        git(
            None,
            &["clone", &remote.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        identify(&clone);
        git(Some(&clone), &["remote", "rename", "origin", "upstream"]).unwrap();
        git(Some(&clone), &["checkout", "-b", "main"]).unwrap();
        commit_file(&clone, "base.txt");
        git(Some(&clone), &["push", "upstream", "main"]).unwrap();

        // First PR: pushed to its own branch, then squash-merged upstream
        // (a new commit on main that is not a descendant of the PR head),
        // its branch left on the remote.
        commit_file(&clone, "one.txt");
        let first_sha = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        let first = github_branch("s1", &first_sha);
        push_pinned(&clone, &first_sha, &first).await.unwrap();
        git(Some(&clone), &["checkout", "-b", "squash", "main~1"]).unwrap();
        std::fs::write(clone.join("one.txt"), "one.txt").unwrap();
        git(Some(&clone), &["add", "."]).unwrap();
        git(Some(&clone), &["commit", "-m", "one.txt (#1)"]).unwrap();
        let squash_sha = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        assert_ne!(squash_sha, first_sha, "a squash merge is a new commit");
        git(Some(&clone), &["push", "upstream", "squash:main"]).unwrap();
        git(Some(&clone), &["checkout", "main"]).unwrap();

        // Second PR from the same session: different work, different branch.
        git(Some(&clone), &["reset", "--hard", "upstream/main"]).ok();
        git(Some(&clone), &["fetch", "upstream"]).unwrap();
        git(Some(&clone), &["reset", "--hard", "upstream/main"]).unwrap();
        commit_file(&clone, "two.txt");
        let second_sha = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        let second = github_branch("s1", &second_sha);
        assert_ne!(first, second);
        push_pinned(&clone, &second_sha, &second).await.unwrap();
        assert_eq!(
            git(
                Some(&remote),
                &["rev-parse", &format!("refs/heads/{second}")]
            )
            .unwrap(),
            second_sha
        );
        // The old shared name would have been rejected without a force.
        assert!(
            push_pinned(&clone, &second_sha, &first).await.is_err(),
            "pushes are never forced"
        );
        // A retry of the same PR reuses its branch.
        push_pinned(&clone, &second_sha, &second).await.unwrap();
    }
}
