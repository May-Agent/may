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
use std::sync::{Arc, Mutex};

use askama::Template;
use zenwave::ResponseExt as _;

use crate::db::{ChatDb, PrFilter, PrState};
use crate::forge::{ForgeKind, ForgeRepo};
use crate::host_git::HostGit;
use crate::issue::IssueError;
use crate::issue::github::{ForgeTokenSource, GitHubRest, GitHubTokenSource};
use crate::issue::gitlab::{ForgeGitLabAuth, GitLabRest, GitLabTokenSource};
use crate::repo_clone::{CloneRecord, CloneSandbox, SessionRepos, uncommitted_changes};

pub(crate) mod checks;
pub(crate) mod gitlab;
pub(crate) mod merge_watch;
mod pull_sync;

use pull_sync::SyncedCheckout;

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
    /// A GitHub API call failed.
    #[error("github {0}")]
    Gh(String),
    /// A GitLab API call failed.
    #[error("gitlab {0}")]
    Gl(String),
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
    /// Outbound publish review wired to the fast model and local pre-screen.
    pub review: Option<crate::harness::publish_review::PublishReview>,
    /// The user's signing configuration: every commit of a PR must carry
    /// a signature valid under its key.
    pub signing: crate::commit_signing::CommitSigning,
    /// GitHub REST client the GitHub backend's direct calls go through.
    pub github_api: GitHubRest<PrTokenSource>,
    /// A GitLab REST client override, for tests; `None` builds one per
    /// repository host from `forge_auth`.
    pub gitlab_api: Option<GitLabRest<PrGitLabToken>>,
    /// The live forge authenticator; HTTPS pushes carry its credential.
    pub forge_auth: crate::forge::LiveForgeAuth,
    /// The per-repository land locks every session of the daemon shares.
    pub land_locks: LandLocks,
    /// Daemon-held per-repo default-branch checkouts updated when GitHub PRs merge.
    pub checkouts: Option<crate::repo_clone::DaemonCheckouts>,
    /// The main agent's `tasks.md` inbox: told when a PR opens, merges, is
    /// rejected or closes.
    pub task_events: Option<crate::harness::working_document::WorkEvents>,
    /// Asks the user before a PR is opened on a repository they do not own;
    /// `None` publishes without asking.
    pub publish: Option<crate::agent::publish_approval::PublishGate>,
    /// Records the durations of the PR's checks, when the session keeps
    /// them; `None` records nothing.
    pub ci: Option<CiWatch>,
    /// Announces PR state changes to live UI/TUI subscribers.
    pub ui_events: Option<crate::ui_event_bus::UiEventBus>,
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
    /// `github.com`'s credential in the live [`crate::forge::LiveForgeAuth`].
    Forge(ForgeTokenSource),
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
            Self::Forge(source) => source.token().await,
            #[cfg(test)]
            Self::Fixed(token) => Ok(token.clone()),
            #[cfg(test)]
            Self::Unauthenticated => Err(IssueError::GitHubAuth("not logged in".to_string())),
        }
    }
}

/// Token source for the GitLab backend's REST client.
#[derive(Debug, Clone)]
pub enum PrGitLabToken {
    /// The host's credential in the live [`crate::forge::LiveForgeAuth`].
    Forge(ForgeGitLabAuth),
    /// A fixed token, for tests.
    #[cfg(test)]
    Fixed(String),
    /// No GitLab login, for tests.
    #[cfg(test)]
    Unauthenticated,
}

impl GitLabTokenSource for PrGitLabToken {
    async fn token(&self) -> Result<String, IssueError> {
        match self {
            Self::Forge(source) => source.token().await,
            #[cfg(test)]
            Self::Fixed(token) => Ok(token.clone()),
            #[cfg(test)]
            Self::Unauthenticated => Err(IssueError::GitLabAuth("not logged in".to_string())),
        }
    }
}

impl PrContext {
    /// The production REST client: `api.github.com` with the live forge
    /// authenticator's `github.com` credential.
    #[must_use]
    pub fn default_github_api(auth: crate::forge::LiveForgeAuth) -> GitHubRest<PrTokenSource> {
        GitHubRest::new(PrTokenSource::Forge(ForgeTokenSource::new(auth)))
    }

    /// The REST client of GitLab `host`: `gitlab_api` for tests, else the
    /// host's `api/v4` endpoint with its `[forge]` credential.
    pub(crate) fn gitlab_rest(&self, host: &str) -> GitLabRest<PrGitLabToken> {
        self.gitlab_api.clone().unwrap_or_else(|| {
            GitLabRest::new(
                PrGitLabToken::Forge(ForgeGitLabAuth::new(
                    self.forge_auth.clone(),
                    host.to_string(),
                )),
                host,
            )
        })
    }
}

impl std::fmt::Debug for PrContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrContext")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// The forge a remote backend's merge path talks to.
enum Remote {
    /// A GitHub repository.
    GitHub(ForgeRepo),
    /// A GitLab project.
    GitLab(ForgeRepo),
}

/// The upstream a remote merge watches, per forge.
enum AnyUpstream<'a> {
    /// GitHub's PR through its REST and GraphQL APIs.
    GitHub(merge_watch::GithubPr<'a, PrTokenSource>),
    /// GitLab's MR through its REST API.
    GitLab(gitlab::GitLabMr<'a, PrGitLabToken>),
}

impl merge_watch::Upstream for AnyUpstream<'_> {
    async fn snapshot(&self) -> Result<merge_watch::Snapshot, PrError> {
        match self {
            Self::GitHub(upstream) => merge_watch::Upstream::snapshot(upstream).await,
            Self::GitLab(upstream) => merge_watch::Upstream::snapshot(upstream).await,
        }
    }

    async fn expected_checks(&self) -> Result<std::time::Duration, PrError> {
        match self {
            Self::GitHub(upstream) => merge_watch::Upstream::expected_checks(upstream).await,
            Self::GitLab(upstream) => merge_watch::Upstream::expected_checks(upstream).await,
        }
    }
}

impl Remote {
    /// The repository behind either forge.
    const fn repo(&self) -> &ForgeRepo {
        match self {
            Self::GitHub(repo) | Self::GitLab(repo) => repo,
        }
    }
}

/// Records a remote PR's check durations when its checks finish.
struct CiObserver<'a> {
    store: &'a PrStore,
    pr: &'a crate::db::PrRow,
    remote: &'a Remote,
    head_sha: &'a str,
}

impl merge_watch::ChecksObserver for CiObserver<'_> {
    async fn finished(&self) {
        self.store
            .observe_remote_checks(self.pr, self.remote, self.head_sha)
            .await;
    }
}

/// Serialized per-repo merge: one in-flight land per repository.
///
/// Cloning shares the locks, so the daemon hands one set to every session and
/// a land in one session excludes a land of the same repository in another.
/// The async locks let the GitHub queue/merge calls hold them across `.await`.
#[derive(Clone, Debug, Default)]
pub struct LandLocks(Arc<Mutex<HashMap<String, Arc<async_lock::Mutex<()>>>>>);

impl LandLocks {
    fn get(&self, repo_key: &str) -> Arc<async_lock::Mutex<()>> {
        let mut locks = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.entry(repo_key.to_string()).or_default().clone()
    }
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
            let mut repos =
                SessionRepos::new(&context.session_root, context.session_id.clone(), db)
                    .with_host(context.signing.host().clone())
                    .with_forge_auth(context.forge_auth.clone());
            if let Some(checkouts) = context.checkouts.clone() {
                repos = repos.with_checkouts(checkouts);
            }
            repos
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

    /// Which repo does `pr` mean: `--repo` wins (preferring an exact workspace
    /// directory name), then the session's unique workspace matching `cwd` or
    /// binding, then cloning on demand when none exists yet.
    async fn resolve(&self, repo: Option<&str>) -> Result<Resolved, PrError> {
        let repos = self.repos()?;
        let clones = repos.clones().await?;
        if let Some(name) = repo {
            if let Some(record) = clones.iter().find(|c| c.dir.as_str() == name) {
                return Ok(Resolved {
                    dir: repos.path(&record.dir),
                    record: record.clone(),
                });
            }
            let by_key: Vec<_> = clones.iter().filter(|c| c.repo_key == name).collect();
            return match by_key.as_slice() {
                [record] => Ok(Resolved {
                    dir: repos.path(&record.dir),
                    record: (*record).clone(),
                }),
                [] => Err(PrError::Resolve(format!("no session clone named {name}"))),
                _ => Err(PrError::Resolve(format!(
                    "several session workspaces exist for {name}; run inside one or pass its workspace directory name"
                ))),
            };
        }
        // The repository the cwd sits in, when it has one unambiguous workspace.
        let cwd_key = crate::repo_search::worktree_root(&self.context.cwd)
            .and_then(|worktree| {
                crate::repo_clone::CloneSource::of(
                    &worktree,
                    &self.context.workspace_config,
                    &crate::config::MayConfig::default(),
                )
                .ok()
                .flatten()
            })
            .map(|source| source.key());
        if let Some(key) = cwd_key.as_deref() {
            let matching: Vec<_> = clones.iter().filter(|c| c.repo_key == key).collect();
            match matching.as_slice() {
                [record] => {
                    return Ok(Resolved {
                        dir: repos.path(&record.dir),
                        record: (*record).clone(),
                    });
                }
                [] => {}
                _ => {
                    return Err(PrError::Resolve(
                        "several session clones exist; run inside one or pass --repo".to_string(),
                    ));
                }
            }
        }
        // The session's own binding names its repository, checkout or not.
        if let Some(binding) = self.session_binding().await? {
            let matching: Vec<_> = clones
                .iter()
                .filter(|c| c.repo_key == binding.clone_key())
                .collect();
            match matching.as_slice() {
                [record] => {
                    return Ok(Resolved {
                        dir: repos.path(&record.dir),
                        record: (*record).clone(),
                    });
                }
                [] => {}
                _ => {
                    return Err(PrError::Resolve(
                        "several session clones exist; run inside one or pass --repo".to_string(),
                    ));
                }
            }
            let dir = repos
                .ensure_clone_of_binding(
                    &binding,
                    &self.context.workspace_config,
                    &self.context.scratch_root,
                )
                .await?
                .ok_or_else(|| {
                    PrError::Resolve(
                        "repository trust is undecided or binds no clone — run `repo trust set <full|prs-only|local-branch>`".to_string(),
                    )
                })?;
            return Self::registered(repos, dir).await;
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
        // No clones yet: clone the cwd's repo on demand, resolving its
        // trust first (a persisted decision wins; the rest resolves from
        // the viewer's GitHub access).
        let worktree = crate::repo_search::worktree_root(&self.context.cwd)
            .ok_or_else(|| PrError::Resolve("not inside a repository".to_string()))?;
        let config_path =
            crate::session::SessionPaths::new(self.context.session_root.clone()).workspace_config();
        let probe = crate::trust::ForgeProbes::new(
            self.context.github_api.clone(),
            self.context.forge_auth.clone(),
        );
        let level = crate::trust::resolve_checkout(&config_path, &worktree, &probe)
            .await
            .map_err(|e| PrError::Resolve(format!("repository trust: {e}")))?;
        if !level.clones_work() {
            return Err(PrError::Resolve(
                "local trust writes the checkout; `pr` is for full, prs-only and local-branch"
                    .to_string(),
            ));
        }
        // The resolution may have persisted a level: read the config afresh
        // so a GitHub level picks the remote clone.
        let config = crate::workspace::WorkspaceConfig::load(&config_path)
            .map_err(|e| PrError::Resolve(format!("workspace config: {e}")))?;
        let dir = repos
            .ensure_clone_of(&worktree, &config, &self.context.scratch_root)
            .await?
            .ok_or_else(|| {
                PrError::Resolve("trust binds no clone for this repository".to_string())
            })?;
        Self::registered(repos, dir).await
    }

    /// Resolve the workspace for an existing PR `#number`, consulting
    /// `pr_workspaces` when `repo` leaves ambiguity between multiple concurrent
    /// workspaces on the same repository.
    async fn resolve_for_pr(
        &self,
        repo: Option<&str>,
        number: i64,
    ) -> Result<(Resolved, crate::db::PrRow), PrError> {
        let repos = self.repos()?;
        let clones = repos.clones().await?;
        let db = self.db()?;
        // Exact workspace directory always wins.
        if let Some(name) = repo
            && let Some(record) = clones.iter().find(|c| c.dir.as_str() == name)
        {
            let pr = db
                .get_pr(&record.repo_key, number)
                .await?
                .ok_or_else(|| PrError::State(format!("no PR #{number}")))?;
            return Ok((
                Resolved {
                    dir: repos.path(&record.dir),
                    record: record.clone(),
                },
                pr,
            ));
        }
        let candidate_key = if let Some(name) = repo {
            Some(name.to_string())
        } else if let Some(key) = crate::repo_search::worktree_root(&self.context.cwd)
            .and_then(|w| {
                crate::repo_clone::CloneSource::of(
                    &w,
                    &self.context.workspace_config,
                    &crate::config::MayConfig::default(),
                )
                .ok()
                .flatten()
            })
            .map(|s| s.key())
        {
            Some(key)
        } else if let Some(binding) = self.session_binding().await? {
            Some(binding.clone_key())
        } else {
            let mut keys: Vec<String> = clones.iter().map(|c| c.repo_key.clone()).collect();
            keys.sort();
            keys.dedup();
            if keys.len() == 1 { keys.pop() } else { None }
        };
        if let Some(key) = candidate_key
            && let Some(pr) = db.get_pr(&key, number).await?
            && let Some(bound_dir) = db.pr_workspace(pr.id).await?
            && let Some(record) = clones
                .iter()
                .find(|c| c.repo_key == key && c.dir.as_str() == bound_dir)
        {
            return Ok((
                Resolved {
                    dir: repos.path(&record.dir),
                    record: record.clone(),
                },
                pr,
            ));
        }
        let resolved = self.resolve(repo).await?;
        let pr = db
            .get_pr(&resolved.record.repo_key, number)
            .await?
            .ok_or_else(|| PrError::State(format!("no PR #{number}")))?;
        Ok((resolved, pr))
    }

    /// Resolve only the repository identity for read-only queries (`list`, `show`).
    async fn resolve_repo_key(&self, repo: Option<&str>) -> Result<String, PrError> {
        let clones = self.repos()?.clones().await?;
        if let Some(name) = repo {
            if let Some(record) = clones
                .iter()
                .find(|c| c.dir.as_str() == name || c.repo_key == name)
            {
                return Ok(record.repo_key.clone());
            }
            if let Some(project) = self.context.workspace_config.projects.iter().find(|p| {
                p.github_repository()
                    .is_some_and(|r| format!("{}/{}", r.owner(), r.repo()) == name)
                    || p.path.as_ref().is_some_and(|w| {
                        crate::issue::repository_identity(Path::new(w))
                            .is_ok_and(|id| p.has_identity(&id) && name == w)
                    })
            }) {
                if let Some(gh) = project.github_repository() {
                    return Ok(format!(
                        "https://github.com/{}/{}.git",
                        gh.owner(),
                        gh.repo()
                    ));
                }
                if let Some(path) = &project.path
                    && let Ok(identity) = crate::issue::repository_identity(Path::new(path))
                {
                    return Ok(identity.display().to_string());
                }
            }
            if let Ok(Some(source)) = crate::repo_clone::CloneSource::of(
                Path::new(name),
                &self.context.workspace_config,
                &crate::config::MayConfig::default(),
            ) {
                return Ok(source.key());
            }
            if let Ok(resolved) = self.resolve(Some(name)).await {
                return Ok(resolved.record.repo_key);
            }
            if let Ok(db) = self.db() {
                let summaries = db
                    .list_prs_with_repo(None, None, PrFilter::All, false)
                    .await?;
                if let Some(found) = summaries.into_iter().find(|pr| {
                    pr.repo_key == name
                        || github_owner_repo(&pr.repo_key).as_deref() == Some(name)
                        || ForgeRepo::parse_stored(name)
                            .is_ok_and(|repo| repo.cache_key() == pr.repo_key)
                }) {
                    return Ok(found.repo_key);
                }
            }
            return Err(PrError::Resolve(format!("no session clone named {name}")));
        }
        if let Some(worktree) = crate::repo_search::worktree_root(&self.context.cwd)
            && let Ok(Some(source)) = crate::repo_clone::CloneSource::of(
                &worktree,
                &self.context.workspace_config,
                &crate::config::MayConfig::default(),
            )
        {
            return Ok(source.key());
        }
        if let Some(binding) = self.session_binding().await? {
            return Ok(binding.clone_key());
        }
        let resolved = self.resolve(None).await.or_else(|err| {
            let mut keys: Vec<String> = clones.iter().map(|c| c.repo_key.clone()).collect();
            keys.sort();
            keys.dedup();
            if let [only] = keys.as_slice() {
                Ok(Resolved {
                    dir: PathBuf::new(),
                    record: clones.iter().find(|c| &c.repo_key == only).unwrap().clone(),
                })
            } else {
                Err(err)
            }
        })?;
        Ok(resolved.record.repo_key)
    }

    /// The repository the session is bound to, when it has a binding.
    async fn session_binding(&self) -> Result<Option<crate::session::RepoBinding>, PrError> {
        let row = self.db()?.get_session(&self.context.session_id).await?;
        row.and_then(|row| row.repo)
            .map(|stored| crate::session::RepoBinding::parse(&stored))
            .transpose()
            .map_err(|error| PrError::Resolve(error.to_string()))
    }

    /// The registry record of the clone in `dir`.
    async fn registered(repos: &SessionRepos, dir: PathBuf) -> Result<Resolved, PrError> {
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
        let has_workspace_pr = self
            .db()?
            .open_pr_for_workspace(
                &resolved.record.repo_key,
                &self.context.session_id,
                resolved.record.dir.as_str(),
            )
            .await?
            .is_some();
        let has_legacy_pr = if has_workspace_pr {
            false
        } else if let Some(existing) = self
            .db()?
            .open_pr_for(&resolved.record.repo_key, &self.context.session_id)
            .await?
        {
            // Only treat a live PR on the same repo as belonging to this
            // workspace when it has no bound workspace of its own.
            self.db()?.pr_workspace(existing.id).await?.is_none()
        } else {
            false
        };
        if has_workspace_pr || has_legacy_pr {
            return Err(PrError::State(
                "this workspace already has an open PR — merge or close it first".to_string(),
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
        let (detected, forge_repo) = self.backend_for(&resolved.record);
        self.check_pinned_backend(&resolved.record, detected)?;
        let backend = detected;
        let subjects = self
            .commit_subjects(&resolved, &base_sha, &head_sha)
            .await?;
        // Only a forge issue is linked in published text: a May issue
        // number is internal and never leaves the machine.
        let github_issue = issue_number.filter(|_| {
            self.project_for_record(&resolved.record).is_some_and(|p| {
                p.forge_repository(&crate::config::MayConfig::default())
                    .is_some()
            })
        });
        let text = pr_text(title, &subjects, github_issue)?;
        let (title, body) = (text.title.as_str(), text.body.as_str());
        // A local PR's branch is its durable ref, named once its number is known.
        let branch = if resolved.record.local {
            String::new()
        } else {
            remote_branch(&self.context.session_id, &head_sha)
        };
        // Review outbound PR title, body, and commit messages before anything
        // leaves the machine.
        if forge_repo.is_some() {
            let commits = self
                .commit_messages(&resolved, &base_sha, &head_sha)
                .await?;
            let commit_refs: Vec<_> = commits.iter().map(OwnedCommit::as_ref).collect();
            self.review_outbound(crate::harness::publish_review::Publication::pull_request(
                title,
                body,
                &commit_refs,
            ))
            .await?;
        }
        if let (Some(gate), Some(repo)) = (&self.context.publish, &forge_repo) {
            match repo.kind() {
                ForgeKind::GitHub => {
                    gate.approve_forge_pull_request(&self.context.github_api, repo, title, body)
                        .await?;
                }
                ForgeKind::GitLab => {
                    let rest = self.context.gitlab_rest(repo.host().as_str());
                    gate.approve_forge_pull_request(&rest, repo, title, body)
                        .await?;
                }
            }
        }
        // Forge-bound: push the session branch before recording, so the
        // row never points at a PR that failed to exist upstream.
        let upstream_number = if let Some(repo) = &forge_repo {
            Some(
                self.push_and_create(&resolved, &branch, &base_sha, &head_sha, &text, repo)
                    .await?,
            )
        } else {
            self.verify_local(&resolved, &base_sha, &head_sha).await?;
            None
        };
        let project = self.project_for_record(&resolved.record);
        let config_hold = project.is_some_and(|p| p.pr.auto_merge == Some(false));
        let require_review = project.is_some_and(|p| p.pr.require_review == Some(true));
        let effective_hold = hold || config_hold || require_review;
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
                !effective_hold,
                issue_number,
                upstream_number,
            )
            .await?;
        // The PR owns its commit from here on: pin it once, land only it.
        let db = self.db()?;
        db.set_pr_workspace(pr.id, resolved.record.dir.as_str())
            .await?;
        if resolved.record.local {
            db.set_pr_branch(pr.id, &pin_ref(pr.number)).await?;
            self.pin(&resolved, pr.number, &head_sha).await?;
        }
        let pr = db
            .update_pr(pr.id, PrState::Open, Some(&head_sha), None, None)
            .await?;
        self.event(&pr, "opened", Some(&format!("base {base_sha}")))
            .await;
        self.task_event(
            &pr,
            crate::harness::working_document::WorkEvent::PullRequestOpened,
        );
        if let Some(n) = upstream_number {
            let label = match forge_repo.map(|repo| repo.kind()) {
                Some(ForgeKind::GitLab) => format!("gitlab MR !{n}"),
                _ => format!("github PR #{n}"),
            };
            self.event(&pr, "opened", Some(&label)).await;
        }
        if let Some(n) = issue_number {
            self.event(&pr, "issue_linked", Some(&format!("issue #{n}")))
                .await;
        }
        let sigil = pr.sigil();
        if require_review && !hold && !config_hold {
            self.event(
                &pr,
                "review_required",
                Some("user review required before merge"),
            )
            .await;
            return Ok(format!("opened PR {sigil}{} (awaiting review)", pr.number));
        }
        if effective_hold {
            return Ok(format!("opened PR {sigil}{} (hold)", pr.number));
        }
        self.merge(pr.number, Some(resolved.record.dir.as_str()))
            .await
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
        let (resolved, pr) = self.resolve_for_pr(repo, number).await?;
        if pr.session_id != self.context.session_id {
            return Err(PrError::Ownership(format!(
                "PR #{number} belongs to another session — adopt it with `pr adopt {number}`"
            )));
        }
        if self
            .project_for_record(&resolved.record)
            .is_some_and(|p| p.pr.require_review == Some(true))
        {
            return Err(PrError::State(format!(
                "PR #{number} requires user review before merging"
            )));
        }
        if pr.state.is_terminal() {
            return Err(PrError::State(format!("PR #{number} is {}", pr.state)));
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
            .update_pr(pr.id, PrState::ChecksPending, Some(&head_sha), None, None)
            .await?;
        self.event(&pr, "checks_pending", None).await;
        if matches!(pr.backend.as_str(), "github" | "gitlab") {
            return self.merge_remote(&resolved, &pr, &head_sha).await;
        }
        // `[[projects]] checks` — the local check command runs on a detached
        // checkout of the pinned commit.
        if let Some(cmd) = self.check_command(&resolved.record)
            && let Err(err) = self.check_pinned(&resolved, &pr, &head_sha, &cmd).await
        {
            self.db()?
                .update_pr(pr.id, PrState::Failed, Some(&head_sha), None, None)
                .await?;
            self.event(&pr, "check_failed", Some(&err)).await;
            return Err(PrError::CheckFailed(err));
        }
        // Serialized land: one in-flight merge per repository.
        let lock = self.context.land_locks.get(&resolved.record.repo_key);
        self.event(&pr, "merging", None).await;
        let result = blocking::unblock({
            let checkout = resolved.record.source.clone();
            let refname = pr.branch.clone();
            let base = pr.base_sha.clone();
            let sha = head_sha.clone();
            let number = pr.number;
            let host = self.context.signing.host().clone();
            move || land(&host, &checkout, &refname, &base, &sha, number, &lock)
        })
        .await;
        let now = jiff::Timestamp::now().to_string();
        match result {
            Ok(merge_sha) => {
                self.db()?
                    .update_pr(
                        pr.id,
                        PrState::Merged,
                        Some(&head_sha),
                        Some(&merge_sha),
                        Some(&now),
                    )
                    .await?;
                self.event(&pr, "merged", Some(&merge_sha)).await;
                close_linked_may_issue(
                    self.db()?,
                    &self.context.workspace_config,
                    &resolved.record.repo_key,
                    &pr,
                    self.project_for_record(&resolved.record),
                )
                .await;
                self.task_event(
                    &pr,
                    crate::harness::working_document::WorkEvent::PullRequestMerged,
                );
                let clone = self.retire_clone(&resolved, &head_sha).await?;
                Ok(PrMerged {
                    sigil: pr.sigil(),
                    number,
                    merge_sha: Some(&merge_sha),
                    clone_dir: resolved.record.dir.as_str(),
                    clone,
                    remote_branch: RemoteBranch::Local,
                    pull_sync: Vec::new(),
                }
                .render()
                .expect("the merge result renders"))
            }
            Err(reason) => {
                self.db()?
                    .update_pr(pr.id, PrState::Rejected, Some(&head_sha), None, None)
                    .await?;
                self.event(&pr, "rejected", Some(&reason)).await;
                self.task_event(
                    &pr,
                    crate::harness::working_document::WorkEvent::PullRequestRejected,
                );
                Err(PrError::Rejected(reason))
            }
        }
    }

    /// `pr close <n>` — abandon an own, unmerged PR.
    ///
    /// # Errors
    /// Fails on ownership or a terminal state.
    pub async fn close(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let (resolved, pr) = self.resolve_for_pr(repo, number).await?;
        if pr.session_id != self.context.session_id {
            return Err(PrError::Ownership(format!(
                "PR #{number} belongs to another session — adopt it with `pr adopt {number}`"
            )));
        }
        // A terminal PR is refused before anything leaves the machine; the
        // transition below re-checks atomically.
        if pr.state.is_terminal() {
            return Err(PrError::State(format!(
                "PR #{number} is already {}",
                pr.state
            )));
        }
        // Closing upstream first disables auto-merge; if it fails the local
        // row stays as it was.
        match pr.backend.as_str() {
            "github" => {
                let owner_repo = github_owner_repo(&resolved.record.source)
                    .ok_or_else(|| PrError::Resolve("github PR without a repo".to_string()))?;
                self.close_github(&owner_repo, pr.number).await?;
            }
            "gitlab" => {
                let repo = self
                    .forge_of(&resolved.record)
                    .ok_or_else(|| PrError::Resolve("gitlab MR without a repo".to_string()))?;
                let rest = self.context.gitlab_rest(repo.host().as_str());
                gitlab::close_mr(&rest, &repo, pr.number).await?;
            }
            _ => {}
        }
        self.db()?.close_pr(pr.id).await?;
        self.event(&pr, "closed", None).await;
        self.task_event(
            &pr,
            crate::harness::working_document::WorkEvent::PullRequestClosed,
        );
        if resolved.record.local {
            drop_ref(
                self.context.signing.host(),
                &resolved.record.source,
                &pr.branch,
            )
            .await;
        }
        self.retire_closed_clone(&resolved, pr.head_sha.as_deref())
            .await?;
        Ok(format!("closed PR #{number}"))
    }

    /// `pr update <n>` — re-pin an own, unlanded PR to the clone's current
    /// HEAD. The only way a PR's commit changes after it was opened.
    ///
    /// # Errors
    /// Fails on ownership, a terminal state, or a dirty clone.
    pub async fn update(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let (resolved, pr) = self.resolve_for_pr(repo, number).await?;
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
        if matches!(pr.backend.as_str(), "github" | "gitlab") {
            let since = pr.head_sha.as_deref().unwrap_or(&pr.base_sha);
            let commits = match self.commit_messages(&resolved, since, &head_sha).await {
                Ok(commits) => commits,
                Err(_) if pr.head_sha.is_some() => {
                    self.commit_messages(&resolved, &pr.base_sha, &head_sha)
                        .await?
                }
                Err(error) => return Err(error),
            };
            let commit_refs: Vec<_> = commits.iter().map(OwnedCommit::as_ref).collect();
            if !commit_refs.is_empty() {
                self.review_outbound(
                    crate::harness::publish_review::Publication::commit_messages(&commit_refs),
                )
                .await?;
            }
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
                push_https_auth(&self.context.forge_auth, &resolved.record.source).await,
            )
            .await?;
        } else {
            self.verify_local(&resolved, &pr.base_sha, &head_sha)
                .await?;
            self.pin(&resolved, pr.number, &head_sha).await?;
        }
        self.db()?
            .update_pr(pr.id, PrState::Open, Some(&head_sha), None, None)
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
        let (_resolved, pr) = self.resolve_for_pr(repo, number).await?;
        if pr.session_id == self.context.session_id {
            return Ok(format!("PR #{number} is already this session's"));
        }
        let previous = pr.session_id.clone();
        self.db()?.adopt_pr(pr.id, &self.context.session_id).await?;
        let detail = format!("from session {previous}");
        self.event(&pr, "adopted", Some(&detail)).await;
        self.notify_prs(&self.context.session_id);
        Ok(format!("adopted PR #{number} (was session {previous})"))
    }

    /// `pr list` — every PR on the repo (or all repos when no repo scope is
    /// implied), newest first, with repo, state, checks, and provenance.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list(&self, repo: Option<&str>, filter: PrFilter) -> Result<String, PrError> {
        let key = if let Some(name) = repo {
            Some(self.resolve_repo_key(Some(name)).await?)
        } else if let Some(worktree) = crate::repo_search::worktree_root(&self.context.cwd)
            && let Ok(Some(source)) = crate::repo_clone::CloneSource::of(
                &worktree,
                &self.context.workspace_config,
                &crate::config::MayConfig::default(),
            )
        {
            Some(source.key())
        } else if let Some(binding) = self.session_binding().await? {
            Some(binding.clone_key())
        } else {
            let clones = self.repos()?.clones().await?;
            let mut keys: Vec<String> = clones.into_iter().map(|c| c.repo_key).collect();
            keys.sort();
            keys.dedup();
            if keys.len() == 1 { keys.pop() } else { None }
        };
        let db = self.db()?;
        let rows = db
            .list_prs_with_repo(key.as_deref(), None, filter, false)
            .await?;
        if rows.is_empty() {
            return Ok("no PRs".to_string());
        }
        let mut items = Vec::with_capacity(rows.len());
        for pr in rows {
            let events = db.pr_events(pr.id).await?;
            let check_state = db.pr_check_state(pr.id).await?;
            let opened = pr_opened_provenance(&events);
            let detail = pr_failure_detail(&events, pr.state, check_state.as_ref());
            let checks = format_cached_checks(check_state.as_ref());
            items.push(PrListItemView {
                sigil: crate::db::pr_sigil(&pr.backend),
                number: pr.number,
                state: pr.state,
                title: pr.title,
                merge_commit_sha: pr.merge_commit_sha,
                orphaned: pr.orphaned,
                repo_key: pr.repo_key,
                backend: pr.backend,
                session_id: pr.session_id,
                opened,
                checks,
                detail,
            });
        }
        PrListTemplate { prs: &items }
            .render()
            .map(|text| text.trim_end().to_string())
            .map_err(|e| PrError::State(e.to_string()))
    }

    /// `pr show <n>` — one PR with its state, checks, failure detail, and
    /// full provenance/event trail. Works even after clone retirement or from
    /// another session or subagent.
    ///
    /// # Errors
    /// Fails when the PR does not exist or its repository is ambiguous.
    pub async fn show(&self, number: i64, repo: Option<&str>) -> Result<String, PrError> {
        let key = if let Some(name) = repo {
            Some(self.resolve_repo_key(Some(name)).await?)
        } else {
            self.resolve_repo_key(None).await.ok()
        };
        let db = self.db()?;
        let mut matches: Vec<_> = db
            .list_prs_with_repo(key.as_deref(), None, PrFilter::All, false)
            .await?
            .into_iter()
            .filter(|pr| pr.number == number)
            .collect();
        if matches.is_empty() && key.is_some() && repo.is_none() {
            matches = db
                .list_prs_with_repo(None, None, PrFilter::All, false)
                .await?
                .into_iter()
                .filter(|pr| pr.number == number)
                .collect();
        }
        let mut iter = matches.into_iter();
        let pr = match (iter.next(), iter.next()) {
            (None, _) => return Err(PrError::State(format!("no PR #{number}"))),
            (Some(row), None) => row,
            (Some(_), Some(_)) => {
                return Err(PrError::Resolve(format!(
                    "several repositories have PR #{number}; pass --repo"
                )));
            }
        };
        let events = db.pr_events(pr.id).await?;
        let check_state = db.pr_check_state(pr.id).await?;
        let opened = pr_opened_provenance(&events);
        let detail = pr_failure_detail(&events, pr.state, check_state.as_ref());
        // The live check rollup for a remote PR still in flight — one
        // snapshot, best-effort, skipped for the may backend and for
        // states the forge no longer reports checks on.
        let checks = if pr.backend == "github"
            && pr.state.is_in_flight()
            && let Some(owner_repo) = github_owner_repo(&pr.repo_key)
        {
            let github = merge_watch::GithubPr {
                api: &self.context.github_api,
                owner_repo: &owner_repo,
                number: pr.number,
            };
            let actions = crate::ci::github::GhActions::new(
                self.context.github_api.clone(),
                &owner_repo,
                pr.number,
            );
            let section = match merge_watch::Upstream::snapshot(&github).await {
                Ok(snapshot) => {
                    let report = checks::ChecksReport::of(&snapshot);
                    let failures = checks::failure_details(&actions, &report).await;
                    checks::ChecksSection::Read(checks::ReadChecks::new(&report, failures))
                }
                Err(error) => checks::ChecksSection::Unreadable {
                    forge: "GitHub",
                    error: error.to_string(),
                },
            };
            Some(
                section
                    .render()
                    .map_err(|e| PrError::State(e.to_string()))?,
            )
        } else if pr.backend == "gitlab" && pr.state.is_in_flight() {
            let section = match ForgeRepo::from_cache_key(ForgeKind::GitLab, &pr.repo_key) {
                Ok(repo) => {
                    let rest = self.context.gitlab_rest(repo.host().as_str());
                    let mr = gitlab::GitLabMr {
                        rest: &rest,
                        repo: &repo,
                        iid: pr.number,
                    };
                    let ci =
                        crate::ci::gitlab::GitLabCi::new(rest.clone(), repo.clone(), pr.number);
                    match merge_watch::Upstream::snapshot(&mr).await {
                        Ok(snapshot) => {
                            let report = checks::ChecksReport::of(&snapshot);
                            let failures = checks::failure_details(&ci, &report).await;
                            checks::ChecksSection::Read(checks::ReadChecks::new(&report, failures))
                        }
                        Err(error) => checks::ChecksSection::Unreadable {
                            forge: "GitLab",
                            error: error.to_string(),
                        },
                    }
                }
                Err(error) => checks::ChecksSection::Unreadable {
                    forge: "GitLab",
                    error: error.to_string(),
                },
            };
            Some(
                section
                    .render()
                    .map_err(|e| PrError::State(e.to_string()))?,
            )
        } else {
            format_cached_checks(check_state.as_ref()).map(|summary| format!("checks: {summary}"))
        };
        let event_views: Vec<PrEventView> = events
            .into_iter()
            .map(|e| PrEventView {
                created_at: e.created_at,
                kind: e.kind,
                agent_id: e.agent_id,
                session_id: e.session_id,
                message_id: e.message_id,
                detail: e.detail,
            })
            .collect();
        PrShowTemplate {
            sigil: crate::db::pr_sigil(&pr.backend),
            number: pr.number,
            state: pr.state,
            title: &pr.title,
            repo_key: &pr.repo_key,
            session_id: &pr.session_id,
            orphaned: pr.orphaned,
            backend: &pr.backend,
            base_sha: &pr.base_sha,
            head_sha: pr.head_sha.as_deref().unwrap_or("-"),
            merge_commit_sha: pr.merge_commit_sha.as_deref(),
            opened: opened.as_ref(),
            checks: checks.as_deref(),
            detail: detail.as_deref(),
            events: &event_views,
        }
        .render()
        .map(|text| text.trim_end().to_string())
        .map_err(|e| PrError::State(e.to_string()))
    }

    /// The forge repository a remote clone's `origin` names, resolved by
    /// the live `[forge]` map so self-hosted kinds count.
    fn forge_of(&self, record: &CloneRecord) -> Option<ForgeRepo> {
        if record.local {
            return None;
        }
        let auth = self.context.forge_auth.get();
        ForgeRepo::from_remote_url_with(&record.source, |host| auth.kind_of(host)).ok()
    }

    /// Which backend lands this record: local-bound clones merge into the
    /// checkout through the may backend; remote clones publish through
    /// their forge. `[[projects]] pr.backend = "may"` pins a remote clone to
    /// local landing — rejected, there is no checkout to land into.
    fn backend_for(&self, record: &CloneRecord) -> (&'static str, Option<ForgeRepo>) {
        if record.local {
            return ("may", None);
        }
        self.forge_of(record).map_or(("may", None), |repo| {
            (repo.kind().binding_prefix(), Some(repo))
        })
    }

    /// `[[projects]] pr.backend` pins landing to one backend; the pin and
    /// the clone disagreeing means the PR is refused.
    fn check_pinned_backend(&self, record: &CloneRecord, detected: &str) -> Result<(), PrError> {
        let pinned = self
            .project_for_record(record)
            .and_then(|project| project.pr.backend.as_deref());
        if let Some(pinned) = pinned.filter(|pinned| *pinned != detected) {
            return Err(PrError::Resolve(format!(
                "this project pins pr.backend to \"{pinned}\" but the session clone resolves to the \"{detected}\" backend; re-run `repo trust set` to settle one backend"
            )));
        }
        Ok(())
    }

    /// The `[[projects]]` entry covering this record, by repository
    /// identity for local clones and by forge repository for remote ones.
    fn project_for_record(&self, record: &CloneRecord) -> Option<&crate::workspace::ProjectConfig> {
        self.context.workspace_config.projects.iter().find(|p| {
            if record.local {
                p.has_identity(Path::new(&record.repo_key))
            } else {
                self.forge_of(record)
                    .is_some_and(|repo| p.covers_forge(&repo, &crate::config::MayConfig::default()))
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

    /// Commits in `base..head`, oldest first, with abbreviated hash and full message.
    async fn commit_messages(
        &self,
        resolved: &Resolved,
        base: &str,
        head: &str,
    ) -> Result<Vec<OwnedCommit>, PrError> {
        let script = CommitMessages {
            base: ShellWord::new(base).map_err(PrError::State)?,
            head: ShellWord::new(head).map_err(PrError::State)?,
        }
        .render()
        .expect("the commit messages script renders");
        let log = self.sandbox.run(&resolved.dir, &script).await?;
        Ok(parse_commit_messages(&log))
    }

    /// Run outbound publish review for external text before it leaves May.
    async fn review_outbound(
        &self,
        publication: crate::harness::publish_review::Publication<'_>,
    ) -> Result<(), PrError> {
        let Some(review) = self.context.review.as_ref() else {
            return Err(PrError::Screened(
                "no outbound publish review is wired".to_string(),
            ));
        };
        review
            .review(publication)
            .await
            .map_err(|error| PrError::Screened(error.to_string()))
    }

    /// Push the session branch to the remote and open a real PR or MR.
    /// Returns the upstream number (`iid` for GitLab).
    async fn push_and_create(
        &self,
        resolved: &Resolved,
        branch: &str,
        base_sha: &str,
        head_sha: &str,
        text: &PrText,
        repo: &ForgeRepo,
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
            push_https_auth(&self.context.forge_auth, &resolved.record.source).await,
        )
        .await?;
        match repo.kind() {
            ForgeKind::GitHub => {
                create_github_pr(
                    &self.context.github_api,
                    &repo.full_path(),
                    branch,
                    &text.title,
                    &text.body,
                )
                .await
            }
            ForgeKind::GitLab => {
                gitlab::create_mr(
                    &self.context.gitlab_rest(repo.host().as_str()),
                    repo,
                    branch,
                    &text.title,
                    &text.body,
                )
                .await
            }
        }
    }

    /// Remote landing: the pinned commit is already on the upstream branch
    /// (pushed at open or update); queue the forge's merge, watch until the
    /// upstream state is terminal, then best-effort pull-sync the recorded
    /// checkout and retire the clone.
    async fn merge_remote(
        &self,
        resolved: &Resolved,
        pr: &crate::db::PrRow,
        head_sha: &str,
    ) -> Result<String, PrError> {
        /// How long before an unexplained removal is tried again; multiplied
        /// by the attempt so the wait grows.
        const REQUEUE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);
        let repo = self
            .forge_of(&resolved.record)
            .ok_or_else(|| PrError::Resolve("remote PR without a repo".to_string()))?;
        let remote = match repo.kind() {
            ForgeKind::GitHub => Remote::GitHub(repo.clone()),
            ForgeKind::GitLab => Remote::GitLab(repo.clone()),
        };
        let upstream_number = pr.number;
        let lock = self.context.land_locks.get(&resolved.record.repo_key);
        let owner_repo = repo.full_path();
        let gitlab_rest = (repo.kind() == ForgeKind::GitLab)
            .then(|| self.context.gitlab_rest(repo.host().as_str()));
        let upstream: AnyUpstream<'_> = match &remote {
            Remote::GitHub(_) => AnyUpstream::GitHub(merge_watch::GithubPr {
                api: &self.context.github_api,
                owner_repo: &owner_repo,
                number: upstream_number,
            }),
            Remote::GitLab(repo) => AnyUpstream::GitLab(gitlab::GitLabMr {
                rest: gitlab_rest.as_ref().expect("gitlab backend has a client"),
                repo,
                iid: upstream_number,
            }),
        };
        let expected = merge_watch::Upstream::expected_checks(&upstream).await?;
        // A merge queue can drop an entry with no reason and nothing
        // failing: such a removal is transient, so the queue-and-watch
        // cycle re-queues a bounded number of times before the agent is
        // told. A removal with a cause is reported instead.
        let max_requeues = 3u32;
        let mut requeues = 0u32;
        let outcome = loop {
            // Read before queueing: the removals that predate it.
            let before = merge_watch::Upstream::snapshot(&upstream).await?;
            ensure_head_pinned(upstream_number, head_sha, &before.head_sha, pr.sigil())?;
            self.event(pr, "queued", None).await;
            // Serialized land: the lock covers queueing the PR, not the wait
            // for it to land.
            {
                let _guard = lock.lock().await;
                match &remote {
                    Remote::GitHub(_) => {
                        queue_github_merge(
                            &self.context.github_api,
                            &owner_repo,
                            upstream_number,
                            head_sha,
                        )
                        .await?;
                    }
                    Remote::GitLab(repo) => {
                        let pipeline_running = before.checks.iter().any(|check| {
                            matches!(
                                check,
                                merge_watch::CheckContext::CheckRun {
                                    conclusion: None,
                                    ..
                                }
                            )
                        });
                        gitlab::queue_merge(
                            gitlab_rest.as_ref().expect("gitlab backend has a client"),
                            repo,
                            upstream_number,
                            head_sha,
                            pipeline_running,
                        )
                        .await?;
                    }
                }
            }
            // The checks are timed the moment they finish, not when the PR lands.
            let observer = CiObserver {
                store: self,
                pr,
                remote: &remote,
                head_sha,
            };
            let watching = merge_watch::Observing::new(&upstream, &observer);
            let outcome = merge_watch::watch(
                &watching,
                &merge_watch::SystemClock::new(),
                before.removals,
                expected,
            )
            .await?;
            watching.tell_if_unheard().await;
            if let merge_watch::Outcome::Stopped(merge_watch::Stopped::Removed(removal)) = &outcome
                && removal.unexplained
                && requeues < max_requeues
            {
                requeues += 1;
                // A removal nothing explains tends to be a queue flap:
                // wait for it to settle, longer each time, before the PR
                // goes back on the queue. A removal with a cause — a
                // failed check, a conflict — is terminal and is reported.
                async_io::Timer::after(REQUEUE_BACKOFF * requeues).await;
                self.event(pr, "requeued", Some(&removal.reason)).await;
                continue;
            }
            break outcome;
        };
        let now = jiff::Timestamp::now().to_string();
        match outcome {
            merge_watch::Outcome::Merged => {
                self.db()?
                    .update_pr(
                        pr.id,
                        PrState::Merged,
                        Some(head_sha),
                        None,
                        Some(now.as_str()),
                    )
                    .await?;
                self.event(pr, "merged", Some(head_sha)).await;
                close_linked_may_issue(
                    self.db()?,
                    &self.context.workspace_config,
                    &resolved.record.repo_key,
                    pr,
                    self.project_for_record(&resolved.record),
                )
                .await;
                self.record_queue_wait(pr).await;
                self.task_event(
                    pr,
                    crate::harness::working_document::WorkEvent::PullRequestMerged,
                );
                let pull_sync = self
                    .pull_sync(&resolved.record, &remote.repo().cache_key())
                    .await;
                if let Some(checkouts) = &self.context.checkouts
                    && let Err(error) = checkouts.refresh_existing(&resolved.record.repo_key).await
                {
                    tracing::warn!(
                        %error,
                        repo = %resolved.record.repo_key,
                        "failed to refresh daemon checkout after remote PR merged"
                    );
                }
                let clone = self.retire_clone(resolved, head_sha).await?;
                let remote_branch = self.delete_remote_branch(&remote, &pr.branch).await;
                Ok(PrMerged {
                    sigil: pr.sigil(),
                    number: pr.number,
                    merge_sha: None,
                    clone_dir: resolved.record.dir.as_str(),
                    clone,
                    remote_branch,
                    pull_sync,
                }
                .render()
                .expect("the merge result renders"))
            }
            merge_watch::Outcome::Stopped(stopped) => {
                let report = merge_watch::StoppedReport::new(pr.sigil(), pr.number, &stopped)
                    .render()
                    .expect("the watch report renders");
                let report = report.trim_end().to_string();
                match stopped {
                    merge_watch::Stopped::Closed => {
                        self.db()?
                            .update_pr(pr.id, PrState::Closed, Some(head_sha), None, None)
                            .await?;
                        self.event(pr, "closed", Some(&report)).await;
                        self.task_event(
                            pr,
                            crate::harness::working_document::WorkEvent::PullRequestClosed,
                        );
                        let _ = self.retire_closed_clone(resolved, Some(head_sha)).await;
                        Err(PrError::State(report))
                    }
                    merge_watch::Stopped::CheckFailed(_) => {
                        self.db()?
                            .update_pr(pr.id, PrState::Failed, Some(head_sha), None, None)
                            .await?;
                        self.event(pr, "check_failed", Some(&report)).await;
                        Err(PrError::CheckFailed(report))
                    }
                    merge_watch::Stopped::Removed(_) => {
                        self.db()?
                            .update_pr(pr.id, PrState::Failed, Some(head_sha), None, None)
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
        close_github_api(&self.context.github_api, owner_repo, external).await
    }

    /// Delete a merged PR's `may/*` branch on the remote. A branch already
    /// gone (the repository deletes head branches on merge) counts as
    /// deleted; any other failure is reported, since the merge has landed.
    async fn delete_remote_branch(&self, remote: &Remote, branch: &str) -> RemoteBranch {
        match remote {
            Remote::GitHub(repo) => {
                delete_remote_branch_api(&self.context.github_api, &repo.full_path(), branch).await
            }
            Remote::GitLab(repo) => {
                gitlab::delete_branch(
                    &self.context.gitlab_rest(repo.host().as_str()),
                    repo,
                    branch,
                )
                .await
            }
        }
    }

    /// Best-effort `git pull --ff-only` on every local checkout recorded
    /// for the merged repository — sync targets, never blockers.
    /// `[[projects]] pr.pull_sync = false` turns it off. What happened to
    /// each checkout is returned for the merge report.
    async fn pull_sync(&self, record: &CloneRecord, owner_repo: &str) -> Vec<SyncedCheckout> {
        let disabled = self
            .project_for_record(record)
            .is_some_and(|project| !project.pr.pulls_after_merge());
        let Some(db) = self.context.db.as_ref().filter(|_| !disabled) else {
            return Vec::new();
        };
        let checkouts = pull_sync::recorded_checkouts(db, owner_repo).await;
        pull_sync::sync(
            self.context.signing.host(),
            checkouts,
            pull_sync::PULL_TIMEOUT,
        )
        .await
    }

    fn check_command(&self, record: &CloneRecord) -> Option<String> {
        self.context
            .workspace_config
            .project_for_identity(Path::new(&record.repo_key))
            .and_then(|p| p.checks.clone())
    }

    async fn base_sha(&self, resolved: &Resolved) -> Result<String, PrError> {
        if let Some(base) = &resolved.record.base_sha {
            // The clone was based on its checkout's work in progress: the
            // session's work starts after that commit.
            Ok(base.clone())
        } else if resolved.record.local {
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

    /// After closing a PR, delete its workspace when the directory is missing
    /// or holds no uncommitted work beyond the PR commit.
    async fn retire_closed_clone(
        &self,
        resolved: &Resolved,
        pinned: Option<&str>,
    ) -> Result<CloneFate, PrError> {
        if !resolved.dir.exists() {
            self.repos()?.delete_clone(&resolved.record.dir).await?;
            return Ok(CloneFate::Deleted);
        }
        let clean = uncommitted_changes(&self.sandbox, &resolved.dir)
            .await?
            .is_empty();
        let same = match pinned {
            Some(sha) if sha != "base" => self.head_sha(resolved).await? == sha,
            _ => true,
        };
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
        change: fn(
            crate::harness::working_document::WorkRef,
        ) -> crate::harness::working_document::WorkEvent,
    ) {
        if let Some(events) = &self.context.task_events
            && let Ok(number) = u64::try_from(pr.number)
        {
            events.record(change(
                crate::harness::working_document::WorkRef::pull_request(number),
            ));
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

    fn notify_prs(&self, session_id: &str) {
        if let Some(bus) = self
            .context
            .ui_events
            .as_ref()
            .or_else(|| self.context.ci.as_ref().map(|watch| &watch.ui_events))
        {
            notify_prs_bus(bus, session_id);
        }
    }

    async fn event(&self, pr: &crate::db::PrRow, kind: &str, detail: Option<&str>) {
        if let Some(db) = self.context.db.as_ref() {
            let message_id = db
                .max_message_id(&self.context.session_id)
                .await
                .ok()
                .flatten();
            let _ = db
                .add_pr_event(
                    pr.id,
                    kind,
                    &self.context.session_id,
                    "main",
                    message_id,
                    detail,
                )
                .await;
        }
        self.notify_prs(&pr.session_id);
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

    /// Records the remote checks of `head_sha` and warns the user about an
    /// abnormally long job. A recording failure is reported, never fatal: the
    /// PR lands or fails on its checks, not on their timing.
    async fn observe_remote_checks(&self, pr: &crate::db::PrRow, remote: &Remote, head_sha: &str) {
        let (Some((watch, limits)), Ok(db)) = (self.ci_limits().await, self.db()) else {
            return;
        };
        let outcome = match remote {
            Remote::GitHub(repo) => {
                let actions = crate::ci::github::GhActions::new(
                    self.context.github_api.clone(),
                    repo.full_path(),
                    pr.number,
                );
                let recorder = crate::ci::record::Recorder {
                    db,
                    actions: &actions,
                    limits,
                };
                recorder.observe_remote(pr, head_sha).await
            }
            Remote::GitLab(repo) => {
                let actions = crate::ci::gitlab::GitLabCi::new(
                    self.context.gitlab_rest(repo.host().as_str()),
                    repo.clone(),
                    pr.number,
                );
                let recorder = crate::ci::record::Recorder {
                    db,
                    actions: &actions,
                    limits,
                };
                recorder.observe_remote(pr, head_sha).await
            }
        };
        match outcome {
            Ok(Some(_)) => {
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

/// Announces to live Web UI and TUI subscribers that PR lists may have changed.
pub(crate) fn notify_prs_bus(bus: &crate::ui_event_bus::UiEventBus, session_id: &str) {
    bus.publish(crate::ui_event_bus::UiEventTopics::PRS);
    if !session_id.is_empty() {
        bus.publish_session_change(
            session_id,
            crate::daemon::session_events::SessionChange::Prs,
        );
    }
}

/// Whether the repository configuration requires explicit user review before
/// merging PRs from this backend/repository.
#[must_use]
pub(crate) fn pr_requires_review(
    workspace: &crate::workspace::WorkspaceConfig,
    backend: &str,
    repo_key: &str,
) -> bool {
    workspace
        .projects
        .iter()
        .find(|project| {
            let kind = match backend {
                "gitlab" => Some(ForgeKind::GitLab),
                "github" => Some(ForgeKind::GitHub),
                _ => None,
            };
            kind.and_then(|kind| ForgeRepo::from_cache_key(kind, repo_key).ok())
                .map_or_else(
                    || project.has_identity(Path::new(repo_key)),
                    |repo| project.covers_forge(&repo, &crate::config::MayConfig::default()),
                )
        })
        .is_some_and(|project| project.pr.require_review == Some(true))
}

/// Merge an orphaned PR (or one awaiting user review) directly on the host,
/// attributing every recorded lifecycle event to `"user"`.
///
/// # Errors
/// Fails when the DB is unavailable, the PR does not exist, belongs to an
/// active session that does not require review, is already terminal, or the
/// land/GitHub merge fails.
pub async fn user_merge_pr(context: &PrContext, pr_id: i64) -> Result<crate::db::PrRow, PrError> {
    let db = context
        .db
        .as_ref()
        .ok_or_else(|| PrError::Resolve("pr requires the history database".to_string()))?;
    let pr = db
        .get_pr_by_id(pr_id)
        .await?
        .ok_or_else(|| PrError::State(format!("no PR with id {pr_id}")))?;
    let repo_key = db
        .repo_key_of_project(pr.project_id)
        .await?
        .ok_or_else(|| PrError::State(format!("PR #{} repository is missing", pr.number)))?;
    let orphaned = !db.session_exists(&pr.session_id).await?;
    let require_review = pr_requires_review(&context.workspace_config, &pr.backend, &repo_key);
    if !orphaned && !require_review {
        return Err(PrError::Ownership(format!(
            "PR #{} belongs to active session {} — wait for it or delete the session first",
            pr.number, pr.session_id
        )));
    }
    if pr.state.is_terminal() {
        return Err(PrError::State(format!(
            "PR #{} is already {}",
            pr.number, pr.state
        )));
    }
    let head_sha = pr.head_sha.clone().ok_or_else(|| {
        PrError::State(format!("PR #{} has no pinned commit to merge", pr.number))
    })?;
    let pr = db
        .update_pr(pr.id, PrState::ChecksPending, Some(&head_sha), None, None)
        .await?;
    record_user_event(db, context, &pr, "checks_pending", None).await;
    if matches!(pr.backend.as_str(), "github" | "gitlab") {
        return user_merge_remote(db, context, &pr, &repo_key, &head_sha).await;
    }
    let checkout = resolve_host_checkout(db, &context.workspace_config, &repo_key).await?;
    let lock = context.land_locks.get(&repo_key);
    record_user_event(db, context, &pr, "merging", None).await;
    let result = blocking::unblock({
        let refname = pr.branch.clone();
        let base = pr.base_sha.clone();
        let sha = head_sha.clone();
        let number = pr.number;
        let host = context.signing.host().clone();
        let checkout = checkout.to_string_lossy().into_owned();
        move || land(&host, &checkout, &refname, &base, &sha, number, &lock)
    })
    .await;
    let now = jiff::Timestamp::now().to_string();
    match result {
        Ok(merge_sha) => {
            let updated = db
                .update_pr(
                    pr.id,
                    PrState::Merged,
                    Some(&head_sha),
                    Some(&merge_sha),
                    Some(&now),
                )
                .await?;
            record_user_event(db, context, &updated, "merged", Some(&merge_sha)).await;
            close_linked_may_issue(db, &context.workspace_config, &repo_key, &updated, None).await;
            Ok(updated)
        }
        Err(reason) => {
            let updated = db
                .update_pr(pr.id, PrState::Rejected, Some(&head_sha), None, None)
                .await?;
            record_user_event(db, context, &updated, "rejected", Some(&reason)).await;
            Err(PrError::Rejected(reason))
        }
    }
}

/// Close an orphaned PR (or one awaiting user review) directly on the host,
/// attributing the close event to `"user"`.
///
/// # Errors
/// Fails when the DB is unavailable, the PR does not exist, belongs to an
/// active session that does not require review, is already terminal, or
/// GitHub rejects the close.
pub async fn user_close_pr(context: &PrContext, pr_id: i64) -> Result<crate::db::PrRow, PrError> {
    let db = context
        .db
        .as_ref()
        .ok_or_else(|| PrError::Resolve("pr requires the history database".to_string()))?;
    let pr = db
        .get_pr_by_id(pr_id)
        .await?
        .ok_or_else(|| PrError::State(format!("no PR with id {pr_id}")))?;
    let repo_key = db
        .repo_key_of_project(pr.project_id)
        .await?
        .ok_or_else(|| PrError::State(format!("PR #{} repository is missing", pr.number)))?;
    let orphaned = !db.session_exists(&pr.session_id).await?;
    let require_review = pr_requires_review(&context.workspace_config, &pr.backend, &repo_key);
    if !orphaned && !require_review {
        return Err(PrError::Ownership(format!(
            "PR #{} belongs to active session {} — close it from that session or delete the session first",
            pr.number, pr.session_id
        )));
    }
    if pr.state.is_terminal() {
        return Err(PrError::State(format!(
            "PR #{} is already {}",
            pr.number, pr.state
        )));
    }
    match pr.backend.as_str() {
        "github" => {
            let owner_repo = github_owner_repo(&repo_key)
                .ok_or_else(|| PrError::Resolve("github PR without a repo".to_string()))?;
            close_github_api(&context.github_api, &owner_repo, pr.number).await?;
        }
        "gitlab" => {
            let repo = ForgeRepo::from_cache_key(ForgeKind::GitLab, &repo_key)
                .map_err(|error| PrError::Resolve(error.to_string()))?;
            let rest = context.gitlab_rest(repo.host().as_str());
            gitlab::close_mr(&rest, &repo, pr.number).await?;
        }
        _ => {}
    }
    let updated = db.close_pr(pr.id).await?;
    record_user_event(db, context, &updated, "closed", None).await;
    if pr.backend == "may"
        && let Ok(checkout) = resolve_host_checkout(db, &context.workspace_config, &repo_key).await
    {
        drop_ref(
            context.signing.host(),
            &checkout.to_string_lossy(),
            &pr.branch,
        )
        .await;
    }
    Ok(updated)
}

struct NoopChecksObserver;

impl merge_watch::ChecksObserver for NoopChecksObserver {
    async fn finished(&self) {}
}

async fn user_merge_remote(
    db: &ChatDb,
    context: &PrContext,
    pr: &crate::db::PrRow,
    repo_key: &str,
    head_sha: &str,
) -> Result<crate::db::PrRow, PrError> {
    let kind = match pr.backend.as_str() {
        "gitlab" => ForgeKind::GitLab,
        _ => ForgeKind::GitHub,
    };
    let repo = ForgeRepo::from_cache_key(kind, repo_key)
        .map_err(|error| PrError::Resolve(error.to_string()))?;
    let owner_repo = repo.full_path();
    let upstream_number = pr.number;
    let lock = context.land_locks.get(repo_key);
    let gitlab_rest =
        (kind == ForgeKind::GitLab).then(|| context.gitlab_rest(repo.host().as_str()));
    let upstream: AnyUpstream<'_> = match kind {
        ForgeKind::GitHub => AnyUpstream::GitHub(merge_watch::GithubPr {
            api: &context.github_api,
            owner_repo: &owner_repo,
            number: upstream_number,
        }),
        ForgeKind::GitLab => AnyUpstream::GitLab(gitlab::GitLabMr {
            rest: gitlab_rest.as_ref().expect("gitlab backend has a client"),
            repo: &repo,
            iid: upstream_number,
        }),
    };
    let before = merge_watch::Upstream::snapshot(&upstream).await?;
    ensure_head_pinned(upstream_number, head_sha, &before.head_sha, pr.sigil())?;
    let expected = merge_watch::Upstream::expected_checks(&upstream).await?;
    record_user_event(db, context, pr, "queued", None).await;
    {
        let _guard = lock.lock().await;
        match kind {
            ForgeKind::GitHub => {
                queue_github_merge(&context.github_api, &owner_repo, upstream_number, head_sha)
                    .await?;
            }
            ForgeKind::GitLab => {
                let pipeline_running = before.checks.iter().any(|check| {
                    matches!(
                        check,
                        merge_watch::CheckContext::CheckRun {
                            conclusion: None,
                            ..
                        }
                    )
                });
                gitlab::queue_merge(
                    gitlab_rest.as_ref().expect("gitlab backend has a client"),
                    &repo,
                    upstream_number,
                    head_sha,
                    pipeline_running,
                )
                .await?;
            }
        }
    }
    let observer = NoopChecksObserver;
    let watching = merge_watch::Observing::new(&upstream, &observer);
    let outcome = merge_watch::watch(
        &watching,
        &merge_watch::SystemClock::new(),
        before.removals,
        expected,
    )
    .await?;
    let now = jiff::Timestamp::now().to_string();
    match outcome {
        merge_watch::Outcome::Merged => {
            let updated = db
                .update_pr(
                    pr.id,
                    PrState::Merged,
                    Some(head_sha),
                    None,
                    Some(now.as_str()),
                )
                .await?;
            record_user_event(db, context, &updated, "merged", Some(head_sha)).await;
            close_linked_may_issue(db, &context.workspace_config, repo_key, &updated, None).await;
            let disabled = context
                .workspace_config
                .projects
                .iter()
                .find(|project| project.covers_forge(&repo, &crate::config::MayConfig::default()))
                .is_some_and(|project| !project.pr.pulls_after_merge());
            if !disabled {
                let checkouts = pull_sync::recorded_checkouts(db, &repo.cache_key()).await;
                pull_sync::sync(context.signing.host(), checkouts, pull_sync::PULL_TIMEOUT).await;
            }
            if let Some(checkouts) = &context.checkouts
                && let Err(error) = checkouts.refresh_existing(repo_key).await
            {
                tracing::warn!(
                    %error,
                    repo = %repo_key,
                    "failed to refresh daemon checkout after user merged remote PR"
                );
            }
            let remote = match kind {
                ForgeKind::GitHub => Remote::GitHub(repo.clone()),
                ForgeKind::GitLab => Remote::GitLab(repo.clone()),
            };
            match &remote {
                Remote::GitHub(repo) => {
                    let _ = delete_remote_branch_api(
                        &context.github_api,
                        &repo.full_path(),
                        &pr.branch,
                    )
                    .await;
                }
                Remote::GitLab(repo) => {
                    let _ = gitlab::delete_branch(
                        gitlab_rest.as_ref().expect("gitlab backend has a client"),
                        repo,
                        &pr.branch,
                    )
                    .await;
                }
            }
            Ok(updated)
        }
        merge_watch::Outcome::Stopped(stopped) => {
            let report = merge_watch::StoppedReport::new(pr.sigil(), pr.number, &stopped)
                .render()
                .expect("the watch report renders");
            let report = report.trim_end().to_string();
            match stopped {
                merge_watch::Stopped::Closed => {
                    db.update_pr(pr.id, PrState::Closed, Some(head_sha), None, None)
                        .await?;
                    record_user_event(db, context, pr, "closed", Some(&report)).await;
                    Err(PrError::State(report))
                }
                merge_watch::Stopped::CheckFailed(_) => {
                    db.update_pr(pr.id, PrState::Failed, Some(head_sha), None, None)
                        .await?;
                    record_user_event(db, context, pr, "check_failed", Some(&report)).await;
                    Err(PrError::CheckFailed(report))
                }
                merge_watch::Stopped::Removed(_) => {
                    db.update_pr(pr.id, PrState::Failed, Some(head_sha), None, None)
                        .await?;
                    record_user_event(db, context, pr, "removed_from_queue", Some(&report)).await;
                    Err(PrError::State(report))
                }
                merge_watch::Stopped::TimedOut { .. } => {
                    record_user_event(db, context, pr, "watch_timeout", Some(&report)).await;
                    Err(PrError::State(report))
                }
            }
        }
    }
}

async fn close_linked_may_issue(
    db: &ChatDb,
    workspace_config: &crate::workspace::WorkspaceConfig,
    repo_key: &str,
    pr: &crate::db::PrRow,
    project: Option<&crate::workspace::ProjectConfig>,
) {
    let Some(issue_number) = pr.issue_number else {
        return;
    };
    let uses_github_issues = project
        .or_else(|| {
            workspace_config.projects.iter().find(|p| {
                p.has_identity(Path::new(repo_key))
                    || github_owner_repo(repo_key)
                        == p.github_repository()
                            .map(|r| format!("{}/{}", r.owner(), r.repo()))
            })
        })
        .is_some_and(|p| p.github_repository().is_some());
    if uses_github_issues {
        return;
    }
    let Ok(Some(_)) = db.get_issue(repo_key, issue_number).await else {
        return;
    };
    let relation_note = format!("landed_by PR #{}", pr.number);
    if let Err(error) = db
        .add_issue_comment(repo_key, issue_number, &relation_note)
        .await
    {
        tracing::warn!(
            %error,
            pr = pr.number,
            issue = issue_number,
            "failed to record landed_by comment on May issue"
        );
    }
    if let Err(error) = db
        .update_issue(
            repo_key,
            issue_number,
            None,
            None,
            Some(crate::issue::IssueState::Closed),
        )
        .await
    {
        tracing::warn!(
            %error,
            pr = pr.number,
            issue = issue_number,
            "failed to close linked May issue on PR merge"
        );
    } else {
        let _ = db
            .add_pr_event(
                pr.id,
                "issue_closed",
                &pr.session_id,
                "pr",
                None,
                Some(&relation_note),
            )
            .await;
    }
}

async fn record_user_event(
    db: &ChatDb,
    context: &PrContext,
    pr: &crate::db::PrRow,
    kind: &str,
    detail: Option<&str>,
) {
    let _ = db
        .add_pr_event(pr.id, kind, &pr.session_id, "user", None, detail)
        .await;
    if let Some(bus) = context
        .ui_events
        .as_ref()
        .or_else(|| context.ci.as_ref().map(|watch| &watch.ui_events))
    {
        notify_prs_bus(bus, &pr.session_id);
    }
}

async fn resolve_host_checkout(
    db: &ChatDb,
    workspace_config: &crate::workspace::WorkspaceConfig,
    repo_key: &str,
) -> Result<PathBuf, PrError> {
    let target = Path::new(repo_key);
    if target.file_name().is_some_and(|name| name == ".git")
        && let Some(parent) = target.parent()
        && parent.join(".git").exists()
    {
        return Ok(parent.to_path_buf());
    }
    if target.join(".git").exists() {
        return Ok(target.to_path_buf());
    }
    for project in &workspace_config.projects {
        if project.has_identity(target)
            && let Some(path) = &project.path
        {
            let candidate = Path::new(path);
            if candidate.exists() {
                return Ok(candidate.to_path_buf());
            }
        }
    }
    if let Ok(usage) = db.repo_usage_since(0).await {
        for entry in usage {
            let candidate = entry.path();
            if candidate.exists()
                && crate::issue::repository_identity(&candidate)
                    .is_ok_and(|identity| identity == target)
            {
                return Ok(candidate);
            }
        }
    }
    Err(PrError::Resolve(format!(
        "no local checkout found on disk for repository {repo_key}"
    )))
}

async fn close_github_api(
    api: &GitHubRest<PrTokenSource>,
    owner_repo: &str,
    external: i64,
) -> Result<(), PrError> {
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

async fn delete_remote_branch_api(
    api: &GitHubRest<PrTokenSource>,
    owner_repo: &str,
    branch: &str,
) -> RemoteBranch {
    if !branch.starts_with("may/") {
        return RemoteBranch::NotDeleted(
            branch.to_string(),
            "only may/* branches are the session's to delete".to_string(),
        );
    }
    let url = format!(
        "{}/repos/{owner_repo}/git/refs/heads/{branch}",
        api.api_base()
    );
    match api
        .request_unchecked(zenwave::Method::DELETE, url, None)
        .await
    {
        Ok(response) if response.status().is_success() => RemoteBranch::Deleted(branch.to_string()),
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

#[derive(Debug, Clone)]
struct PrOpenProvenance {
    session_id: String,
    agent_id: String,
    message_id: Option<i64>,
    created_at: String,
}

/// Deserializes a PR or MR number from a JSON integer or a string with an
/// optional leading `#` or `!` sigil.
///
/// # Errors
/// Fails when the value is neither an integer nor a positive `#N`/`!N`/`N` string.
pub fn deserialize_pr_number<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct PrNumberVisitor;

    impl serde::de::Visitor<'_> for PrNumberVisitor {
        type Value = i64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a positive PR or MR number (optionally prefixed with `#` or `!`)")
        }

        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
            if value > 0 {
                Ok(value)
            } else {
                Err(E::custom(format!(
                    "PR number must be positive, got {value}"
                )))
            }
        }

        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
            let signed = i64::try_from(value)
                .map_err(|_| E::custom(format!("PR number {value} overflows i64")))?;
            self.visit_i64(signed)
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
            let trimmed = value.trim();
            let digits = trimmed
                .strip_prefix('#')
                .or_else(|| trimmed.strip_prefix('!'))
                .unwrap_or(trimmed);
            let parsed = digits
                .parse::<i64>()
                .map_err(|_| E::custom(format!("invalid PR number: {value:?}")))?;
            self.visit_i64(parsed)
        }
    }

    deserializer.deserialize_any(PrNumberVisitor)
}

#[derive(Debug, Clone)]
struct PrListItemView {
    sigil: char,
    number: i64,
    state: PrState,
    title: String,
    merge_commit_sha: Option<String>,
    orphaned: bool,
    repo_key: String,
    backend: String,
    session_id: String,
    opened: Option<PrOpenProvenance>,
    checks: Option<String>,
    detail: Option<String>,
}

#[derive(Template)]
#[template(path = "pr_list.txt", escape = "none")]
struct PrListTemplate<'a> {
    prs: &'a [PrListItemView],
}

struct PrEventView {
    created_at: String,
    kind: String,
    agent_id: String,
    session_id: String,
    message_id: Option<i64>,
    detail: Option<String>,
}

#[derive(Template)]
#[template(path = "pr_show.txt", escape = "none")]
struct PrShowTemplate<'a> {
    sigil: char,
    number: i64,
    state: PrState,
    title: &'a str,
    repo_key: &'a str,
    session_id: &'a str,
    orphaned: bool,
    backend: &'a str,
    base_sha: &'a str,
    head_sha: &'a str,
    merge_commit_sha: Option<&'a str>,
    opened: Option<&'a PrOpenProvenance>,
    checks: Option<&'a str>,
    detail: Option<&'a str>,
    events: &'a [PrEventView],
}

fn pr_opened_provenance(events: &[crate::db::PrEventRow]) -> Option<PrOpenProvenance> {
    events
        .iter()
        .find(|event| event.kind == "opened")
        .or_else(|| events.first())
        .map(|event| PrOpenProvenance {
            session_id: event.session_id.clone(),
            agent_id: event.agent_id.clone(),
            message_id: event.message_id,
            created_at: event.created_at.clone(),
        })
}

fn pr_failure_detail(
    events: &[crate::db::PrEventRow],
    state: PrState,
    check_state: Option<&crate::db::PrCheckState>,
) -> Option<String> {
    if matches!(state, PrState::Failed | PrState::Rejected)
        && let Some(detail) = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    "rejected" | "check_failed" | "removed_from_queue"
                ) && event.detail.as_ref().is_some_and(|d| !d.trim().is_empty())
            })
            .and_then(|event| event.detail.clone())
    {
        return Some(detail);
    }
    if state == PrState::Closed
        && let Some(detail) = events
            .iter()
            .rev()
            .find(|event| {
                event.kind == "closed"
                    && event.detail.as_ref().is_some_and(|d| !d.trim().is_empty())
            })
            .and_then(|event| event.detail.clone())
    {
        return Some(detail);
    }
    if check_state.is_some_and(|c| c.unreadable) {
        return Some("latest check poll could not read check status".to_string());
    }
    None
}

fn format_cached_checks(check_state: Option<&crate::db::PrCheckState>) -> Option<String> {
    let check_state = check_state?;
    if check_state.unreadable {
        return Some("unreadable".to_string());
    }
    let status = match check_state.rollup {
        checks::Rollup::Pending => "pending",
        checks::Rollup::Passed => "passed",
        checks::Rollup::Failed => "failed",
    };
    Some(format!("{status} ({})", check_state.head_sha))
}

/// The remote branch of a remote PR, named from its session and the commit it
/// is first opened at, so two PRs of one session never share a branch and a
/// retried open of the same work reuses its branch. The name is stored in the
/// PR row; `update`, close and adoption read it from there.
fn remote_branch(session_id: &str, head_sha: &str) -> String {
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
#[allow(clippy::too_many_arguments)]
async fn push_pinned(
    scratch_root: &Path,
    clone: &Path,
    url: &str,
    sha: &str,
    branch: &str,
    branch_state: BranchState,
    signed: SignedRange<'_>,
    https_auth: Option<crate::host_git::HttpsAuth>,
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
        let host = signing.host().clone();
        // The credential rides in git's environment, scoped to its own
        // origin: it reaches the fetch and the push of an `https://` remote
        // and nothing else.
        let host = match https_auth {
            Some(auth) => host.with_https_auth(auth),
            None => host,
        };
        host_git(
            &host,
            None,
            &["init", "--quiet", "--bare", &repo.to_string_lossy()],
        )?;
        fetch_from_clone(&host, repo, &clone, &sha)?;
        fetch_upstream(&host, repo, &url, branch_state, &branch)?;
        // The signature check covers the PR's own commits. After a rebase
        // onto an upstream that moved since the recorded base was pinned,
        // `base..sha` would span unsigned upstream commits already merged:
        // verify from the fork point with the upstream tip instead.
        let base = host_git(
            &host,
            Some(repo),
            &["merge-base", "refs/remotes/origin/default", &sha],
        )
        .map_or(base, |output| output.trim().to_string());
        // What is pushed is what was verified: both read the host's copy.
        signing.verify_range_blocking(repo, &base, &sha)?;
        host_git(&host, Some(repo), &["push", "--quiet", &url, &spec])
            .map(drop)
            .map_err(PrError::from)
    })
    .await
}

/// The HTTPS credential `push_pinned` carries for `url`: when the remote is
/// an `https://` URL on a forge host `forge_auth` authenticates, that host's
/// token scoped to its origin. SSH, `file://` and unauthenticated remotes
/// keep git exactly as it was, so local and SSH pushes need no login.
async fn push_https_auth(
    forge_auth: &crate::forge::LiveForgeAuth,
    url: &str,
) -> Option<crate::host_git::HttpsAuth> {
    let authority = url
        .trim()
        .strip_prefix("https://")?
        .split('/')
        .next()
        .filter(|host| !host.is_empty() && !host.contains('@'))?;
    forge_auth.git_https_auth(authority).await.ok()
}

/// Open the pull request of `branch` on `owner_repo` over the REST API:
/// the repository's `default_branch` is the base. Returns its number.
async fn create_github_pr<T: GitHubTokenSource + Sync>(
    api: &GitHubRest<T>,
    owner_repo: &str,
    branch: &str,
    title: &str,
    body: &str,
) -> Result<i64, PrError> {
    let repository: RepositoryRef = api
        .request(
            zenwave::Method::GET,
            format!("{}/repos/{owner_repo}", api.api_base()),
            None,
        )
        .await
        .map_err(|e| PrError::Gh(e.to_string()))?
        .into_json()
        .await
        .map_err(|e| PrError::Gh(e.to_string()))?;
    let base = repository
        .default_branch
        .ok_or_else(|| PrError::Gh(format!("{owner_repo} names no default branch")))?;
    let created: CreatedPull = api
        .request(
            zenwave::Method::POST,
            format!("{}/repos/{owner_repo}/pulls", api.api_base()),
            Some(serde_json::json!({
                "title": title,
                "body": body,
                "head": branch,
                "base": base,
            })),
        )
        .await
        .map_err(|e| PrError::Gh(e.to_string()))?
        .into_json()
        .await
        .map_err(|e| PrError::Gh(e.to_string()))?;
    Ok(created.number)
}

/// `GET /repos/{owner}/{repo}`, to the field May reads.
#[derive(serde::Deserialize)]
struct RepositoryRef {
    default_branch: Option<String>,
}

/// `POST /repos/{owner}/{repo}/pulls`, to the field May reads.
#[derive(serde::Deserialize)]
struct CreatedPull {
    number: i64,
}

/// Queue the PR for GitHub to merge: `enablePullRequestAutoMerge` with the
/// pinned head, what `gh pr merge --auto --squash --match-head-commit`
/// ran. When GitHub refuses auto-merge — the repository does not allow it,
/// or the PR is already clean and mergeable — the same pinned squash merge
/// runs at once through `PUT /pulls/{number}/merge`.
async fn queue_github_merge<T: GitHubTokenSource + Sync>(
    api: &GitHubRest<T>,
    owner_repo: &str,
    number: i64,
    head_sha: &str,
) -> Result<(), PrError> {
    match enable_auto_merge(api, owner_repo, number, head_sha).await {
        Ok(()) => Ok(()),
        // A GraphQL refusal is GitHub understanding and declining the
        // auto-merge; the pinned merge runs directly instead.
        Err(AutoMergeError::Refused(message)) => {
            tracing::debug!(%message, "auto-merge refused; merging directly");
            merge_now(api, owner_repo, number, head_sha).await
        }
        Err(AutoMergeError::Call(error)) => Err(error),
    }
}

/// Why the auto-merge mutation did not queue the PR.
enum AutoMergeError {
    /// GitHub answered with GraphQL `errors`: auto-merge is not enabled on
    /// the repository or the PR is already mergeable.
    Refused(String),
    /// The call itself failed.
    Call(PrError),
}

/// `enablePullRequestAutoMerge` on `number`, expecting head `head_sha`.
async fn enable_auto_merge<T: GitHubTokenSource + Sync>(
    api: &GitHubRest<T>,
    owner_repo: &str,
    number: i64,
    head_sha: &str,
) -> Result<(), AutoMergeError> {
    let (owner, name) = owner_repo.split_once('/').ok_or_else(|| {
        AutoMergeError::Call(PrError::Resolve(format!("not an owner/repo: {owner_repo}")))
    })?;
    let graphql = |body: serde_json::Value| async move {
        api.request(
            zenwave::Method::POST,
            format!("{}/graphql", api.api_base()),
            Some(body),
        )
        .await
        .map_err(|e| AutoMergeError::Call(PrError::Gh(e.to_string())))?
        .into_json::<GraphReply>()
        .await
        .map_err(|e| AutoMergeError::Call(PrError::Gh(e.to_string())))
    };
    let reply = graphql(serde_json::json!({
        "query": "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id}}}",
        "variables": { "owner": owner, "name": name, "number": number },
    }))
    .await?;
    if let Some(message) = reply.error_message() {
        return Err(AutoMergeError::Refused(message));
    }
    let pull_request_id = reply
        .pull_request_id()
        .ok_or_else(|| AutoMergeError::Call(PrError::Gh(format!("GitHub has no PR #{number}"))))?;
    let reply = graphql(serde_json::json!({
        "query": "mutation($id:ID!,$oid:GitObjectID!){enablePullRequestAutoMerge(input:{pullRequestId:$id,mergeMethod:SQUASH,expectedHeadOid:$oid}){pullRequest{number}}}",
        "variables": { "id": pull_request_id, "oid": head_sha },
    }))
    .await?;
    if let Some(message) = reply.error_message() {
        return Err(AutoMergeError::Refused(message));
    }
    Ok(())
}

/// A GraphQL reply: `errors` first, then the little `data` May reads.
#[derive(serde::Deserialize)]
struct GraphReply {
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<GraphIssue>,
}

#[derive(serde::Deserialize)]
struct GraphIssue {
    message: String,
}

impl GraphReply {
    /// The joined GraphQL error messages, when any arrived.
    fn error_message(&self) -> Option<String> {
        (!self.errors.is_empty()).then(|| {
            self.errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        })
    }

    /// `data.repository.pullRequest.id`.
    fn pull_request_id(&self) -> Option<String> {
        self.data
            .as_ref()?
            .pointer("/repository/pullRequest/id")?
            .as_str()
            .map(str::to_string)
    }
}

/// The immediate pinned squash merge of `number`, the auto-merge fallback.
async fn merge_now<T: GitHubTokenSource + Sync>(
    api: &GitHubRest<T>,
    owner_repo: &str,
    number: i64,
    head_sha: &str,
) -> Result<(), PrError> {
    api.request(
        zenwave::Method::PUT,
        format!("{}/repos/{owner_repo}/pulls/{number}/merge", api.api_base()),
        Some(serde_json::json!({
            "sha": head_sha,
            "merge_method": "squash",
        })),
    )
    .await
    .map_err(|e| PrError::Gh(e.to_string()))?;
    Ok(())
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

/// One entry of `git status --porcelain=v1 -z`: two status letters, a space
/// and the path.
struct PorcelainStatus<'a> {
    /// The entry renames or copies a path; the next entry is its origin.
    renamed: bool,
    path: &'a str,
}

impl<'a> PorcelainStatus<'a> {
    fn parse(entry: &'a str) -> Self {
        let mut chars = entry.chars();
        let (index, worktree) = (chars.next(), chars.next());
        let separator = chars.next();
        assert_eq!(separator, Some(' '), "porcelain entry {entry:?}");
        Self {
            renamed: matches!(index, Some('R' | 'C')) || matches!(worktree, Some('R' | 'C')),
            path: chars.as_str(),
        }
    }
}

/// The serialized land of the PR's pinned commit `sha`, whose work starts at
/// `base`: merge-tree preflight, checkout-overlap check, merge, then the ref
/// is dropped. Runs inside `blocking::unblock` under the per-repo lock.
fn land(
    host: &HostGit,
    checkout: &str,
    refname: &str,
    base: &str,
    sha: &str,
    number: i64,
    lock: &async_lock::Mutex<()>,
) -> Result<String, String> {
    let git = |dir: Option<&Path>, args: &[&str]| host_git(host, dir, args);
    let _guard = lock.lock_blocking();
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
    // must not intersect the paths the PR's own work touched. A clone based
    // on the checkout's work in progress holds those files in its base
    // commit, so the diff starts at the PR's base, not at the checkout's HEAD.
    let dirty = git(
        Some(checkout_path),
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .map_err(|e| e.to_string())?
    .split('\0')
    .filter(|entry| !entry.is_empty())
    .scan(false, |skip_origin, entry| {
        // A rename lists its origin as a bare path entry after the entry.
        if std::mem::take(skip_origin) {
            return Some(None);
        }
        let status = PorcelainStatus::parse(entry);
        *skip_origin = status.renamed;
        Some(Some(status.path.to_string()))
    })
    .flatten()
    .collect::<std::collections::HashSet<_>>();
    let touched = git(
        Some(checkout_path),
        &["diff", "--name-only", &format!("{base}..{sha}")],
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

/// One commit's short hash and raw message parsed from `pr_commit_messages.sh`.
struct OwnedCommit {
    short_sha: String,
    message: String,
}

impl OwnedCommit {
    fn as_ref(&self) -> crate::harness::publish_review::CommitMessage<'_> {
        crate::harness::publish_review::CommitMessage {
            short_sha: &self.short_sha,
            message: &self.message,
        }
    }
}

fn parse_commit_messages(output: &str) -> Vec<OwnedCommit> {
    output
        .split('\0')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (short_sha, message) = entry
                .split_once('\n')
                .map_or((entry, ""), |(sha, msg)| (sha.trim(), msg.trim()));
            Some(OwnedCommit {
                short_sha: short_sha.to_string(),
                message: message.to_string(),
            })
        })
        .collect()
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
    sigil: char,
    number: i64,
    /// The merge commit in the checkout; `None` for a PR merged upstream.
    merge_sha: Option<&'a str>,
    clone_dir: &'a str,
    clone: CloneFate,
    remote_branch: RemoteBranch,
    /// What became of each local checkout's post-merge pull.
    pull_sync: Vec<SyncedCheckout>,
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
pub(crate) enum RemoteBranch {
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

/// Prints the short hashes and raw commit messages `base..head`, oldest first,
/// NUL-separated; run in the session clone.
#[derive(Template)]
#[template(path = "pr_commit_messages.sh", escape = "none")]
struct CommitMessages {
    base: ShellWord,
    head: ShellWord,
}

/// Removes a check's checkout; run in the session root.
#[derive(Template)]
#[template(path = "pr_check_remove.sh", escape = "none")]
struct RemoveCheckClone {
    target: ShellWord,
}

/// `owner/repo` from a GitHub remote URL (https or ssh), or `None`.
pub(crate) fn github_owner_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(".git");
    let tail = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = tail.split('/');
    Some(format!("{}/{}", parts.next()?, parts.next()?))
}

/// Fails when the PR's head upstream is not the commit pinned at `pr update`.
fn ensure_head_pinned(
    number: i64,
    pinned: &str,
    upstream: &str,
    sigil: char,
) -> Result<(), PrError> {
    if pinned == upstream {
        return Ok(());
    }
    Err(PrError::State(format!(
        "PR {sigil}{number}'s head moved: upstream holds {upstream}, the checked commit is {pinned} — \
         run `pr update {number}` to pin the current head and check it again"
    )))
}

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
        BranchState, GitHubRest, LandLocks, PrContext, PrStore, PrTokenSource, git,
        github_owner_repo, host_git, push_pinned, remote_branch,
    };
    use crate::commit_signing::test_user::TestUser;
    use crate::db::{PrFilter, PrState};
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

        /// `<home>/sessions/<id>`: `SessionPaths::workspace_config` lands on
        /// `<tmp>/workspace.toml`, where `config_at` writes.
        fn session_root(&self, id: &str) -> PathBuf {
            self.tmp.path().join("sessions").join(id)
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
                    path: Some(self.checkout.clone()),
                    repository: None,
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
                    review: None,
                    signing: self.signing(id),
                    github_api: PrContext::default_github_api(
                        crate::forge::LiveForgeAuth::default(),
                    ),
                    gitlab_api: None,
                    forge_auth: crate::forge::LiveForgeAuth::default(),
                    land_locks: LandLocks::default(),
                    checkouts: None,
                    task_events: None,
                    ci: None,
                    publish: None,
                    ui_events: None,
                },
                self.sandbox(id).await,
            );
            (store, clone)
        }

        /// A store whose only clone is a GitHub remote clone of `o/r`, with
        /// a GitHub-backed PR #1 (upstream #7) in `state`, and its API at `base`.
        async fn github_store(&self, id: &str, base: &str, state: PrState) -> PrStore {
            let url = "https://github.com/o/r.git";
            self.db
                .insert_repo_clone(
                    id,
                    &crate::db::RepoCloneRow {
                        repo_key: url.to_string(),
                        dir: "r-00000000".to_string(),
                        source: url.to_string(),
                        local: false,
                        base_sha: None,
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
            if state != PrState::Open {
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
                    review: None,
                    signing: self.signing(id),
                    github_api: GitHubRest::with_api_base(
                        PrTokenSource::Fixed("test-token".to_string()),
                        base,
                    ),
                    gitlab_api: None,
                    forge_auth: crate::forge::LiveForgeAuth::default(),
                    land_locks: LandLocks::default(),
                    checkouts: None,
                    task_events: None,
                    ci: None,
                    publish: None,
                    ui_events: None,
                },
                self.sandbox(id).await,
            )
        }

        async fn pr_state(&self) -> PrState {
            self.db
                .get_pr("https://github.com/o/r.git", 7)
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
                            .map_or_default(http_body_util::Collected::to_bytes);
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

    /// A PR whose clone started from the checkout's work in progress touches
    /// the checkout's uncommitted files only through its base commit: the
    /// overlap check compares the dirty files with the PR's own work.
    #[test]
    fn the_overlap_check_ignores_the_work_in_progress_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap();
        git(Some(&repo), &["init", "-q", "-b", "main"]).unwrap();
        git(Some(&repo), &["config", "user.name", "t"]).unwrap();
        git(Some(&repo), &["config", "user.email", "t@t"]).unwrap();
        git(Some(&repo), &["config", "commit.gpgsign", "false"]).unwrap();
        commit_file(&repo, "a.txt");
        git(Some(&repo), &["checkout", "-q", "-b", "side"]).unwrap();
        std::fs::write(repo.join("wip.txt"), "wip").unwrap();
        git(Some(&repo), &["add", "."]).unwrap();
        git(Some(&repo), &["commit", "-qm", "wip"]).unwrap();
        let base = git(Some(&repo), &["rev-parse", "HEAD"]).unwrap();
        commit_file(&repo, "work.txt");
        let sha = git(Some(&repo), &["rev-parse", "HEAD"]).unwrap();
        git(Some(&repo), &["update-ref", "refs/may/pr/1", &sha]).unwrap();
        git(Some(&repo), &["checkout", "-q", "main"]).unwrap();
        // The checkout holds the same uncommitted file the base commit has.
        std::fs::write(repo.join("wip.txt"), "wip").unwrap();

        let host = crate::host_git::HostGit::from_file("/dev/null");
        let lock = async_lock::Mutex::new(());
        let outcome = super::land(
            &host,
            repo.to_str().unwrap(),
            "refs/may/pr/1",
            &base,
            &sha,
            1,
            &lock,
        );
        let error = outcome.unwrap_err();
        assert!(
            !error.contains("uncommitted changes in PR paths"),
            "the base commit's files are not the PR's: {error}"
        );

        std::fs::write(repo.join("work.txt"), "dirty").unwrap();
        let overlap = super::land(
            &host,
            repo.to_str().unwrap(),
            "refs/may/pr/1",
            &base,
            &sha,
            1,
            &lock,
        )
        .unwrap_err();
        assert!(overlap.contains("work.txt"), "{overlap}");
    }

    #[test]
    fn a_porcelain_entry_splits_into_its_status_and_path() {
        let entry = super::PorcelainStatus::parse("R  new name.txt");
        assert!(entry.renamed);
        assert_eq!(entry.path, "new name.txt");
        let entry = super::PorcelainStatus::parse("?? 文件/日本語.txt");
        assert!(!entry.renamed);
        assert_eq!(entry.path, "文件/日本語.txt");
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
        use aither_agent::{ContextSource, Moment};

        let fx = Fixture::new().await;
        let (mut store, clone) = fx.store("s1", None).await;
        let events = crate::harness::working_document::WorkEvents::new();
        store.context.task_events = Some(events.clone());
        let sandbox = tempfile::tempdir().unwrap();
        std::fs::write(
            sandbox.path().join("tasks.md"),
            "## Doing\n- #3 work, PR #1 open\n",
        )
        .unwrap();
        let mut tasks = crate::tasks_md::schema::working_document(sandbox.path(), events);
        tasks.blocks(Moment::Request { reassembled: false }).await;

        commit_file(&clone, "work.txt");
        store.open(None, Some("work"), None, true).await.unwrap();
        store.close(1, None).await.unwrap();

        tasks.blocks(Moment::Request { reassembled: false }).await;
        let alert = tasks
            .blocks(Moment::ToolTurnEnd)
            .await
            .into_iter()
            .next()
            .expect("the row is told");
        let text = alert.block().content();
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
        let store = fx.github_store("s1", &base, PrState::Open).await;
        store.close(7, None).await.unwrap();
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
        assert_eq!(fx.pr_state().await, PrState::Closed);
    }

    #[tokio::test]
    async fn a_failed_upstream_close_leaves_the_row_unchanged() {
        let fx = Fixture::new().await;
        let (base, requests) = mock_api(422).await;
        let store = fx.github_store("s1", &base, PrState::Open).await;
        let err = store.close(7, None).await.unwrap_err();
        assert!(err.to_string().contains("422"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(fx.pr_state().await, PrState::Open);
    }

    #[tokio::test]
    async fn a_merged_or_closed_github_pr_cannot_be_closed_again() {
        for state in [PrState::Merged, PrState::Closed] {
            let fx = Fixture::new().await;
            let (base, requests) = mock_api(200).await;
            let store = fx.github_store("s1", &base, state).await;
            let err = store.close(7, None).await.unwrap_err();
            assert!(err.to_string().contains(state.as_str()), "{err}");
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
            .update_pr(pr.id, PrState::Merged, None, None, None)
            .await
            .unwrap();
        let err = store.close(1, None).await.unwrap_err();
        assert!(err.to_string().contains("merged"), "{err}");
        let pr = fx.db.get_pr(key, 1).await.unwrap().unwrap();
        assert_eq!(pr.state, PrState::Merged);
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
        review: Option<crate::harness::publish_review::PublishReview>,
    ) -> PrStore {
        PrStore::new(
            PrContext {
                db: Some(fx.db.clone()),
                session_id: id.to_string(),
                session_root: fx.session_root(id),
                scratch_root: fx.scratch_root(),
                cwd: fx.checkout.clone(),
                workspace_config,
                review,
                signing: fx.signing(id),
                github_api,
                gitlab_api: None,
                forge_auth: crate::forge::LiveForgeAuth::default(),
                land_locks: LandLocks::default(),
                checkouts: None,
                task_events: None,
                ci: None,
                publish: None,
                ui_events: None,
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
        let (backend, repo) = store.backend_for(&resolved.record);
        assert_eq!(backend, "may");
        assert!(repo.is_none());
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
        assert_eq!(git(Some(&origin_repo), &["for-each-ref"]).unwrap(), "");
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prs_only_finds_its_project_and_reviews_without_a_github_issue_backend() {
        use aither_core::llm::{
            Event, LLMRequest,
            model::{Parameters, Profile},
        };
        use futures::Stream;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct FixedModel {
            reply: &'static str,
            captured: Arc<Mutex<Vec<String>>>,
            calls: Arc<AtomicUsize>,
        }
        #[derive(Debug, thiserror::Error)]
        #[error("fixed error")]
        struct FixedErr;
        impl aither_core::llm::Classify for FixedErr {
            fn failure(&self) -> aither_core::llm::Failure {
                aither_core::llm::Failure::permanent()
            }
        }
        impl aither_core::LanguageModel for FixedModel {
            type Error = FixedErr;
            fn respond(
                &self,
                request: LLMRequest,
            ) -> impl Stream<Item = Result<Event, FixedErr>> + Send {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.captured.lock().unwrap().push(format!("{request:?}"));
                let reply = self.reply.to_string();
                futures_lite::stream::iter([Ok(Event::Text(reply))])
            }
            fn profile(
                &self,
            ) -> impl std::future::Future<Output = Result<Profile, FixedErr>> + Send {
                std::future::ready(Ok(Profile::new("m", "t", "fast", "fast", 100_000)))
            }
        }

        let fx = Fixture::new().await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let review = crate::harness::publish_review::PublishReview::for_slot(
            crate::agent::ModelSlot::new(
                FixedModel {
                    reply: r#"{"decision":"refuse","findings":["mentions internal workflow"]}"#,
                    captured: captured.clone(),
                    calls: calls.clone(),
                },
                Parameters::default(),
            ),
            crate::harness::publish_review::LocalEnvironment::empty(),
        );
        add_github_origin(&fx);
        // A `prs-only` project as existing files carry it: no `repository`
        // key and no GitHub issue backend.
        let config_path = fx.tmp.path().join("workspace.toml");
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
            crate::trust::persisted_for(&config, &fx.repo_key),
            Some(crate::trust::TrustLevel::PrsOnly)
        );
        let store = trusted_store(
            &fx,
            "s1",
            config,
            PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
            Some(review),
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
        assert_eq!(store.backend_for(&resolved.record).0, "github");
        assert_eq!(
            store.backend_for(&resolved.record).1.map(|r| r.full_path()),
            Some("o/r".to_string())
        );
        let project = store.project_for_record(&resolved.record).unwrap();
        assert!(matches!(project.issues, ProjectIssues::May));
        assert_eq!(project.pr.screen, Some(true));
        let err = store
            .review_outbound(crate::harness::publish_review::Publication::pull_request(
                "a title",
                "- a subject\n",
                &[],
            ))
            .await
            .expect_err("the review ran and withheld the text");
        assert!(
            err.to_string().contains("mentions internal workflow"),
            "{err}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let reqs = captured.lock().unwrap();
        assert!(reqs[0].contains("a title") && reqs[0].contains("- a subject"));
        drop(reqs);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_outbound_review_refuses_when_unwired() {
        let fx = Fixture::new().await;
        add_github_origin(&fx);
        let config_path = fx.tmp.path().join("screen-workspace.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[projects]]\npath = {:?}\n\n[projects.pr]\nbackend = \"github\"\nscreen = true\n",
                fx.checkout
            ),
        )
        .unwrap();
        let config = WorkspaceConfig::load(&config_path).unwrap();
        let store = trusted_store(
            &fx,
            "s1",
            config,
            PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
            None,
        )
        .await;
        let err = store
            .review_outbound(crate::harness::publish_review::Publication::pull_request(
                "a title",
                "- a subject\n",
                &[],
            ))
            .await
            .expect_err("no wired review refuses");
        assert!(matches!(err, super::PrError::Screened(_)), "{err}");
    }

    #[test]
    fn a_merge_names_the_checked_commit_and_fails_when_the_head_moved() {
        assert!(super::ensure_head_pinned(7, "abc123", "abc123", '#').is_ok());
        let err = super::ensure_head_pinned(7, "abc123", "def456", '#').unwrap_err();
        assert!(matches!(err, super::PrError::State(_)), "{err}");
        assert!(err.to_string().contains("head moved"), "{err}");
    }

    /// A store whose cwd is no repository at all, for session `id`.
    async fn store_without_checkout(
        fx: &Fixture,
        id: &str,
        workspace_config: WorkspaceConfig,
    ) -> PrStore {
        let elsewhere = fx.tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        PrStore::new(
            PrContext {
                db: Some(fx.db.clone()),
                session_id: id.to_string(),
                session_root: fx.session_root(id),
                scratch_root: fx.scratch_root(),
                cwd: elsewhere,
                workspace_config,
                review: None,
                signing: fx.signing(id),
                github_api: PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
                gitlab_api: None,
                forge_auth: crate::forge::LiveForgeAuth::default(),
                land_locks: LandLocks::default(),
                checkouts: None,
                task_events: None,
                ci: None,
                publish: None,
                ui_events: None,
            },
            fx.sandbox(id).await,
        )
    }

    #[tokio::test]
    async fn a_github_bound_session_resolves_its_clone_with_no_checkout() {
        let fx = Fixture::new().await;
        let config_path = fx.tmp.path().join("remote-workspace.toml");
        crate::trust::persist_github(&config_path, "o/r", crate::trust::TrustLevel::Full).unwrap();
        let config = WorkspaceConfig::load(&config_path).unwrap();
        fx.db.create_session("s1", None, None).await.unwrap();
        fx.db
            .set_session_repo("s1", Some("github:o/r"))
            .await
            .unwrap();
        let store = store_without_checkout(&fx, "s1", config).await;
        // Stand in for the network clone: an existing directory is adopted.
        let seeded = store
            .repos()
            .unwrap()
            .path(&crate::repo_clone::CloneDir::for_repo_key("o/r"));
        std::fs::create_dir_all(&seeded).unwrap();

        let resolved = store.resolve(None).await.unwrap();

        assert_eq!(resolved.dir, seeded);
        assert!(!resolved.record.local);
        assert_eq!(store.backend_for(&resolved.record).0, "github");
        assert_eq!(
            store.backend_for(&resolved.record).1.map(|r| r.full_path()),
            Some("o/r".to_string())
        );
        let project = store.project_for_record(&resolved.record).unwrap();
        assert!(project.path.is_none(), "the project has no checkout");
        assert!(matches!(project.issues, ProjectIssues::Github { .. }));
    }

    #[tokio::test]
    async fn an_unbound_session_without_a_checkout_cannot_resolve() {
        let fx = Fixture::new().await;
        fx.db.create_session("s1", None, None).await.unwrap();
        let store = store_without_checkout(&fx, "s1", WorkspaceConfig::default()).await;
        let Err(error) = store.resolve(None).await else {
            panic!("resolved without a repository");
        };
        assert!(
            error.to_string().contains("not inside a repository"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn with_several_clones_the_cwds_repository_is_chosen() {
        let fx = Fixture::new().await;
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
            PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
            None,
        )
        .await;
        let first = store.resolve(None).await.unwrap();
        // A second clone, of another repository, joins the session.
        let other = store
            .repos()
            .unwrap()
            .path(&crate::repo_clone::CloneDir::for_repo_key("x/y"));
        std::fs::create_dir_all(&other).unwrap();
        fx.db
            .insert_repo_clone(
                "s1",
                &crate::db::RepoCloneRow {
                    repo_key: "x/y".to_string(),
                    dir: crate::repo_clone::CloneDir::for_repo_key("x/y")
                        .as_str()
                        .to_string(),
                    source: "https://github.com/x/y.git".to_string(),
                    local: false,
                    base_sha: None,
                },
            )
            .await
            .unwrap();

        let resolved = store.resolve(None).await.unwrap();

        assert_eq!(resolved.dir, first.dir);
        assert!(resolved.record.local);
        let named = store.resolve(Some("x/y")).await.unwrap();
        assert_eq!(named.dir, other);
    }

    #[tokio::test]
    async fn a_pin_to_the_may_backend_refuses_a_clone_of_the_remote() {
        let fx = Fixture::new().await;
        let record = crate::repo_clone::CloneRecord {
            repo_key: "o/r".to_string(),
            dir: crate::repo_clone::CloneDir::for_repo_key("o/r"),
            source: "https://github.com/o/r.git".to_string(),
            local: false,
            base_sha: None,
        };
        let project = |backend: &str| WorkspaceConfig {
            workdirs: Vec::new(),
            projects: vec![ProjectConfig {
                path: None,
                repository: Some("o/r".parse().unwrap()),
                issues: ProjectIssues::May,
                checks: None,
                pr: ProjectPr {
                    backend: Some(backend.to_string()),
                    ..ProjectPr::default()
                },
            }],
        };
        let pinned = store_without_checkout(&fx, "s1", project("may")).await;
        assert!(pinned.check_pinned_backend(&record, "github").is_err());
        let github = store_without_checkout(&fx, "s2", project("github")).await;
        assert!(github.check_pinned_backend(&record, "github").is_ok());
    }

    #[tokio::test]
    async fn a_project_turns_the_post_merge_pull_off() {
        let fx = Fixture::new().await;
        let record = crate::repo_clone::CloneRecord {
            repo_key: "o/r".to_string(),
            dir: crate::repo_clone::CloneDir::for_repo_key("o/r"),
            source: "https://github.com/o/r.git".to_string(),
            local: false,
            base_sha: None,
        };
        let project = |pull_sync: Option<bool>| WorkspaceConfig {
            workdirs: Vec::new(),
            projects: vec![ProjectConfig {
                path: None,
                repository: Some("o/r".parse().unwrap()),
                issues: ProjectIssues::May,
                checks: None,
                pr: ProjectPr {
                    pull_sync,
                    ..ProjectPr::default()
                },
            }],
        };
        // A recorded checkout of o/r whose pull would be visible if it ran.
        let checkout = fx.tmp.path().join("recorded");
        git(None, &["init", "-q", &checkout.to_string_lossy()]).unwrap();
        git(
            Some(&checkout),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        )
        .unwrap();
        fx.db.touch_repo_usage(&checkout).await.unwrap();

        let off = store_without_checkout(&fx, "s1", project(Some(false))).await;
        let synced = off.pull_sync(&record, "o/r").await;
        assert_eq!(synced.len(), 0);
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
        let first = remote_branch("s1", &first_sha);
        push_pinned(
            &scratch,
            &clone,
            &url,
            &first_sha,
            &first,
            BranchState::New,
            range(&base),
            None,
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
        let second = remote_branch("s1", &second_sha);
        assert_ne!(first, second);
        push_pinned(
            &scratch,
            &clone,
            &url,
            &second_sha,
            &second,
            BranchState::New,
            range(&base),
            None,
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
                range(&base),
                None,
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
            None,
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
            None,
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
        assert!(
            store
                .list(None, PrFilter::All)
                .await
                .unwrap()
                .contains("no PRs")
        );
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
            None,
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
            None,
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
            None,
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
            None,
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
        assert_eq!(
            fx.db.get_pr(key, 1).await.unwrap().unwrap().state,
            PrState::Open
        );

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
            let store = fx.github_store("s1", &base, PrState::Merged).await;
            let repo =
                crate::forge::ForgeRepo::from_cache_key(crate::forge::ForgeKind::GitHub, "o/r")
                    .unwrap();
            let remote = super::Remote::GitHub(repo);

            assert_eq!(store.delete_remote_branch(&remote, "may/s").await, expected);
            assert_eq!(
                requests.lock().unwrap().clone(),
                vec![(
                    "DELETE".to_string(),
                    "/repos/o/r/git/refs/heads/may/s".to_string(),
                    "Bearer test-token".to_string(),
                    String::new(),
                )]
            );

            let refused = store.delete_remote_branch(&remote, "main").await;
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
                sigil: '#',
                number: 3,
                merge_sha,
                clone_dir: "r-1a2b3c4d",
                clone,
                remote_branch,
                pull_sync: Vec::new(),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_prs_on_same_repo_open_land_and_retire_own_workspace() {
        let fx = Fixture::new().await;
        let config_file = fx.tmp.path().join("workspace.toml");
        crate::trust::persist(
            &config_file,
            &fx.checkout,
            None,
            crate::trust::TrustLevel::LocalBranch,
        )
        .unwrap();
        let config = WorkspaceConfig::load(&config_file).unwrap();
        let repos = SessionRepos::new(&fx.session_root("s1"), "s1", fx.db.clone());
        let ws1 = repos
            .request_clone_of(&fx.checkout, &config, &fx.scratch_root(), true)
            .await
            .unwrap()
            .unwrap();
        let ws2 = repos
            .request_clone_of(&fx.checkout, &config, &fx.scratch_root(), true)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(ws1, ws2);
        assert_eq!(fx.db.list_repo_clones("s1").await.unwrap().len(), 2);

        fx.user.identify(&ws1);
        fx.user.identify(&ws2);
        commit_file(&ws1, "one.txt");
        commit_file(&ws2, "two.txt");

        let dir1 = ws1.file_name().unwrap().to_str().unwrap().to_string();
        let dir2 = ws2.file_name().unwrap().to_str().unwrap().to_string();

        let store = PrStore::new(
            PrContext {
                db: Some(fx.db.clone()),
                session_id: "s1".to_string(),
                session_root: fx.session_root("s1"),
                scratch_root: fx.scratch_root(),
                cwd: fx.checkout.clone(),
                workspace_config: config,
                review: None,
                signing: fx.signing("s1"),
                github_api: PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
                gitlab_api: None,
                forge_auth: crate::forge::LiveForgeAuth::default(),
                land_locks: LandLocks::default(),
                checkouts: None,
                task_events: None,
                ci: None,
                publish: None,
                ui_events: None,
            },
            fx.sandbox("s1").await,
        );

        let r1 = store
            .open(Some(&dir1), Some("pr one"), None, true)
            .await
            .unwrap();
        assert!(r1.contains("PR #1"), "{r1}");
        let r2 = store
            .open(Some(&dir2), Some("pr two"), None, true)
            .await
            .unwrap();
        assert!(r2.contains("PR #2"), "{r2}");

        // Closing PR #1 deletes only ws1, leaving ws2 intact.
        store.close(1, None).await.unwrap();
        assert!(!ws1.exists());
        assert!(ws2.exists());
        assert_eq!(fx.db.list_repo_clones("s1").await.unwrap().len(), 1);

        // Merging PR #2 lands two.txt and deletes ws2.
        let merged = store.merge(2, None).await.unwrap();
        assert!(merged.contains("merged PR #2"), "{merged}");
        assert!(!ws2.exists());
        assert!(fx.checkout.join("two.txt").exists());
        assert!(!fx.checkout.join("one.txt").exists());
        assert!(fx.db.list_repo_clones("s1").await.unwrap().is_empty());
    }

    fn mock_review(
        reply: &str,
        env: crate::harness::publish_review::LocalEnvironment,
        credential: Option<(&str, &str)>,
    ) -> (
        crate::harness::publish_review::PublishReview,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        use aither_core::llm::{
            Event, LLMRequest,
            model::{Parameters, Profile},
        };
        use futures::Stream;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct ScriptedPrModel {
            reply: Arc<str>,
            captured: Arc<Mutex<Vec<String>>>,
            calls: Arc<AtomicUsize>,
        }
        #[derive(Debug, thiserror::Error)]
        #[error("scripted error")]
        struct ScriptedErr;
        impl aither_core::llm::Classify for ScriptedErr {
            fn failure(&self) -> aither_core::llm::Failure {
                aither_core::llm::Failure::permanent()
            }
        }
        impl aither_core::LanguageModel for ScriptedPrModel {
            type Error = ScriptedErr;
            fn respond(
                &self,
                request: LLMRequest,
            ) -> impl Stream<Item = Result<Event, ScriptedErr>> + Send {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.captured.lock().unwrap().push(format!("{request:?}"));
                let reply = self.reply.to_string();
                futures_lite::stream::iter([Ok(Event::Text(reply))])
            }
            fn profile(
                &self,
            ) -> impl std::future::Future<Output = Result<Profile, ScriptedErr>> + Send
            {
                std::future::ready(Ok(Profile::new("m", "t", "fast", "fast", 100_000)))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut review = crate::harness::publish_review::PublishReview::for_slot(
            crate::agent::ModelSlot::new(
                ScriptedPrModel {
                    reply: Arc::from(reply),
                    captured: captured.clone(),
                    calls: calls.clone(),
                },
                Parameters::default(),
            ),
            env,
        );
        if let Some((name, value)) = credential {
            review = review.with_credential(name, value);
        }
        (review, calls, captured)
    }

    /// GitHub `pr open` reviews title, body, and commit messages before pushing,
    /// and `pr update` reviews new commit messages and pushes only when clean.
    #[tokio::test(flavor = "multi_thread")]
    async fn github_pr_open_and_update_review_outbound_text() {
        use std::sync::atomic::Ordering;

        let secret = "provider-pr-secret-443322";
        let fx = Fixture::new().await;
        add_github_origin(&fx);
        let config_path = fx.tmp.path().join("workspace.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[projects]]\npath = {:?}\n\n[projects.pr]\nbackend = \"github\"\n",
                fx.checkout
            ),
        )
        .unwrap();
        let config = WorkspaceConfig::load(&config_path).unwrap();
        let env = crate::harness::publish_review::LocalEnvironment {
            system_hosts: vec!["lexo-mbp.local".to_string()],
            local_hosts: Vec::new(),
            tailscale_hosts: Vec::new(),
            home_dirs: vec![PathBuf::from("/Users/lexotest")],
        };

        // 1. `pr open` model refusal blocks before push and includes commit body.
        let (refuse_review, refuse_calls, refuse_captured) = mock_review(
            r#"{"decision":"refuse","findings":["remove session details from commit message"]}"#,
            env.clone(),
            Some(("GITHUB_TOKEN", secret)),
        );
        let open_store = trusted_store(
            &fx,
            "s-open",
            config.clone(),
            PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
            Some(refuse_review),
        )
        .await;
        let seeded = open_store
            .repos()
            .unwrap()
            .path(&crate::repo_clone::CloneDir::for_repo_key("o/r"));
        std::fs::create_dir_all(seeded.parent().unwrap()).unwrap();
        git(
            None,
            &[
                "clone",
                "-q",
                "--origin",
                "upstream",
                &fx.checkout.to_string_lossy(),
                &seeded.to_string_lossy(),
            ],
        )
        .unwrap();
        identify(&seeded);
        git(Some(&seeded), &["config", "commit.gpgsign", "false"]).unwrap();
        git(
            Some(&seeded),
            &[
                "update-ref",
                "refs/remotes/upstream/HEAD",
                "refs/remotes/upstream/master",
            ],
        )
        .or_else(|_| {
            git(
                Some(&seeded),
                &[
                    "update-ref",
                    "refs/remotes/upstream/HEAD",
                    "refs/remotes/upstream/main",
                ],
            )
        })
        .unwrap();
        std::fs::write(seeded.join("fix.txt"), "fix").unwrap();
        git(Some(&seeded), &["add", "."]).unwrap();
        git(
            Some(&seeded),
            &[
                "commit",
                "-m",
                "Fix decoder bounds check",
                "-m",
                "In this session I traced the failure body.",
            ],
        )
        .unwrap();

        let err = open_store
            .open(None, Some("Fix decoder bounds check"), None, true)
            .await
            .expect_err("model refusal stops github pr open");
        assert!(
            matches!(err, super::PrError::Screened(_))
                && err
                    .to_string()
                    .contains("remove session details from commit message"),
            "{err}"
        );
        assert_eq!(refuse_calls.load(Ordering::SeqCst), 1);
        let sent = refuse_captured.lock().unwrap()[0].clone();
        assert!(
            sent.contains("In this session I traced the failure body."),
            "commit body missing from fast-model review input: {sent}"
        );
        assert!(fx.db.get_pr("o/r", 1).await.unwrap().is_none());

        // 2. `pr open` refuses home dir / host name / credential with 0 model calls.
        let (pre_review, pre_calls, _) = mock_review(
            r#"{"decision":"publish","findings":[]}"#,
            env.clone(),
            Some(("GITHUB_TOKEN", secret)),
        );
        let pre_store = trusted_store(
            &fx,
            "s-open",
            config,
            PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
            Some(pre_review),
        )
        .await;
        for bad_title in [
            format!("Token {secret} leaked"),
            "Crash on lexo-mbp.local".to_string(),
            "Log from /Users/lexotest/may".to_string(),
        ] {
            let err = pre_store
                .open(None, Some(&bad_title), None, true)
                .await
                .expect_err("precheck stops open without model call");
            let msg = err.to_string();
            assert!(matches!(err, super::PrError::Screened(_)), "{msg}");
            assert!(!msg.contains(secret), "refusal echoed secret: {msg}");
        }
        assert_eq!(pre_calls.load(Ordering::SeqCst), 0);

        // 3. `pr update` reviews new commits, blocks leaks/refusals, and pushes clean commits.
        let remote = fx.tmp.path().join("update-remote.git");
        let base_sha = git(Some(&fx.checkout), &["rev-parse", "HEAD"]).unwrap();
        seed_remote(&remote, &fx.checkout, &base_sha);
        let session_root = fx.session_root("s-upd");
        let update_clone = session_root.join("repos/r-upd");
        std::fs::create_dir_all(update_clone.parent().unwrap()).unwrap();
        git(
            None,
            &[
                "clone",
                "-q",
                &remote.to_string_lossy(),
                &update_clone.to_string_lossy(),
            ],
        )
        .unwrap();
        fx.user.identify(&update_clone);
        let sandbox = fx.signing_sandbox("s-upd").await;
        std::fs::write(update_clone.join("one.txt"), "one").unwrap();
        sandbox
            .run(&update_clone, "git add . && git commit -m 'initial change'")
            .await
            .unwrap();
        let first_sha = git(Some(&update_clone), &["rev-parse", "HEAD"]).unwrap();
        let branch = "may/s-upd";
        push_pinned(
            &fx.scratch_root(),
            &update_clone,
            &remote.to_string_lossy(),
            &first_sha,
            branch,
            BranchState::New,
            range_of(&fx.signing("s-upd"), &base_sha),
            None,
        )
        .await
        .unwrap();
        let remote_url = remote.to_string_lossy().into_owned();
        fx.db
            .insert_repo_clone(
                "s-upd",
                &crate::db::RepoCloneRow {
                    repo_key: "https://github.com/o/upd.git".to_string(),
                    dir: "r-upd".to_string(),
                    source: remote_url.clone(),
                    local: false,
                    base_sha: Some(base_sha.clone()),
                },
            )
            .await
            .unwrap();
        let pr = fx
            .db
            .create_pr(
                "https://github.com/o/upd.git",
                "s-upd",
                "Initial PR",
                "",
                branch,
                &base_sha,
                "github",
                true,
                None,
                Some(12),
            )
            .await
            .unwrap();
        fx.db
            .update_pr(pr.id, PrState::Open, Some(&first_sha), None, None)
            .await
            .unwrap();

        let make_update_store = |review| {
            PrStore::new(
                PrContext {
                    db: Some(fx.db.clone()),
                    session_id: "s-upd".to_string(),
                    session_root: fx.session_root("s-upd"),
                    scratch_root: fx.scratch_root(),
                    cwd: fx.tmp.path().to_path_buf(),
                    workspace_config: WorkspaceConfig::default(),
                    review: Some(review),
                    signing: fx.signing("s-upd"),
                    github_api: PrContext::default_github_api(
                        crate::forge::LiveForgeAuth::default(),
                    ),
                    gitlab_api: None,
                    forge_auth: crate::forge::LiveForgeAuth::default(),
                    land_locks: LandLocks::default(),
                    checkouts: None,
                    task_events: None,
                    ci: None,
                    publish: None,
                    ui_events: None,
                },
                sandbox.clone(),
            )
        };

        // Add a clean commit -> passes review and pushes to remote.git.
        std::fs::write(update_clone.join("two.txt"), "two").unwrap();
        sandbox
            .run(
                &update_clone,
                "git add . && git commit -m 'Handle empty header' -m 'Return explicit error.'",
            )
            .await
            .unwrap();
        let second_sha = git(Some(&update_clone), &["rev-parse", "HEAD"]).unwrap();
        let (upd_pass, upd_pass_calls, upd_pass_captured) = mock_review(
            r#"{"decision":"publish","findings":[]}"#,
            env.clone(),
            Some(("GITHUB_TOKEN", secret)),
        );
        make_update_store(upd_pass)
            .update(pr.number, None)
            .await
            .expect("clean commit updates PR");
        assert_eq!(upd_pass_calls.load(Ordering::SeqCst), 1);
        let upd_sent = upd_pass_captured.lock().unwrap()[0].clone();
        assert!(
            upd_sent.contains("Handle empty header") && upd_sent.contains("Return explicit error.")
        );
        assert!(!upd_sent.contains("initial change"));
        assert_eq!(
            git(
                Some(&remote),
                &["rev-parse", &format!("refs/heads/{branch}")]
            )
            .unwrap(),
            second_sha
        );

        // Add a commit with internal narration -> model refusal prevents push.
        std::fs::write(update_clone.join("three.txt"), "three").unwrap();
        sandbox
            .run(
                &update_clone,
                "git add . && git commit -m 'Fix after agent retry'",
            )
            .await
            .unwrap();
        let (upd_refuse, upd_refuse_calls, _) = mock_review(
            r#"{"decision":"refuse","findings":["remove agent retry mention"]}"#,
            env.clone(),
            Some(("GITHUB_TOKEN", secret)),
        );
        let err = make_update_store(upd_refuse)
            .update(pr.number, None)
            .await
            .expect_err("model refusal stops update push");
        assert!(
            err.to_string().contains("remove agent retry mention"),
            "{err}"
        );
        assert_eq!(upd_refuse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            git(
                Some(&remote),
                &["rev-parse", &format!("refs/heads/{branch}")]
            )
            .unwrap(),
            second_sha
        );

        // Secret / host name / home dir in commit message stops before any model call.
        git(
            Some(&update_clone),
            &[
                "commit",
                "--amend",
                "-m",
                &format!("Read {secret} from /Users/lexotest on lexo-mbp.local"),
            ],
        )
        .unwrap();
        let (upd_pre, upd_pre_calls, _) = mock_review(
            r#"{"decision":"publish","findings":[]}"#,
            env,
            Some(("GITHUB_TOKEN", secret)),
        );
        let err = make_update_store(upd_pre)
            .update(pr.number, None)
            .await
            .expect_err("precheck stops update before model call");
        assert!(matches!(err, super::PrError::Screened(_)), "{err}");
        assert!(!err.to_string().contains(secret), "{err}");
        assert_eq!(upd_pre_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            git(
                Some(&remote),
                &["rev-parse", &format!("refs/heads/{branch}")]
            )
            .unwrap(),
            second_sha
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pr_list_and_show_surface_provenance_and_survive_clone_retirement() {
        let fx = Fixture::new().await;
        fx.db.create_session("s1", None, None).await.unwrap();
        fx.db.create_session("s2", None, None).await.unwrap();

        let (store1, clone1) = fx.store("s1", None).await;
        let resolved1 = store1.resolve(None).await.unwrap();
        fx.user.identify(&clone1);
        commit_file(&clone1, "a.txt");
        let open_out = store1
            .open(None, Some("Feature A"), None, false)
            .await
            .unwrap();
        assert!(open_out.contains("PR #1"));

        let pr_row = fx
            .db
            .get_pr(resolved1.record.repo_key.as_str(), 1)
            .await
            .unwrap()
            .unwrap();
        fx.db
            .update_pr(
                pr_row.id,
                PrState::Rejected,
                pr_row.head_sha.as_deref(),
                None,
                None,
            )
            .await
            .unwrap();
        fx.db
            .add_pr_event(
                pr_row.id,
                "rejected",
                "s1",
                "main",
                None,
                Some("merge conflict in file.txt"),
            )
            .await
            .unwrap();

        // Retire session 1's clone row and verify another session (s2) can still
        // list and show the PR with full state, detail, repo key, and provenance.
        fx.db
            .delete_repo_clone("s1", resolved1.record.dir.as_str())
            .await
            .unwrap();

        let (store2, _) = fx.store("s2", None).await;
        let listed = store2.list(None, PrFilter::All).await.unwrap();
        assert!(listed.contains("#1"), "{listed}");
        assert!(listed.contains("Feature A"), "{listed}");
        assert!(listed.contains("rejected"), "{listed}");

        let shown = store2.show(1, None).await.unwrap();
        assert!(shown.contains("Feature A"), "{shown}");
        assert!(shown.contains("rejected"), "{shown}");
        assert!(shown.contains("merge conflict in file.txt"), "{shown}");
        assert!(shown.contains("s1"), "{shown}");
        assert!(shown.contains("main"), "{shown}");
        assert!(shown.contains(&resolved1.record.repo_key), "{shown}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn require_review_blocks_agent_merge_and_allows_user_merge_and_close() {
        let fx = Fixture::new().await;
        fx.db.create_session("s1", None, None).await.unwrap();
        let bus = crate::ui_event_bus::UiEventBus::new();
        let mut rx = bus.subscribe();

        let root = fx.session_root("s1");
        let clone = SessionRepos::new(&root, "s1", fx.db.clone())
            .ensure_local_clone(&fx.checkout, &fx.repo_key, &fx.scratch_root())
            .await
            .unwrap();
        fx.user.identify(&clone);

        let project = ProjectConfig {
            path: Some(fx.checkout.clone()),
            repository: None,
            issues: ProjectIssues::default(),
            checks: None,
            pr: ProjectPr {
                auto_merge: Some(false),
                require_review: Some(true),
                ..ProjectPr::default()
            },
        };
        let store = PrStore::new(
            PrContext {
                db: Some(fx.db.clone()),
                session_id: "s1".to_string(),
                session_root: root.clone(),
                scratch_root: fx.scratch_root(),
                cwd: fx.checkout.clone(),
                workspace_config: WorkspaceConfig {
                    workdirs: Vec::new(),
                    projects: vec![project.clone()],
                },
                review: None,
                signing: fx.signing("s1"),
                github_api: PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
                gitlab_api: None,
                forge_auth: crate::forge::LiveForgeAuth::default(),
                land_locks: LandLocks::default(),
                checkouts: None,
                task_events: None,
                ci: None,
                publish: None,
                ui_events: Some(bus.clone()),
            },
            fx.sandbox("s1").await,
        );

        commit_file(&clone, "feature1.txt");
        store.open(None, Some("PR one"), None, false).await.unwrap();
        let pr1 = fx
            .db
            .get_pr(&fx.repo_key.to_string_lossy(), 1)
            .await
            .unwrap()
            .unwrap();
        assert!(!pr1.auto_merge);

        let err = store
            .merge(1, None)
            .await
            .expect_err("require_review blocks agent merge");
        assert!(err.to_string().contains("requires user review"), "{err}");

        let host_ctx = PrContext {
            db: Some(fx.db.clone()),
            session_id: String::new(),
            session_root: root,
            scratch_root: fx.scratch_root(),
            cwd: fx.checkout.clone(),
            workspace_config: WorkspaceConfig {
                workdirs: Vec::new(),
                projects: vec![project],
            },
            review: None,
            signing: fx.user.signing(&fx.tmp.path().join("may"), "user"),
            github_api: PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
            gitlab_api: None,
            forge_auth: crate::forge::LiveForgeAuth::default(),
            land_locks: LandLocks::default(),
            checkouts: None,
            task_events: None,
            ci: None,
            publish: None,
            ui_events: Some(bus.clone()),
        };

        let merged = super::user_merge_pr(&host_ctx, pr1.id).await.unwrap();
        assert_eq!(merged.state, PrState::Merged);
        let evs = fx.db.pr_events(pr1.id).await.unwrap();
        assert!(
            evs.iter()
                .any(|e| e.agent_id == "user" && e.kind == "merged"),
            "{evs:?}"
        );

        // Drain events and check UI bus received PRS topic.
        let mut saw_prs = false;
        while let Ok(topics) = rx.try_recv() {
            if topics.contains(crate::ui_event_bus::UiEventTopics::PRS) {
                saw_prs = true;
            }
        }
        assert!(saw_prs, "expected UiEventTopics::PRS publish");

        // Second PR on another session: closed by user after session deletion (orphaned).
        fx.db.create_session("s2", None, None).await.unwrap();
        let root2 = fx.session_root("s2");
        let clone2 = SessionRepos::new(&root2, "s2", fx.db.clone())
            .ensure_local_clone(&fx.checkout, &fx.repo_key, &fx.scratch_root())
            .await
            .unwrap();
        fx.user.identify(&clone2);
        let store2 = PrStore::new(
            PrContext {
                db: Some(fx.db.clone()),
                session_id: "s2".to_string(),
                session_root: root2,
                scratch_root: fx.scratch_root(),
                cwd: fx.checkout.clone(),
                workspace_config: host_ctx.workspace_config.clone(),
                review: None,
                signing: fx.signing("s2"),
                github_api: PrContext::default_github_api(crate::forge::LiveForgeAuth::default()),
                gitlab_api: None,
                forge_auth: crate::forge::LiveForgeAuth::default(),
                land_locks: LandLocks::default(),
                checkouts: None,
                task_events: None,
                ci: None,
                publish: None,
                ui_events: Some(bus.clone()),
            },
            fx.sandbox("s2").await,
        );
        commit_file(&clone2, "feature2.txt");
        store2
            .open(None, Some("PR two"), None, false)
            .await
            .unwrap();
        let pr2 = fx
            .db
            .get_pr(&fx.repo_key.to_string_lossy(), 2)
            .await
            .unwrap()
            .unwrap();
        fx.db.delete_session("s2").await.unwrap();
        let closed = super::user_close_pr(&host_ctx, pr2.id).await.unwrap();
        assert_eq!(closed.state, PrState::Closed);
        let evs2 = fx.db.pr_events(pr2.id).await.unwrap();
        assert!(
            evs2.iter()
                .any(|e| e.agent_id == "user" && e.kind == "closed"),
            "{evs2:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn merging_pr_with_linked_may_issue_records_landed_by_and_closes_issue() {
        let fx = Fixture::new().await;
        fx.db.create_session("s1", None, None).await.unwrap();
        let repo_key = fx.repo_key.to_string_lossy().to_string();
        let issue = fx
            .db
            .create_issue(&repo_key, "Fix bug", "Details", &[])
            .await
            .unwrap();
        assert_eq!(issue.state, crate::issue::IssueState::Open);

        let (store, clone) = fx.store("s1", None).await;
        fx.user.identify(&clone);
        commit_file(&clone, "fix.txt");
        let out = store
            .open(None, Some("Fix bug"), Some(issue.number), false)
            .await
            .unwrap();
        assert!(out.contains("merged"), "{out}");

        let refreshed = fx
            .db
            .get_issue(&repo_key, issue.number)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.state, crate::issue::IssueState::Closed);
        assert!(
            refreshed
                .comments
                .iter()
                .any(|c| c.body.contains("landed_by PR #1")),
            "{:?}",
            refreshed.comments
        );
    }
}
