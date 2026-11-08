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
//!
//! The clone is agent-writable, so its git configuration, hooks and
//! attributes are the agent's. Git that works inside it and the configured
//! checks run in the session's terminal sandbox ([`CloneSandbox`]); host git
//! touches only the user's checkout and host scratch repositories, and reads
//! a clone only by fetching from it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use askama::Template;

use crate::db::ChatDb;
use crate::host_git::HostGit;
use crate::issue::IssueError;
use crate::issue::github::{GhCliTokenSource, GitHubRest, GitHubTokenSource};
use crate::repo_clone::{CloneRecord, CloneSandbox, SessionRepos, uncommitted_changes};

mod merge_watch;

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
    /// A commit of the PR's range is not signed by the user.
    #[error(transparent)]
    Signing(#[from] crate::commit_signing::SigningError),
    /// Filesystem failure while staging.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A subagent called an action only the main agent has.
    #[error("`pr {0}` is the main agent's; a subagent has `pr list` and `pr show`")]
    MainAgentOnly(&'static str),
    /// The user's approval to open the PR was refused or could not be had.
    #[error(transparent)]
    Publish(#[from] crate::agent::publish_approval::PublishError),
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
    /// Host-owned scratch space outside every sandbox, on the session root's
    /// filesystem: new clones are made and pushes staged here.
    pub scratch_root: PathBuf,
    /// Session cwd — the repo `pr` falls back to outside a clone.
    pub cwd: PathBuf,
    /// Workspace config — `[[projects]]` backends and checks.
    pub workspace_config: crate::workspace::WorkspaceConfig,
    /// Outbound screen: `Ok(true)` publishes, anything else blocks. Wired
    /// to the fast model by the command registration.
    pub screen: Option<std::sync::Arc<ScreenFn>>,
    /// The user's signing configuration: every commit of a PR must carry
    /// a signature valid under its key.
    pub signing: crate::commit_signing::CommitSigning,
    /// GitHub REST client the GitHub backend's direct calls go through.
    pub github_api: GitHubRest<PrTokenSource>,
    /// The main agent's `tasks.md` inbox: told when a PR opens, merges, is
    /// rejected or closes.
    pub task_events: Option<aither_agent::TaskEvents>,
    /// Asks the user before a PR is opened on a repository they do not own;
    /// `None` publishes without asking.
    pub publish: Option<crate::agent::publish_approval::PublishGate>,
    /// Records the durations of the PR's checks, when the session keeps
    /// them; `None` records nothing.
    pub ci: Option<CiWatch>,
}

/// What recording a PR's CI durations needs beyond the database.
#[derive(Clone, Debug)]
pub struct CiWatch {
    /// The May home, whose `config.toml` holds the `[ci]` thresholds, read
    /// each time a PR's checks finish.
    pub may_dir: PathBuf,
    /// Where a finding's warning is announced to the clients.
    pub ui_events: crate::ui_event_bus::UiEventBus,
}

/// Token source for the GitHub backend's REST client.
#[derive(Debug, Clone)]
pub enum PrTokenSource {
    /// The `gh` CLI login.
    Gh(GhCliTokenSource),
    /// A fixed token, for tests.
    #[cfg(test)]
    Fixed(String),
    /// No GitHub login, for tests.
    #[cfg(test)]
    Unauthenticated,
}

impl GitHubTokenSource for PrTokenSource {
    async fn token(&self) -> Result<String, IssueError> {
        match self {
            Self::Gh(source) => source.token().await,
            #[cfg(test)]
            Self::Fixed(token) => Ok(token.clone()),
            #[cfg(test)]
            Self::Unauthenticated => Err(IssueError::GitHubAuth("not logged in".to_string())),
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

/// Records a GitHub PR's check durations when its checks finish.
struct CiObserver<'a> {
    store: &'a PrStore,
    pr: &'a crate::db::PrRow,
    owner_repo: &'a str,
    head_sha: &'a str,
}

impl merge_watch::ChecksObserver for CiObserver<'_> {
    async fn finished(&self) {
        self.store
            .observe_github_checks(self.pr, self.owner_repo, self.head_sha)
            .await;
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
    sandbox: CloneSandbox,
}

impl std::fmt::Debug for PrStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrStore").finish_non_exhaustive()
    }
}

impl PrStore {
    /// Build the engine for one session, running what works inside its
    /// clones in `sandbox`.
    #[must_use]
    pub fn new(context: PrContext, sandbox: CloneSandbox) -> Self {
        let repos = context.db.clone().map(|db| {
            SessionRepos::new(&context.session_root, context.session_id.clone(), db)
                .with_host(context.signing.host().clone())
        });
        Self {
            context,
            repos,
            sandbox,
        }
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
        // No clones yet: clone the cwd's repo on demand, as its trust level
        // decides (`CloneSource::of`).
        let worktree = crate::repo_search::worktree_root(&self.context.cwd)
            .ok_or_else(|| PrError::Resolve("not inside a repository".to_string()))?;
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
        let dir = repos
            .ensure_clone_of(
                &worktree,
                &self.context.workspace_config,
                &self.context.scratch_root,
            )
            .await?
            .ok_or_else(|| {
                PrError::Resolve("trust binds no clone for this repository".to_string())
            })?;
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
        title: Option<&str>,
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
        let dirty = uncommitted_changes(&self.sandbox, &resolved.dir).await?;
        if !dirty.is_empty() {
            return Err(PrError::DirtyClone(dirty.join(", ")));
        }
        let head_sha = self.head_sha(&resolved).await?;
        let base_sha = self.base_sha(&resolved).await?;
        if resolved.record.local && head_sha == base_sha {
            return Err(PrError::State("no committed work in the clone".to_string()));
        }
        let (backend, github_repo) = Self::backend_for(&resolved.record);
        let subjects = self
            .commit_subjects(&resolved, &base_sha, &head_sha)
            .await?;
        // Only a GitHub issue is linked in published text: a May issue
        // number is internal and never leaves the machine.
        let github_issue = issue_number.filter(|_| {
            self.project_for_record(&resolved.record)
                .is_some_and(|p| p.github_repository().is_some())
        });
        let text = pr_text(title, &subjects, github_issue)?;
        let (title, body) = (text.title.as_str(), text.body.as_str());
        // A local PR's branch is its durable ref, named once its number is known.
        let branch = if resolved.record.local {
            String::new()
        } else {
            github_branch(&self.context.session_id, &head_sha)
        };
        // The outbound screen vets every published text before it leaves —
        // the "PRs only" trust mode's guard.
        if backend == "github"
            && let Some(reason) = self.screen_outbound(&resolved.record, title, body).await?
        {
            return Err(PrError::Screened(reason));
        }
        if backend == "github"
            && let (Some(gate), Some(owner_repo)) = (&self.context.publish, github_repo.as_deref())
        {
            let repository = owner_repo
                .parse::<crate::workspace::GitHubRepository>()
                .map_err(|error| PrError::Resolve(error.to_string()))?;
            gate.approve_pull_request(&self.context.github_api, &repository, title, body)
                .await?;
        }
        // GitHub-bound: push the session branch before recording, so the
        // row never points at a PR that failed to exist upstream.
        let external_number = if backend == "github" {
            let owner_repo = github_repo
                .as_deref()
                .ok_or_else(|| PrError::Resolve("github backend without a repo".to_string()))?;
            Some(
                self.push_and_create(&resolved, &branch, &base_sha, &head_sha, &text, owner_repo)
                    .await?,
            )
        } else {
            self.verify_local(&resolved, &base_sha, &head_sha).await?;
            None
        };
        let pr = self
            .db()?
            .create_pr(
                &resolved.record.repo_key,
                &self.context.session_id,
                title,
                body,
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
        self.task_event(&pr, aither_agent::TaskEvent::PullRequestOpened);
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
    /// Fails on ownership, state, a dirty clone, checks, or a rejected merge.
    ///
    /// # Panics
    /// Never: the merge report renders for every input.
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
        // Merging deletes the clone: work it holds outside a commit would
        // go with it, so it refuses and names the files.
        let dirty = uncommitted_changes(&self.sandbox, &resolved.dir).await?;
        if !dirty.is_empty() {
            return Err(PrError::DirtyClone(dirty.join(", ")));
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
            let host = self.context.signing.host().clone();
            move || land(&host, &checkout, &refname, &sha, number, &lock)
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
                self.task_event(&pr, aither_agent::TaskEvent::PullRequestMerged);
                let clone = self.retire_clone(&resolved, &head_sha).await?;
                Ok(PrMerged {
                    number,
                    merge_sha: Some(&merge_sha),
                    clone_dir: resolved.record.dir.as_str(),
                    clone,
                    remote_branch: RemoteBranch::Local,
                }
                .render()
                .expect("the merge result renders"))
            }
            Err(reason) => {
                self.db()?
                    .update_pr(pr.id, "rejected", Some(&head_sha), None, None)
                    .await?;
                self.event(&pr, "rejected", Some(&reason)).await;
                self.task_event(&pr, aither_agent::TaskEvent::PullRequestRejected);
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
        self.task_event(&pr, aither_agent::TaskEvent::PullRequestClosed);
        if resolved.record.local {
            drop_ref(
                self.context.signing.host(),
                &resolved.record.source,
                &pr.branch,
            )
            .await;
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
        let dirty = uncommitted_changes(&self.sandbox, &resolved.dir).await?;
        if !dirty.is_empty() {
            return Err(PrError::DirtyClone(dirty.join(", ")));
        }
        let head_sha = self.head_sha(&resolved).await?;
        if pr.backend == "github" {
            push_pinned(
                &self.context.scratch_root,
                &resolved.dir,
                &resolved.record.source,
                &head_sha,
                &pr.branch,
                BranchState::Pushed,
                SignedRange {
                    signing: &self.context.signing,
                    base: &pr.base_sha,
                },
            )
            .await?;
        } else {
            self.verify_local(&resolved, &pr.base_sha, &head_sha)
                .await?;
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

    /// Subjects of the commits `base..head`, oldest first.
    async fn commit_subjects(
        &self,
        resolved: &Resolved,
        base: &str,
        head: &str,
    ) -> Result<Vec<String>, PrError> {
        let script = CommitSubjects {
            base: ShellWord::new(base).map_err(PrError::State)?,
            head: ShellWord::new(head).map_err(PrError::State)?,
        }
        .render()
        .expect("the commit subjects script renders");
        let log = self.sandbox.run(&resolved.dir, &script).await?;
        Ok(log.lines().map(str::to_string).collect())
    }

    /// Run the outbound screen when the project enables it. `Some(reason)`
    /// means the published text was blocked.
    async fn screen_outbound(
        &self,
        record: &CloneRecord,
        title: &str,
        body: &str,
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
        Ok(match screen(title, body).await {
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
        base_sha: &str,
        head_sha: &str,
        text: &PrText,
        owner_repo: &str,
    ) -> Result<i64, PrError> {
        push_pinned(
            &self.context.scratch_root,
            &resolved.dir,
            &resolved.record.source,
            head_sha,
            branch,
            BranchState::New,
            SignedRange {
                signing: &self.context.signing,
                base: base_sha,
            },
        )
        .await?;
        let url = blocking::unblock({
            let owner_repo = owner_repo.to_string();
            let branch = branch.to_string();
            let title = text.title.clone();
            let body = text.body.clone();
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
                    &body,
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
        let github = merge_watch::GithubPr {
            api: &self.context.github_api,
            owner_repo: &owner_repo,
            number: external,
        };
        // Read before queueing: the bound and the removals that predate it.
        let before = merge_watch::Upstream::snapshot(&github).await?;
        let expected = merge_watch::Upstream::expected_checks(&github).await?;
        self.event(pr, "queued", None).await;
        // Serialized land: the lock covers queueing the PR, not the wait for
        // it to land.
        blocking::unblock({
            let owner_repo = owner_repo.clone();
            move || -> Result<(), String> {
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
                .map(drop)
                .map_err(|e| e.to_string())
            }
        })
        .await
        .map_err(PrError::Gh)?;
        // The checks are timed the moment they finish, not when the PR lands.
        let observer = CiObserver {
            store: self,
            pr,
            owner_repo: &owner_repo,
            head_sha,
        };
        let watching = merge_watch::Observing::new(&github, &observer);
        let outcome = merge_watch::watch(
            &watching,
            &merge_watch::SystemClock::new(),
            before.removals,
            expected,
        )
        .await?;
        watching.tell_if_unheard().await;
        let now = jiff::Timestamp::now().to_string();
        match outcome {
            merge_watch::Outcome::Merged => {
                self.db()?
                    .update_pr(pr.id, "merged", Some(head_sha), None, Some(now.as_str()))
                    .await?;
                self.event(pr, "merged", Some(head_sha)).await;
                self.record_queue_wait(pr).await;
                self.task_event(pr, aither_agent::TaskEvent::PullRequestMerged);
                self.pull_sync(&owner_repo).await;
                let clone = self.retire_clone(resolved, head_sha).await?;
                let remote_branch = self.delete_remote_branch(&owner_repo, &pr.branch).await;
                Ok(PrMerged {
                    number: pr.number,
                    merge_sha: None,
                    clone_dir: resolved.record.dir.as_str(),
                    clone,
                    remote_branch,
                }
                .render()
                .expect("the merge result renders"))
            }
            merge_watch::Outcome::Stopped(stopped) => {
                let report = merge_watch::StoppedReport::new(pr.number, &stopped)
                    .render()
                    .expect("the watch report renders");
                let report = report.trim_end().to_string();
                match stopped {
                    merge_watch::Stopped::Closed => {
                        self.db()?
                            .update_pr(pr.id, "closed", Some(head_sha), None, None)
                            .await?;
                        self.event(pr, "closed", Some(&report)).await;
                        self.task_event(pr, aither_agent::TaskEvent::PullRequestClosed);
                        Err(PrError::State(report))
                    }
                    merge_watch::Stopped::CheckFailed(_) => {
                        self.db()?
                            .update_pr(pr.id, "failed", Some(head_sha), None, None)
                            .await?;
                        self.event(pr, "check_failed", Some(&report)).await;
                        Err(PrError::CheckFailed(report))
                    }
                    merge_watch::Stopped::Removed(_) => {
                        self.db()?
                            .update_pr(pr.id, "failed", Some(head_sha), None, None)
                            .await?;
                        self.event(pr, "removed_from_queue", Some(&report)).await;
                        Err(PrError::State(report))
                    }
                    merge_watch::Stopped::TimedOut { .. } => {
                        // Still queued: the state stays as it is, and
                        // `pr merge` watches it again.
                        self.event(pr, "watch_timeout", Some(&report)).await;
                        Err(PrError::State(report))
                    }
                }
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

    /// Delete a merged PR's `may/*` branch on GitHub. A branch already gone
    /// (the repository deletes head branches on merge) counts as deleted;
    /// any other failure is reported, since the merge itself has landed.
    async fn delete_remote_branch(&self, owner_repo: &str, branch: &str) -> RemoteBranch {
        if !branch.starts_with("may/") {
            return RemoteBranch::NotDeleted(
                branch.to_string(),
                "only may/* branches are the session's to delete".to_string(),
            );
        }
        let api = &self.context.github_api;
        let url = format!(
            "{}/repos/{owner_repo}/git/refs/heads/{branch}",
            api.api_base()
        );
        match api
            .request_unchecked(zenwave::Method::DELETE, url, None)
            .await
        {
            // 404 and 422 ("Reference does not exist"): already deleted.
            Ok(response) if response.status().is_success() => {
                RemoteBranch::Deleted(branch.to_string())
            }
            Ok(response) if matches!(response.status().as_u16(), 404 | 422) => {
                RemoteBranch::Deleted(branch.to_string())
            }
            Ok(response) => RemoteBranch::NotDeleted(
                branch.to_string(),
                format!("GitHub answered {}", response.status()),
            ),
            Err(error) => RemoteBranch::NotDeleted(branch.to_string(), error.to_string()),
        }
    }

    /// Best-effort `git pull --ff-only` on the cwd's checkout when it
    /// tracks the merged remote — a sync target, never a blocker.
    async fn pull_sync(&self, owner_repo: &str) {
        let Some(worktree) = crate::repo_search::worktree_root(&self.context.cwd) else {
            return;
        };
        let owner_repo = owner_repo.to_string();
        let host = self.context.signing.host().clone();
        let synced = blocking::unblock(move || {
            let url = host_git(&host, Some(&worktree), &["remote", "get-url", "origin"]).ok()?;
            if github_owner_repo(&url).as_deref() != Some(owner_repo.as_str()) {
                return None;
            }
            host_git(&host, Some(&worktree), &["pull", "--ff-only"]).ok()
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
                let host = self.context.signing.host().clone();
                move || host_git(&host, Some(Path::new(&src)), &["rev-parse", "HEAD"])
            })
            .await
            .map_err(PrError::RepoClone)
        } else {
            Ok(self
                .sandbox
                .run(&resolved.dir, "git rev-parse --verify upstream/HEAD")
                .await?
                .trim()
                .to_string())
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
            let host = self.context.signing.host().clone();
            move || clone_head(&host, checkout.as_deref().map(Path::new), &clone)
        })
        .await
        .map_err(PrError::RepoClone)
    }

    /// Refuse a local PR whose range holds a commit without the user's
    /// signature. The commits are read from a scratch repository that borrows
    /// the checkout's objects and fetches only what the clone added, so the
    /// check reads the host's copy and runs nothing the clone configures.
    async fn verify_local(
        &self,
        resolved: &Resolved,
        base: &str,
        head: &str,
    ) -> Result<(), PrError> {
        let scratch_root = self.context.scratch_root.clone();
        let checkout = resolved.record.source.clone();
        let clone = resolved.dir.to_string_lossy().into_owned();
        let signing = self.context.signing.clone();
        let (base, head) = (base.to_string(), head.to_string());
        blocking::unblock(move || {
            let host = signing.host();
            std::fs::create_dir_all(&scratch_root)?;
            let staging = tempfile::Builder::new()
                .prefix("verify-")
                .tempdir_in(&scratch_root)?;
            let repo = staging.path();
            host_git(
                host,
                None,
                &["init", "--quiet", "--bare", &repo.to_string_lossy()],
            )?;
            let objects = host_git(
                host,
                Some(Path::new(&checkout)),
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "objects",
                ],
            )?;
            std::fs::write(repo.join("objects/info/alternates"), format!("{objects}\n"))?;
            fetch_from_clone(host, repo, &clone, &head)?;
            signing
                .verify_range_blocking(repo, &base, &head)
                .map_err(PrError::from)
        })
        .await
    }

    /// Fetch exactly `sha` from the clone into the PR's durable ref.
    async fn pin(&self, resolved: &Resolved, number: i64, sha: &str) -> Result<(), PrError> {
        blocking::unblock({
            let checkout = resolved.record.source.clone();
            let clone = resolved.dir.to_string_lossy().into_owned();
            let sha = sha.to_string();
            let host = self.context.signing.host().clone();
            move || {
                fetch_from_clone(
                    &host,
                    Path::new(&checkout),
                    &clone,
                    &format!("+{sha}:{}", pin_ref(number)),
                )
                .map(drop)
            }
        })
        .await
        .map_err(PrError::RepoClone)
    }

    /// Run the check command in the sandbox on a throwaway detached checkout
    /// of `sha`, so what is checked is exactly the commit that lands whatever
    /// the session clone does meanwhile. The checkout lives in the session's
    /// `tmp/` and is removed whatever the outcome.
    async fn check_pinned(
        &self,
        resolved: &Resolved,
        pr: &crate::db::PrRow,
        sha: &str,
        cmd: &str,
    ) -> Result<(), String> {
        let root = &self.context.session_root;
        let target = Path::new("tmp").join(format!("pr-check-{}-{}", pr.number, crate::id::new()));
        let prepare = CheckClone {
            source: ShellWord::path(&SessionRepos::relative_path(&resolved.record.dir))?,
            target: ShellWord::path(&target)?,
            sha: ShellWord::new(sha)?,
        }
        .render()
        .expect("the check clone script renders");
        let result = match self.sandbox.run(root, &prepare).await {
            Ok(_) => {
                let started = jiff::Timestamp::now();
                let clock = std::time::Instant::now();
                let ran = self.sandbox.run(&root.join(&target), cmd).await.map(drop);
                self.observe_local_check(pr, sha, cmd, started, clock.elapsed(), ran.is_ok())
                    .await;
                ran
            }
            Err(error) => Err(error),
        }
        .map_err(|e| e.to_string());
        let remove = RemoveCheckClone {
            target: ShellWord::path(&target)?,
        }
        .render()
        .expect("the check clone removal script renders");
        if let Err(error) = self.sandbox.run(root, &remove).await {
            tracing::warn!(%error, "the check checkout was left in the session's tmp");
        }
        result
    }

    /// After landing, delete the clone when it holds exactly the landed
    /// work; otherwise keep it and say so, so nothing newer is lost.
    async fn retire_clone(&self, resolved: &Resolved, landed: &str) -> Result<CloneFate, PrError> {
        let clean = uncommitted_changes(&self.sandbox, &resolved.dir)
            .await?
            .is_empty();
        let same = self.head_sha(resolved).await? == landed;
        if clean && same {
            self.repos()?.delete_clone(&resolved.record.dir).await?;
            Ok(CloneFate::Deleted)
        } else {
            Ok(CloneFate::Kept)
        }
    }

    /// Records on the `tasks.md` inbox that `pr` changed state as `change`
    /// says.
    fn task_event(
        &self,
        pr: &crate::db::PrRow,
        change: fn(aither_agent::WorkRef) -> aither_agent::TaskEvent,
    ) {
        if let Some(events) = &self.context.task_events
            && let Ok(number) = u64::try_from(pr.number)
        {
            events.record(change(aither_agent::WorkRef::pull_request(number)));
        }
    }

    /// The name of the session clone `dir` lies in, when it lies in one:
    /// a `pr` run inside a clone means that clone's repository.
    ///
    /// # Errors
    /// Fails when the clone registry cannot be read.
    pub async fn clone_containing(&self, dir: &Path) -> Result<Option<String>, PrError> {
        let repos = self.repos()?;
        let (Ok(dir), Ok(root)) = (
            async_fs::canonicalize(dir).await,
            async_fs::canonicalize(repos.root()).await,
        ) else {
            return Ok(None);
        };
        let Some(std::path::Component::Normal(name)) = dir
            .strip_prefix(&root)
            .ok()
            .and_then(|rest| rest.components().next())
        else {
            return Ok(None);
        };
        Ok(repos
            .clones()
            .await?
            .into_iter()
            .find(|c| std::ffi::OsStr::new(c.dir.as_str()) == name)
            .map(|c| c.dir.as_str().to_string()))
    }

    async fn event(&self, pr: &crate::db::PrRow, kind: &str, detail: Option<&str>) {
        if let Some(db) = self.context.db.as_ref() {
            let _ = db
                .add_pr_event(pr.id, kind, &self.context.session_id, "main", None, detail)
                .await;
        }
    }

    /// The `[ci]` thresholds as they are now. `None` when the session keeps
    /// no CI durations or its config cannot be read, which is reported.
    async fn ci_limits(&self) -> Option<(&CiWatch, crate::ci::timing::Limits)> {
        let watch = self.context.ci.as_ref()?;
        match crate::config::MayConfig::load_from(&watch.may_dir.join("config.toml")).await {
            Ok(config) => Some((watch, config.ci.into())),
            Err(error) => {
                tracing::error!(%error, "the [ci] settings could not be read, so this PR's CI durations are not recorded");
                None
            }
        }
    }

    /// Records the GitHub checks of `head_sha` and warns the user about an
    /// abnormally long job. A recording failure is reported, never fatal: the
    /// PR lands or fails on its checks, not on their timing.
    async fn observe_github_checks(&self, pr: &crate::db::PrRow, owner_repo: &str, head_sha: &str) {
        let (Some((watch, limits)), Ok(db), Some(external)) =
            (self.ci_limits().await, self.db(), pr.external_number)
        else {
            return;
        };
        let actions = crate::ci::github::GhActions::new(owner_repo, external);
        let recorder = crate::ci::record::Recorder {
            db,
            actions: &actions,
            limits,
        };
        match recorder.observe_github(pr, head_sha).await {
            Ok(Some(_)) => {
                watch
                    .ui_events
                    .publish(crate::ui_event_bus::UiEventTopics::SESSION_WARNING);
                watch.ui_events.publish_session_change(
                    &pr.session_id,
                    crate::daemon::session_events::SessionChange::Warning,
                );
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, pr = pr.number, "recording the PR's CI durations failed");
            }
        }
    }

    /// Records a local check that took `duration` and warns the user when it
    /// ran abnormally long.
    async fn observe_local_check(
        &self,
        pr: &crate::db::PrRow,
        head_sha: &str,
        command: &str,
        started: jiff::Timestamp,
        duration: std::time::Duration,
        success: bool,
    ) {
        let (Some((watch, limits)), Ok(db)) = (self.ci_limits().await, self.db()) else {
            return;
        };
        let recorder = crate::ci::record::Recorder {
            db,
            actions: &(),
            limits,
        };
        match recorder
            .observe_local(pr, head_sha, command, started, duration, success)
            .await
        {
            Ok(Some(_)) => {
                watch
                    .ui_events
                    .publish(crate::ui_event_bus::UiEventTopics::SESSION_WARNING);
                watch.ui_events.publish_session_change(
                    &pr.session_id,
                    crate::daemon::session_events::SessionChange::Warning,
                );
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, pr = pr.number, "recording the local check's duration failed");
            }
        }
    }

    /// Records the PR's merge-queue wait against its run time.
    async fn record_queue_wait(&self, pr: &crate::db::PrRow) {
        let (Some((_, limits)), Ok(db)) = (self.ci_limits().await, self.db()) else {
            return;
        };
        let recorder = crate::ci::record::Recorder {
            db,
            actions: &(),
            limits,
        };
        if let Err(error) = recorder.record_queue_wait(pr).await {
            tracing::error!(%error, pr = pr.number, "recording the merge-queue wait failed");
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

/// HEAD of `clone`, read from its ref advertisement by `ls-remote`: the
/// `git upload-pack` that serves [`fetch_from_clone`], so nothing the clone
/// configures runs.
fn clone_head(
    host: &HostGit,
    checkout: Option<&Path>,
    clone: &str,
) -> Result<String, crate::repo_clone::RepoCloneError> {
    let out = host_git(host, checkout, &["ls-remote", clone, "HEAD"])?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| crate::repo_clone::RepoCloneError::Git {
            args: format!("ls-remote {clone} HEAD"),
            stderr: "the clone has no HEAD".to_string(),
        })
}

/// Push the pinned commit `sha` of `clone` to the PR's branch at `url`, never
/// forced. The push leaves from a bare repository in host scratch that
/// fetched the commit from the clone, so neither the clone's configuration
/// nor its remotes take part; `url` is the host's record of the remote. The
/// staging repository also holds the remote's own state as remote-tracking
/// refs, so a `pre-push` hook that scans what is not yet on a remote sees
/// only the PR's commits.
async fn push_pinned(
    scratch_root: &Path,
    clone: &Path,
    url: &str,
    sha: &str,
    branch: &str,
    branch_state: BranchState,
    signed: SignedRange<'_>,
) -> Result<(), PrError> {
    let scratch_root = scratch_root.to_path_buf();
    let clone = clone.to_string_lossy().into_owned();
    let url = url.to_string();
    let sha = sha.to_string();
    let spec = format!("{sha}:refs/heads/{branch}");
    let branch = branch.to_string();
    let signing = signed.signing.clone();
    let base = signed.base.to_string();
    blocking::unblock(move || {
        std::fs::create_dir_all(&scratch_root)?;
        let staging = tempfile::Builder::new()
            .prefix("push-")
            .tempdir_in(&scratch_root)?;
        let repo = staging.path();
        let host = signing.host();
        host_git(
            host,
            None,
            &["init", "--quiet", "--bare", &repo.to_string_lossy()],
        )?;
        fetch_from_clone(host, repo, &clone, &sha)?;
        fetch_upstream(host, repo, &url, branch_state, &branch)?;
        // What is pushed is what was verified: both read the host's copy.
        signing.verify_range_blocking(repo, &base, &sha)?;
        host_git(host, Some(repo), &["push", "--quiet", &url, &spec])
            .map(drop)
            .map_err(PrError::from)
    })
    .await
}

/// Whether a PR's branch is already on the remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BranchState {
    /// The first push of the branch.
    New,
    /// An update of a branch an earlier push created.
    Pushed,
}

/// Fetch the remote's real state from `url` itself, never from the agent's
/// clone, into the remote-tracking refs of the staging repository `repo`:
/// its default branch, and the PR's branch when it was pushed before.
fn fetch_upstream(
    host: &HostGit,
    repo: &Path,
    url: &str,
    state: BranchState,
    branch: &str,
) -> Result<(), crate::repo_clone::RepoCloneError> {
    let pushed = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    let mut args = vec![
        "fetch",
        "--quiet",
        "--no-tags",
        url,
        "HEAD:refs/remotes/origin/default",
    ];
    if state == BranchState::Pushed {
        args.push(&pushed);
    }
    host_git(host, Some(repo), &args).map(drop)
}

/// The commits of `base..head` that must carry the user's signature.
#[derive(Clone, Copy)]
struct SignedRange<'a> {
    signing: &'a crate::commit_signing::CommitSigning,
    base: &'a str,
}

/// Fetch `refspec` from the agent-writable repository `clone` into the
/// trusted repository `repo`. The clone serves the fetch through
/// `git upload-pack`, which runs on the host inside the clone, yet none of
/// the executable configuration a repository can carry for itself
/// (fsmonitor, hooks, filters, `uploadpack.packObjectsHook`, `core.sshCommand`
/// and the like) takes part in serving a fetch.
fn fetch_from_clone(
    host: &HostGit,
    repo: &Path,
    clone: &str,
    refspec: &str,
) -> Result<String, crate::repo_clone::RepoCloneError> {
    host_git(
        host,
        Some(repo),
        &["fetch", "--quiet", "--no-tags", clone, refspec],
    )
}

/// Remove a PR's durable ref once the PR is terminal.
async fn drop_ref(host: &HostGit, checkout: &str, refname: &str) {
    let host = host.clone();
    let checkout = checkout.to_string();
    let refname = refname.to_string();
    let _ = blocking::unblock(move || {
        host_git(
            &host,
            Some(Path::new(&checkout)),
            &["update-ref", "-d", &refname],
        )
    })
    .await;
}

/// The serialized land of the PR's pinned commit `sha`: merge-tree
/// preflight, checkout-overlap check, merge, then the ref is dropped. Runs
/// inside `blocking::unblock` under the per-repo lock.
fn land(
    host: &HostGit,
    checkout: &str,
    refname: &str,
    sha: &str,
    number: i64,
    lock: &Mutex<()>,
) -> Result<String, String> {
    let git = |dir: Option<&Path>, args: &[&str]| host_git(host, dir, args);
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

/// One shell word: a value quoted so a POSIX shell reads it back unchanged.
struct ShellWord(String);

impl ShellWord {
    fn new(value: &str) -> Result<Self, String> {
        shlex::try_quote(value)
            .map(|quoted| Self(quoted.into_owned()))
            .map_err(|error| format!("{value:?} is no shell word: {error}"))
    }

    fn path(path: &Path) -> Result<Self, String> {
        Self::new(
            path.to_str()
                .ok_or_else(|| format!("{} is not UTF-8", path.display()))?,
        )
    }
}

impl std::fmt::Display for ShellWord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The body of a PR opened by `pr open`: its commit subjects and, for a
/// GitHub-backed project, the issue it closes.
#[derive(Template)]
#[template(path = "pr_body.md", escape = "none")]
struct PrBody<'a> {
    subjects: &'a [String],
    github_issue: Option<i64>,
}

/// What `pr merge` reports once a PR has landed: where it landed, and what
/// became of the session clone and of the PR's remote branch.
#[derive(Template)]
#[template(path = "pr_merged.txt", escape = "none")]
struct PrMerged<'a> {
    number: i64,
    /// The merge commit in the checkout; `None` for a PR merged upstream.
    merge_sha: Option<&'a str>,
    clone_dir: &'a str,
    clone: CloneFate,
    remote_branch: RemoteBranch,
}

/// What became of the session clone a PR landed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneFate {
    /// It held exactly the landed work and was deleted.
    Deleted,
    /// It holds work the PR did not land and was kept.
    Kept,
}

/// What became of a merged PR's branch on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteBranch {
    /// A local PR has no remote branch.
    Local,
    /// The branch is gone from the remote.
    Deleted(String),
    /// The branch could not be deleted, and why.
    NotDeleted(String, String),
}

/// The title and body of a PR: the given title, else the first commit's
/// subject (the only commit's when there is one).
fn pr_text(
    title: Option<&str>,
    subjects: &[String],
    github_issue: Option<i64>,
) -> Result<PrText, PrError> {
    let title = match (title, subjects.first()) {
        (Some(title), _) => title.to_string(),
        (None, Some(first)) => first.clone(),
        (None, None) => return Err(PrError::State("no commits to title the PR".to_string())),
    };
    let body = PrBody {
        subjects,
        github_issue,
    }
    .render()
    .map_err(|e| PrError::State(e.to_string()))?;
    Ok(PrText { title, body })
}

/// The published text of a PR.
struct PrText {
    title: String,
    body: String,
}

/// Makes the detached checkout of a PR's commit its check runs on; run in
/// the session root.
#[derive(Template)]
#[template(path = "pr_check_clone.sh", escape = "none")]
struct CheckClone {
    source: ShellWord,
    target: ShellWord,
    sha: ShellWord,
}

/// Prints the subjects of the commits `base..head`, oldest first; run in
/// the session clone.
#[derive(Template)]
#[template(path = "pr_commit_subjects.sh", escape = "none")]
struct CommitSubjects {
    base: ShellWord,
    head: ShellWord,
}

/// Removes a check's checkout; run in the session root.
#[derive(Template)]
#[template(path = "pr_check_remove.sh", escape = "none")]
struct RemoveCheckClone {
    target: ShellWord,
}

/// `gh` with the same error contract as [`git`].
pub(crate) fn gh(args: &[&str]) -> Result<String, PrError> {
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

/// `git` on the host through `host`, with its stdout trimmed.
fn host_git(
    host: &HostGit,
    dir: Option<&Path>,
    args: &[&str],
) -> Result<String, crate::repo_clone::RepoCloneError> {
    Ok(host.output(dir, args)?.trim().to_string())
}

/// Test fixtures' own git, on an empty configuration: neither the developer's
/// global or system configuration nor its hooks apply. A fixture repository
/// names its own identity.
#[cfg(test)]
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, crate::repo_clone::RepoCloneError> {
    host_git(&HostGit::from_file("/dev/null"), dir, args)
}

#[cfg(test)]
mod tests {
    use super::{
        BranchState, GitHubRest, PrContext, PrStore, PrTokenSource, git, github_branch,
        github_owner_repo, host_git, push_pinned,
    };
    use crate::commit_signing::test_user::TestUser;
    use crate::repo_clone::test_sandbox::{TestTerminal, arm_hostile_config, markers};
    use crate::repo_clone::{CloneSandbox, SessionRepos};
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

    /// A temporary directory under its canonical path, as the sandbox
    /// resolves paths.
    fn canonical_tempdir() -> tempfile::TempDir {
        tempfile::tempdir_in(dunce::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }

    struct Fixture {
        tmp: tempfile::TempDir,
        checkout: PathBuf,
        db: crate::db::ChatDb,
        repo_key: PathBuf,
        /// The user the harness signs as.
        user: TestUser,
        /// The session terminals the stores run on, alive as long as the
        /// fixture.
        terminals: Mutex<Vec<Arc<TestTerminal>>>,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = canonical_tempdir();
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
            let user = TestUser::new(&tmp.path().join("user"));
            Self {
                tmp,
                checkout,
                db,
                repo_key,
                user,
                terminals: Mutex::default(),
            }
        }

        /// The harness of session `id`, signing as the fixture's user.
        fn signing(&self, id: &str) -> crate::commit_signing::CommitSigning {
            self.user.signing(&self.tmp.path().join("may"), id)
        }

        /// The sandbox of session `id` as the daemon builds it: git there
        /// signs as the fixture's user through the harness.
        async fn signing_sandbox(&self, id: &str) -> CloneSandbox {
            let terminal =
                Arc::new(TestTerminal::signing(&self.session_root(id), self.signing(id)).await);
            let sandbox = terminal.sandbox();
            self.terminals.lock().unwrap().push(terminal);
            sandbox
        }

        fn session_root(&self, id: &str) -> PathBuf {
            self.tmp.path().join(id)
        }

        fn scratch_root(&self) -> PathBuf {
            self.tmp.path().join("scratch")
        }

        /// The sandbox of session `id`'s terminal, started on first use.
        async fn sandbox(&self, id: &str) -> CloneSandbox {
            let terminal = Arc::new(TestTerminal::new(&self.session_root(id)).await);
            let sandbox = terminal.sandbox();
            self.terminals.lock().unwrap().push(terminal);
            sandbox
        }

        /// A session's store with a clone of the checkout already made.
        async fn store(&self, id: &str, checks: Option<&str>) -> (PrStore, PathBuf) {
            let root = self.session_root(id);
            let clone = SessionRepos::new(&root, id, self.db.clone())
                .ensure_local_clone(&self.checkout, &self.repo_key, &self.scratch_root())
                .await
                .unwrap();
            self.user.identify(&clone);
            let projects = checks
                .map(|c| ProjectConfig {
                    path: self.checkout.clone(),
                    issues: ProjectIssues::default(),
                    checks: Some(c.to_string()),
                    pr: ProjectPr::default(),
                })
                .into_iter()
                .collect();
            let store = PrStore::new(
                PrContext {
                    db: Some(self.db.clone()),
                    session_id: id.to_string(),
                    session_root: root,
                    scratch_root: self.scratch_root(),
                    cwd: self.checkout.clone(),
                    workspace_config: WorkspaceConfig {
                        workdirs: Vec::new(),
                        projects,
                    },
                    screen: None,
                    signing: self.signing(id),
                    github_api: PrContext::default_github_api(),
                    task_events: None,
                    ci: None,
                    publish: None,
                },
                self.sandbox(id).await,
            );
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
            PrStore::new(
                PrContext {
                    db: Some(self.db.clone()),
                    session_id: id.to_string(),
                    session_root: self.session_root(id),
                    scratch_root: self.scratch_root(),
                    cwd: self.checkout.clone(),
                    workspace_config: WorkspaceConfig {
                        workdirs: Vec::new(),
                        projects: Vec::new(),
                    },
                    screen: None,
                    signing: self.signing(id),
                    github_api: GitHubRest::with_api_base(
                        PrTokenSource::Fixed("test-token".to_string()),
                        base,
                    ),
                    task_events: None,
                    ci: None,
                    publish: None,
                },
                self.sandbox(id).await,
            )
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

    fn range_of<'a>(
        signing: &'a crate::commit_signing::CommitSigning,
        base: &'a str,
    ) -> super::SignedRange<'a> {
        super::SignedRange { signing, base }
    }

    /// A bare remote whose default branch `main` holds `base`, fetched from
    /// `from`, as a hosted repository always has a default branch.
    fn seed_remote(remote: &Path, from: &Path, base: &str) {
        git(
            None,
            &[
                "init",
                "--bare",
                "-q",
                "-b",
                "main",
                &remote.to_string_lossy(),
            ],
        )
        .unwrap();
        git(
            Some(remote),
            &[
                "fetch",
                "-q",
                &from.to_string_lossy(),
                &format!("{base}:refs/heads/main"),
            ],
        )
        .unwrap();
    }

    fn commit_file(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), name).unwrap();
        git(Some(dir), &["add", "."]).unwrap();
        git(Some(dir), &["commit", "-m", name]).unwrap();
    }

    /// Without `--title`, a single commit titles the PR and is its body.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_without_a_title_takes_the_only_commits_subject() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "only.txt");

        store.open(None, None, None, true).await.unwrap();

        let key = fx.repo_key.to_str().unwrap();
        let pr = fx.db.get_pr(key, 1).await.unwrap().unwrap();
        assert_eq!(pr.title, "only.txt");
        assert_eq!(pr.body, "- only.txt\n");
    }

    /// Without `--title`, several commits title the PR with the first one's
    /// subject and the body lists them all, oldest first.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_without_a_title_takes_the_first_commits_subject() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "first.txt");
        commit_file(&clone, "second.txt");
        commit_file(&clone, "third.txt");

        store.open(None, None, None, true).await.unwrap();

        let key = fx.repo_key.to_str().unwrap();
        let pr = fx.db.get_pr(key, 1).await.unwrap().unwrap();
        assert_eq!(pr.title, "first.txt");
        assert_eq!(pr.body, "- first.txt\n- second.txt\n- third.txt\n");
    }

    /// A May issue number is internal: it is recorded, never published in
    /// the body.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_may_issue_is_not_linked_in_the_body() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "a.txt");

        store.open(None, None, Some(5), true).await.unwrap();

        let key = fx.repo_key.to_str().unwrap();
        let pr = fx.db.get_pr(key, 1).await.unwrap().unwrap();
        assert_eq!(pr.body, "- a.txt\n");
    }

    /// A GitHub-issue session's body closes the issue; an explicit title wins.
    #[test]
    fn a_github_issue_is_closed_by_the_body() {
        let subjects = vec!["fix(pr): title".to_string(), "test".to_string()];

        let text = super::pr_text(None, &subjects, Some(653)).unwrap();
        assert_eq!(text.title, "fix(pr): title");
        assert_eq!(text.body, "- fix(pr): title\n- test\n\nCloses #653\n");

        let text = super::pr_text(Some("chosen"), &subjects, None).unwrap();
        assert_eq!(text.title, "chosen");
        assert!(super::pr_text(None, &[], None).is_err());
    }

    #[tokio::test]
    async fn adopted_pr_lands_after_the_original_session_is_deleted() {
        let fx = Fixture::new().await;
        let (first, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        first.open(None, Some("work"), None, true).await.unwrap();
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
        // Inside the session: the check runs in its sandbox.
        let marker = fx.session_root("s1").join("checked");
        let (_, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        // The check records the tree it ran on, then commits in the clone.
        let cmd = format!(
            "test -f work.txt && ! test -f late.txt && git rev-parse HEAD > {marker} && \
             echo late > {clone}/late.txt && git -C {clone} add . && \
             git -C {clone} -c commit.gpgsign=false commit -q -m late",
            marker = marker.display(),
            clone = clone.display(),
        );
        // Same session and clone, now with the check configured.
        let (store, _) = fx.store("s1", Some(&cmd)).await;
        store.open(None, Some("work"), None, false).await.unwrap();
        let checked = std::fs::read_to_string(&marker).unwrap().trim().to_string();
        let landed = git(Some(&fx.checkout), &["rev-parse", "HEAD^2"]).unwrap();
        assert_eq!(landed, checked);
        assert!(!fx.checkout.join("late.txt").exists());
        // The throwaway check clone and the staged clone are gone and the
        // checkout has no worktrees.
        assert!(
            std::fs::read_dir(fx.session_root("s1").join("tmp"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pr-check-"))
        );
        assert_eq!(std::fs::read_dir(fx.scratch_root()).unwrap().count(), 0);
        assert!(
            !git(Some(&fx.checkout), &["worktree", "list"])
                .unwrap()
                .contains("pr-check")
        );
        // The clone moved on, so it is kept rather than deleted.
        assert!(clone.join("late.txt").exists());
    }

    /// A PR the session opens and closes reaches its `tasks.md` inbox, and
    /// the row naming it is told of each change once.
    #[tokio::test]
    async fn an_opened_and_closed_pr_reaches_the_tasks_inbox() {
        use aither_agent::ContextSource;

        let fx = Fixture::new().await;
        let (mut store, clone) = fx.store("s1", None).await;
        let events = aither_agent::TaskEvents::new();
        store.context.task_events = Some(events.clone());
        let sandbox = tempfile::tempdir().unwrap();
        std::fs::write(
            sandbox.path().join("tasks.md"),
            "## Doing\n- #3 work, PR #1 open\n",
        )
        .unwrap();
        let mut tasks = aither_agent::TasksDocument::new(sandbox.path(), 60, events);
        tasks.on_request(false).await;

        commit_file(&clone, "work.txt");
        store.open(None, Some("work"), None, true).await.unwrap();
        store.close(1, None).await.unwrap();

        tasks.on_request(false).await;
        let alert = tasks.on_tool_turn_end().await.expect("the row is told");
        let text = alert.content();
        // The row naming PR #1 already records its opening.
        assert_eq!(text.matches("PR #1 was opened").count(), 0, "{text}");
        assert_eq!(text.matches("PR #1 was closed").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn close_drops_the_ref() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        store.open(None, Some("work"), None, true).await.unwrap();
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
        store.open(None, Some("work"), None, true).await.unwrap();
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

    /// A store for session `id` whose cwd is the fixture checkout, which has
    /// the GitHub origin `o/r` and `workspace_config`; no clone exists yet.
    async fn trusted_store(
        fx: &Fixture,
        id: &str,
        workspace_config: WorkspaceConfig,
        github_api: GitHubRest<PrTokenSource>,
        screen: Option<Arc<super::ScreenFn>>,
    ) -> PrStore {
        PrStore::new(
            PrContext {
                db: Some(fx.db.clone()),
                session_id: id.to_string(),
                session_root: fx.session_root(id),
                scratch_root: fx.scratch_root(),
                cwd: fx.checkout.clone(),
                workspace_config,
                screen,
                signing: fx.signing(id),
                github_api,
                task_events: None,
                ci: None,
                publish: None,
            },
            fx.sandbox(id).await,
        )
    }

    /// Give the fixture checkout the GitHub origin `o/r`.
    fn add_github_origin(fx: &Fixture) {
        git(
            Some(&fx.checkout),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn local_branch_on_a_github_checkout_keeps_its_pr_local() {
        let fx = Fixture::new().await;
        let origin_repo = fx.tmp.path().join("origin.git");
        git(None, &["init", "--bare", &origin_repo.to_string_lossy()]).unwrap();
        let (api_base, requests) = mock_api(200).await;
        add_github_origin(&fx);
        let config = crate::trust::config_at(
            fx.tmp.path(),
            &fx.checkout,
            crate::trust::TrustLevel::LocalBranch,
        );
        let store = trusted_store(
            &fx,
            "s1",
            config,
            GitHubRest::with_api_base(PrTokenSource::Fixed("test-token".to_string()), &api_base),
            None,
        )
        .await;
        // Anything pushed to the origin would land in the bare repository.
        git(
            Some(&fx.checkout),
            &[
                "config",
                "remote.origin.pushurl",
                &origin_repo.to_string_lossy(),
            ],
        )
        .unwrap();

        let resolved = store.resolve(None).await.unwrap();
        assert!(resolved.record.local);
        assert_eq!(PrStore::backend_for(&resolved.record), ("may", None));
        fx.user.identify(&resolved.dir);
        commit_file(&resolved.dir, "work.txt");
        store.open(None, Some("work"), None, true).await.unwrap();

        let pr = fx
            .db
            .get_pr(resolved.record.repo_key.as_str(), 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pr.backend, "may");
        assert_eq!(pr.external_number, None);
        assert_eq!(git(Some(&origin_repo), &["for-each-ref"]).unwrap(), "");
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prs_only_finds_its_project_and_screens_without_a_github_issue_backend() {
        let fx = Fixture::new().await;
        let screened = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = screened.clone();
        let screen: Arc<super::ScreenFn> = Arc::new(move |title: &str, body: &str| {
            seen.lock().unwrap().push(title.to_string());
            seen.lock().unwrap().push(body.to_string());
            Box::pin(async { Ok(false) })
        });
        add_github_origin(&fx);
        // A `prs-only` project as existing files carry it: no `repository`
        // key and no GitHub issue backend.
        let config_path = fx.tmp.path().join("legacy-workspace.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[projects]]\npath = {:?}\n\n[projects.pr]\nbackend = \"github\"\nscreen = true\n",
                fx.checkout
            ),
        )
        .unwrap();
        let config = WorkspaceConfig::load(&config_path).unwrap();
        assert_eq!(
            crate::trust::trust_for(&config, &fx.repo_key, &fx.checkout),
            Some(crate::trust::TrustLevel::PrsOnly)
        );
        let store = trusted_store(
            &fx,
            "s1",
            config,
            PrContext::default_github_api(),
            Some(screen),
        )
        .await;
        // Stand in for the network clone: an existing directory is adopted.
        let seeded = store
            .repos()
            .unwrap()
            .path(&crate::repo_clone::CloneDir::for_repo_key("o/r"));
        std::fs::create_dir_all(&seeded).unwrap();

        let resolved = store.resolve(None).await.unwrap();
        assert_eq!(resolved.dir, seeded);
        assert!(!resolved.record.local);
        assert_eq!(
            PrStore::backend_for(&resolved.record),
            ("github", Some("o/r".to_string()))
        );
        let project = store.project_for_record(&resolved.record).unwrap();
        assert!(matches!(project.issues, ProjectIssues::May));
        assert_eq!(project.pr.screen, Some(true));
        let verdict = store
            .screen_outbound(&resolved.record, "a title", "- a subject\n")
            .await
            .unwrap();
        assert!(verdict.is_some(), "the screen ran and withheld the text");
        assert_eq!(
            *screened.lock().unwrap(),
            vec!["a title".to_string(), "- a subject\n".to_string()]
        );
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
        let scratch = tmp.path().join("scratch");
        let remote = tmp.path().join("remote.git");
        let url = remote.to_string_lossy().into_owned();
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
        let user = TestUser::new(&tmp.path().join("user"));
        let signing = user.signing(tmp.path(), "s1");
        user.identify(&clone);
        git(Some(&clone), &["remote", "rename", "origin", "upstream"]).unwrap();
        git(Some(&clone), &["checkout", "-b", "main"]).unwrap();
        commit_file(&clone, "base.txt");
        git(Some(&clone), &["push", "upstream", "main"]).unwrap();
        let base = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        let range = |base| range_of(&signing, base);

        // First PR: pushed to its own branch, then squash-merged upstream
        // (a new commit on main that is not a descendant of the PR head),
        // its branch left on the remote.
        commit_file(&clone, "one.txt");
        let first_sha = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        let first = github_branch("s1", &first_sha);
        push_pinned(
            &scratch,
            &clone,
            &url,
            &first_sha,
            &first,
            BranchState::New,
            range(&base),
        )
        .await
        .unwrap();
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
        let base = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        commit_file(&clone, "two.txt");
        let second_sha = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        let second = github_branch("s1", &second_sha);
        assert_ne!(first, second);
        push_pinned(
            &scratch,
            &clone,
            &url,
            &second_sha,
            &second,
            BranchState::New,
            range(&base),
        )
        .await
        .unwrap();
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
            push_pinned(
                &scratch,
                &clone,
                &url,
                &second_sha,
                &first,
                BranchState::Pushed,
                range(&base)
            )
            .await
            .is_err(),
            "pushes are never forced"
        );
        // A retry of the same PR reuses its branch.
        push_pinned(
            &scratch,
            &clone,
            &url,
            &second_sha,
            &second,
            BranchState::Pushed,
            range(&base),
        )
        .await
        .unwrap();
        // Every push staged its bare repository in scratch and removed it.
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    }

    /// The agent arms its clone with every executable setting, commits in
    /// the sandbox, and `pr` checks, pins and lands the work: what the clone
    /// configures runs in the sandbox, never on the host.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_clones_executable_config_never_runs_on_the_host() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", Some("test -f work.txt")).await;
        // The clone carries no identity of its own: the sandbox's git
        // configuration makes the commit the user's, signed by the harness.
        TestUser::unidentify(&clone);
        let root = fx.session_root("s1");
        let host = arm_hostile_config(&root, &clone, fx.tmp.path());
        std::fs::write(clone.join("work.txt"), "work").unwrap();
        // The agent's own commit, from its sandbox.
        fx.signing_sandbox("s1")
            .await
            .run(&clone, "git add -A && git commit -q -m work")
            .await
            .unwrap();

        let landed = store.open(None, Some("work"), None, false).await.unwrap();

        assert!(landed.starts_with("merged PR #1"), "{landed}");
        assert!(fx.checkout.join("work.txt").exists());
        assert_eq!(markers(&host), Vec::<String>::new(), "ran on the host");
        let signed = git(Some(&fx.checkout), &["cat-file", "commit", "HEAD^2"]).unwrap();
        assert!(signed.contains("gpgsig"), "{signed}");
        // The settings did run where the agent's commands run.
        let ran = markers(&root.join("ran"));
        for name in ["core.fsmonitor", "filter.hostile.clean", "pre-commit"] {
            assert!(ran.iter().any(|m| m == name), "{name} never ran: {ran:?}");
        }
    }

    /// A GitHub PR's push leaves from host scratch: the clone's remotes,
    /// hooks and transport settings take no part.
    #[tokio::test(flavor = "multi_thread")]
    async fn pushing_a_pr_runs_nothing_the_clone_configures() {
        let fx = Fixture::new().await;
        let (_, clone) = fx.store("s1", None).await;
        // Committed before the clone is armed, so the commit carries the
        // signature the user's own push hooks ask of it.
        commit_file(&clone, "work.txt");
        let root = fx.session_root("s1");
        let remote = fx.tmp.path().join("remote.git");
        let base = git(Some(&fx.checkout), &["rev-parse", "HEAD"]).unwrap();
        seed_remote(&remote, &fx.checkout, &base);
        let host = arm_hostile_config(&root, &clone, fx.tmp.path());
        // A remote the agent points elsewhere, over a hostile transport.
        git(
            Some(&clone),
            &["remote", "add", "upstream", "ssh://example.invalid/r.git"],
        )
        .unwrap();
        let sha = super::clone_head(&fx.user.host(), None, &clone.to_string_lossy()).unwrap();
        let signing = fx.signing("s1");

        push_pinned(
            &fx.scratch_root(),
            &clone,
            &remote.to_string_lossy(),
            &sha,
            "may/s1-work",
            BranchState::New,
            super::SignedRange {
                signing: &signing,
                base: &base,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            git(Some(&remote), &["rev-parse", "refs/heads/may/s1-work"]).unwrap(),
            sha
        );
        assert_eq!(markers(&host), Vec::<String>::new(), "ran on the host");
    }

    /// `pr open` refuses a range with a commit that has no signature, even
    /// below signed ones, and records nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_refuses_a_range_with_an_unsigned_commit() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "signed.txt");
        std::fs::write(clone.join("unsigned.txt"), "x").unwrap();
        git(Some(&clone), &["add", "."]).unwrap();
        git(
            Some(&clone),
            &["-c", "commit.gpgsign=false", "commit", "-m", "unsigned"],
        )
        .unwrap();
        commit_file(&clone, "signed-again.txt");

        let error = store
            .open(None, Some("work"), None, false)
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                super::PrError::Signing(crate::commit_signing::SigningError::Unsigned { .. })
            ),
            "{error}"
        );
        assert!(error.to_string().contains("no signature"), "{error}");
        assert!(store.list(None, None).await.unwrap().contains("no PRs"));
        assert!(!fx.checkout.join("signed.txt").exists());
    }

    /// A signed range opens and lands, and one signed by another key is
    /// refused like an unsigned one.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_accepts_only_the_users_signature() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        // Signed, but by a key that is not the user's.
        TestUser::new(&fx.tmp.path().join("stranger")).identify(&clone);
        commit_file(&clone, "work.txt");
        let error = store
            .open(None, Some("work"), None, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not the user's key"), "{error}");

        fx.user.identify(&clone);
        git(Some(&clone), &["commit", "--amend", "--no-edit", "-q"]).unwrap();
        let landed = store.open(None, Some("work"), None, false).await.unwrap();
        assert!(landed.starts_with("merged PR #1"), "{landed}");
    }

    /// A GitHub PR's push passes the user's global `pre-push` hook, which
    /// refuses commits without a signature; an unsigned range never gets
    /// that far.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_range_pushes_past_a_hook_that_refuses_unsigned_commits() {
        let fx = Fixture::new().await;
        let (_, clone) = fx.store("s1", None).await;
        let base = git(Some(&fx.checkout), &["rev-parse", "HEAD"]).unwrap();
        commit_file(&clone, "work.txt");
        let signed = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        git(
            Some(&clone),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "unsigned",
            ],
        )
        .unwrap();
        let unsigned = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        let remote = fx.tmp.path().join("remote.git");
        seed_remote(&remote, &fx.checkout, &base);
        let signing = fx.signing("s1");
        let range = || super::SignedRange {
            signing: &signing,
            base: &base,
        };

        // The user's hook, installed the way a global one reaches every
        // repository: `core.hooksPath` in their global configuration, which
        // only this fixture's host git reads.
        let ran = fx.tmp.path().join("hook-ran");
        fx.user.install_global_hook(
            "pre-push",
            &format!(
                "#!/bin/sh\ntouch '{}'\nwhile read local_ref local_sha remote_ref remote_sha; do\n  \
                 git cat-file commit \"$local_sha\" | grep -q '^gpgsig ' || {{ echo 'unsigned commit' >&2; exit 1; }}\ndone\n",
                ran.display()
            ),
        );
        let host = fx.user.host();

        // Sanity: the hook does refuse an unsigned commit.
        let staging = fx.tmp.path().join("staging.git");
        host_git(
            &host,
            None,
            &["init", "--bare", "-q", &staging.to_string_lossy()],
        )
        .unwrap();
        host_git(
            &host,
            Some(&staging),
            &["fetch", "-q", &clone.to_string_lossy(), &unsigned],
        )
        .unwrap();
        let refused = host_git(
            &host,
            Some(&staging),
            &[
                "push",
                "-q",
                &remote.to_string_lossy(),
                &format!("{unsigned}:refs/heads/x"),
            ],
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("unsigned commit"),
            "the hook refuses an unsigned commit: {refused}"
        );
        std::fs::remove_file(&ran).unwrap();

        // The unsigned range is refused before any push.
        let error = push_pinned(
            &fx.scratch_root(),
            &clone,
            &remote.to_string_lossy(),
            &unsigned,
            "may/unsigned",
            BranchState::New,
            range(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("no signature"), "{error}");
        assert!(!ran.exists(), "nothing was pushed");

        // The signed range goes through the hook.
        push_pinned(
            &fx.scratch_root(),
            &clone,
            &remote.to_string_lossy(),
            &signed,
            "may/signed",
            BranchState::New,
            range(),
        )
        .await
        .unwrap();
        assert!(ran.exists(), "the hook ran");
        assert_eq!(
            git(Some(&remote), &["rev-parse", "refs/heads/may/signed"]).unwrap(),
            signed
        );
    }

    /// The user's `pre-push` hook as a global one scans a push: per ref,
    /// the commits not on any remote-tracking ref, refused when one is not in
    /// the `allowed` list. A new branch scans `local_sha`, an existing one
    /// `local_sha ^remote_sha`.
    fn install_scanning_hook(user: &TestUser, allowed: &Path) {
        user.install_global_hook(
            "pre-push",
            &format!(
                "#!/bin/sh\nzero=0000000000000000000000000000000000000000\n\
                 while read local_ref local_sha remote_ref remote_sha; do\n  \
                 if [ \"$remote_sha\" = \"$zero\" ]; then range=\"$local_sha\"; \
                 else range=\"$local_sha ^$remote_sha\"; fi\n  \
                 revs=$(git rev-list $range --not --remotes) || exit 1\n  \
                 for c in $revs; do\n    \
                 grep -q \"^$c\\$\" '{}' || {{ echo \"refused $c\" >&2; exit 1; }}\n  done\n\
                 done\n",
                allowed.display()
            ),
        );
    }

    /// Staging repositories hold no remote-tracking refs of their own, yet a
    /// hook that scans what is not on a remote still passes: the push first
    /// fetches the remote's real state, for a new branch and for an update.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_scanning_pre_push_hook_sees_only_the_prs_commits() {
        let fx = Fixture::new().await;
        let (_, clone) = fx.store("s1", None).await;
        // Upstream history the hook would refuse if it were scanned: the
        // initial commit is not in the allowed list.
        let base = git(Some(&fx.checkout), &["rev-parse", "HEAD"]).unwrap();
        let remote = fx.tmp.path().join("remote.git");
        seed_remote(&remote, &fx.checkout, &base);
        let allowed = fx.tmp.path().join("allowed");
        install_scanning_hook(&fx.user, &allowed);
        let signing = fx.signing("s1");
        let range = || super::SignedRange {
            signing: &signing,
            base: &base,
        };

        commit_file(&clone, "one.txt");
        let one = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(&allowed, format!("{one}\n")).unwrap();
        let url = remote.to_string_lossy().into_owned();
        push_pinned(
            &fx.scratch_root(),
            &clone,
            &url,
            &one,
            "may/s1-work",
            BranchState::New,
            range(),
        )
        .await
        .unwrap();
        assert_eq!(
            git(Some(&remote), &["rev-parse", "refs/heads/may/s1-work"]).unwrap(),
            one
        );

        commit_file(&clone, "two.txt");
        let two = git(Some(&clone), &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(&allowed, format!("{two}\n")).unwrap();
        push_pinned(
            &fx.scratch_root(),
            &clone,
            &url,
            &two,
            "may/s1-work",
            BranchState::Pushed,
            range(),
        )
        .await
        .unwrap();
        assert_eq!(
            git(Some(&remote), &["rev-parse", "refs/heads/may/s1-work"]).unwrap(),
            two
        );
    }

    /// A check runs in the session's sandbox: it builds inside its checkout
    /// and cannot write anywhere outside the session.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_check_cannot_write_outside_the_sandbox() {
        let fx = Fixture::new().await;
        let outside = fx.tmp.path().join("outside");
        let cmd = format!("touch built && touch '{}'", outside.display());
        let (store, clone) = fx.store("s1", Some(&cmd)).await;
        commit_file(&clone, "work.txt");

        let err = store
            .open(None, Some("work"), None, false)
            .await
            .unwrap_err();

        assert!(matches!(err, super::PrError::CheckFailed(_)), "{err}");
        assert!(!outside.exists(), "the check wrote outside the sandbox");
        assert!(
            !fx.checkout.join("work.txt").exists(),
            "a failed check lands nothing"
        );
    }

    /// Merging deletes the clone, so a clone with uncommitted changes or
    /// untracked files refuses to merge and names them; once they are gone
    /// the merge lands and the clone with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dirty_clone_refuses_to_merge_and_lists_the_files() {
        let fx = Fixture::new().await;
        let (store, clone) = fx.store("s1", None).await;
        commit_file(&clone, "work.txt");
        store.open(None, Some("work"), None, true).await.unwrap();
        std::fs::write(clone.join("work.txt"), "edited").unwrap();
        std::fs::write(clone.join("notes.txt"), "scratch").unwrap();

        let err = store.merge(1, None).await.unwrap_err();

        assert!(matches!(err, super::PrError::DirtyClone(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("work.txt") && message.contains("notes.txt"),
            "{message}"
        );
        assert!(!fx.checkout.join("work.txt").exists(), "nothing landed");
        assert!(clone.join("notes.txt").exists(), "nothing was deleted");
        let key = fx.repo_key.to_str().unwrap();
        assert_eq!(fx.db.get_pr(key, 1).await.unwrap().unwrap().state, "open");

        git(Some(&clone), &["checkout", "--", "work.txt"]).unwrap();
        std::fs::remove_file(clone.join("notes.txt")).unwrap();
        let landed = store.merge(1, None).await.unwrap();

        assert!(landed.starts_with("merged PR #1 as "), "{landed}");
        assert!(landed.contains("was deleted"), "{landed}");
        assert!(fx.checkout.join("work.txt").exists());
        assert!(!clone.exists());
        assert!(fx.db.list_repo_clones("s1").await.unwrap().is_empty());
    }

    /// A merged PR's `may/*` branch is deleted through the API; one the
    /// repository already deleted counts as deleted, any other failure is
    /// reported, and a branch outside `may/*` is never touched.
    #[tokio::test]
    async fn a_merged_prs_remote_branch_is_deleted() {
        for (status, expected) in [
            (204, super::RemoteBranch::Deleted("may/s".to_string())),
            (422, super::RemoteBranch::Deleted("may/s".to_string())),
            (
                500,
                super::RemoteBranch::NotDeleted(
                    "may/s".to_string(),
                    "GitHub answered 500 Internal Server Error".to_string(),
                ),
            ),
        ] {
            let fx = Fixture::new().await;
            let (base, requests) = mock_api(status).await;
            let store = fx.github_store("s1", &base, "merged").await;

            assert_eq!(store.delete_remote_branch("o/r", "may/s").await, expected);
            assert_eq!(
                requests.lock().unwrap().clone(),
                vec![(
                    "DELETE".to_string(),
                    "/repos/o/r/git/refs/heads/may/s".to_string(),
                    "Bearer test-token".to_string(),
                    String::new(),
                )]
            );

            let refused = store.delete_remote_branch("o/r", "main").await;
            assert!(
                matches!(refused, super::RemoteBranch::NotDeleted(ref b, _) if b == "main"),
                "{refused:?}"
            );
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }

    /// What `pr merge` reports of the clone and the remote branch.
    #[test]
    fn the_merge_report_says_what_was_deleted() {
        use askama::Template as _;
        let render = |merge_sha, clone, remote_branch| {
            super::PrMerged {
                number: 3,
                merge_sha,
                clone_dir: "r-1a2b3c4d",
                clone,
                remote_branch,
            }
            .render()
            .unwrap()
        };
        assert_eq!(
            render(
                Some("abc"),
                super::CloneFate::Deleted,
                super::RemoteBranch::Local
            ),
            "merged PR #3 as abc\nThe session clone repos/r-1a2b3c4d was deleted, ignored files \
             included; the next `repo clone` or `pr open` for this repository clones it again at \
             the new HEAD.\n"
        );
        assert_eq!(
            render(
                None,
                super::CloneFate::Kept,
                super::RemoteBranch::NotDeleted("may/s".to_string(), "boom".to_string())
            ),
            "merged PR #3 upstream\nThe session clone repos/r-1a2b3c4d was kept: it holds commits \
             or changes this PR did not land.\nThe remote branch may/s was not deleted: boom\n"
        );
        assert!(
            render(
                None,
                super::CloneFate::Deleted,
                super::RemoteBranch::Deleted("may/s".to_string())
            )
            .ends_with("The remote branch may/s was deleted.\n")
        );
    }
}
