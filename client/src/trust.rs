//! Repository trust: the level a session works under, resolved per repo.
//!
//! A persisted `[[projects]]` entry in `workspace.toml` wins. Without one
//! the default is derived: a non-forge repository works at `local-branch`;
//! a forge repository's level comes from the authenticated user's access
//! to it — owned with Issues enabled is `full`, push access or ownership
//! without Issues is `prs-only`, anything else is `local-branch` on a
//! checkout and `prs-only` on a remote-only binding. A successful lookup
//! persists the derived level; a failed one (offline, unauthenticated)
//! falls back without writing, so a transient failure never locks a
//! degraded level in. `/permission` and `repo trust set` override either
//! way. Levels:
//!
//! - `full` — forge PRs and forge issues; work in a session clone
//! - `prs_only` — forge PRs, may issues; every published text is screened
//! - `local_branch` — may PR + may issue on a session clone
//! - `local` — write the launch checkout directly (not recommended)

pub use may_config::repo_kind::{RepoKind, remote_url, repo_kind, repo_kind_with_config};
pub use may_protocol::trust::TrustLevel;

use std::future::Future;
use std::path::{Path, PathBuf};

use askama::Template;

use crate::issue::github::RepositoryAccess;
use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::workspace::{
    GitHubRepository, ProjectConfig, ProjectIssues, ProjectPr, WorkspaceConfig,
    WorkspaceConfigError,
};
use may_protocol::ask_user::{AnswerValue, QuestionAnswer};

/// The trust level a `/permission` answer selects. Custom text that is not
/// a level name decides nothing — the repository stays as it was.
#[must_use]
pub fn level_for_answer(answer: &QuestionAnswer) -> Option<TrustLevel> {
    let text = match &answer.answer {
        AnswerValue::Selected(values) => values.first()?.as_str(),
        AnswerValue::Custom(text) => text.as_str(),
        AnswerValue::Skipped => return None,
    };
    option_level(text)
}

/// The level a question option names. Options read `level — description`;
/// custom text is a bare level name.
#[must_use]
pub fn option_level(option: &str) -> Option<TrustLevel> {
    TrustLevel::parse(option.split(" —").next().unwrap_or(option).trim())
}

/// The persisted trust level in `config_path` for `worktree`'s repository;
/// `None` when no `[[projects]]` entry covers it or the repository
/// identity cannot be resolved.
///
/// # Errors
/// Fails when `config_path` exists but cannot be read or parsed.
pub fn persisted_level(
    worktree: &Path,
    config_path: &Path,
) -> Result<Option<TrustLevel>, WorkspaceConfigError> {
    let Ok(repo_key) = crate::issue::repository_identity(worktree) else {
        return Ok(None);
    };
    let config = WorkspaceConfig::load(config_path)?;
    Ok(persisted_for(&config, &repo_key))
}

/// The persisted trust for a repository identity, when a `[[projects]]`
/// entry exists for it.
#[must_use]
pub fn persisted_for(config: &WorkspaceConfig, repo_key: &Path) -> Option<TrustLevel> {
    config.project_for_identity(repo_key).map(level_of)
}

/// The persisted trust for a GitHub repository, whether its `[[projects]]`
/// entry holds a checkout or only the repository.
#[must_use]
pub fn trust_for_github(config: &WorkspaceConfig, owner_repo: &str) -> Option<TrustLevel> {
    let repository = owner_repo.parse::<GitHubRepository>().ok()?;
    trust_for_forge(
        config,
        &repository.forge_repo(),
        &may_config::config::MayConfig::default(),
    )
}

/// The persisted trust for a forge repository, whether its `[[projects]]`
/// entry holds a checkout or only the repository; `may_config` resolves a
/// checkout's remote.
#[must_use]
pub fn trust_for_forge(
    config: &WorkspaceConfig,
    repo: &may_config::forge::ForgeRepo,
    may_config: &may_config::config::MayConfig,
) -> Option<TrustLevel> {
    config.project_for_forge(repo, may_config).map(level_of)
}

/// The effective trust for a local checkout: the persisted level, else the
/// `local-branch` an undecided checkout resolves to.
#[must_use]
pub fn effective_for(config: &WorkspaceConfig, repo_key: &Path) -> TrustLevel {
    persisted_for(config, repo_key).unwrap_or(TrustLevel::LocalBranch)
}

/// The effective trust for a GitHub-bound session: the persisted level,
/// else the `prs-only` an undecided remote binding resolves to.
#[must_use]
pub fn effective_for_github(config: &WorkspaceConfig, owner_repo: &str) -> TrustLevel {
    trust_for_github(config, owner_repo).unwrap_or(TrustLevel::PrsOnly)
}

/// The effective trust for a forge-bound session: the persisted level,
/// else the `prs-only` an undecided remote binding resolves to.
/// `may_config` resolves a self-hosted checkout's remote.
#[must_use]
pub fn effective_for_forge(
    config: &WorkspaceConfig,
    repo: &may_config::forge::ForgeRepo,
    may_config: &may_config::config::MayConfig,
) -> TrustLevel {
    trust_for_forge(config, repo, may_config).unwrap_or(TrustLevel::PrsOnly)
}

fn level_of(project: &ProjectConfig) -> TrustLevel {
    match (project.pr.backend.as_deref(), &project.issues) {
        (Some("checkout"), _) => TrustLevel::Local,
        (
            Some("github" | "gitlab"),
            ProjectIssues::Github { .. } | ProjectIssues::Gitlab { .. },
        ) => TrustLevel::Full,
        (Some("github" | "gitlab"), ProjectIssues::May) => TrustLevel::PrsOnly,
        _ => TrustLevel::LocalBranch,
    }
}

/// The authenticated user's access to a forge repository, asked while
/// resolving the repository's default trust.
pub trait ForgeProbe {
    /// The viewer's access to `repository`.
    ///
    /// # Errors
    /// Fails when the lookup cannot be made — offline or unauthenticated.
    fn repository_access(
        &self,
        repository: &may_config::forge::ForgeRepo,
    ) -> impl Future<Output = Result<RepositoryAccess, crate::issue::IssueError>> + Send;
}

impl<T: crate::issue::github::GitHubTokenSource> ForgeProbe
    for crate::issue::github::GitHubRest<T>
{
    async fn repository_access(
        &self,
        repository: &may_config::forge::ForgeRepo,
    ) -> Result<RepositoryAccess, crate::issue::IssueError> {
        if repository.kind() != may_config::forge::ForgeKind::GitHub {
            return Err(crate::issue::IssueError::Invalid(format!(
                "{} is not a GitHub repository",
                repository.host_and_path()
            )));
        }
        let github = repository.full_path().parse().map_err(
            |error: crate::workspace::GitHubRepositoryError| {
                crate::issue::IssueError::Invalid(error.to_string())
            },
        )?;
        Self::repository_access(self, &github).await
    }
}

impl<T: crate::issue::gitlab::GitLabTokenSource> ForgeProbe
    for crate::issue::gitlab::GitLabRest<T>
{
    async fn repository_access(
        &self,
        repository: &may_config::forge::ForgeRepo,
    ) -> Result<RepositoryAccess, crate::issue::IssueError> {
        if repository.kind() != may_config::forge::ForgeKind::GitLab {
            return Err(crate::issue::IssueError::Invalid(format!(
                "{} is not a GitLab project",
                repository.host_and_path()
            )));
        }
        Self::repository_access(self, repository).await
    }
}

/// A [`ForgeProbe`] over the daemon's live credentials.
///
/// A GitHub repository is asked through `github` (a GitHub Enterprise host
/// through a client built for its `/api/v3`), a GitLab project through a
/// [`crate::issue::gitlab::GitLabRest`] built for its host on `auth`, so a
/// `/login` or a Web Settings change applies to a probe built before it.
#[derive(Debug, Clone)]
pub struct ForgeProbes<T> {
    /// The `api.github.com` REST client.
    github: crate::issue::github::GitHubRest<T>,
    /// The live authenticator GitLab clients are built per host from.
    auth: may_config::forge::LiveForgeAuth,
}

impl<T> ForgeProbes<T> {
    /// Probes reading `github` for GitHub and `auth` for GitLab.
    #[must_use]
    pub const fn new(
        github: crate::issue::github::GitHubRest<T>,
        auth: may_config::forge::LiveForgeAuth,
    ) -> Self {
        Self { github, auth }
    }
}

impl<T: crate::issue::github::GitHubTokenSource + Clone + Sync> ForgeProbe for ForgeProbes<T> {
    async fn repository_access(
        &self,
        repository: &may_config::forge::ForgeRepo,
    ) -> Result<RepositoryAccess, crate::issue::IssueError> {
        match repository.kind() {
            may_config::forge::ForgeKind::GitHub => {
                let host = repository.host().as_str();
                let rest = if host == may_config::forge::GITHUB_HOST {
                    self.github.clone()
                } else {
                    crate::issue::github::GitHubRest::with_api_base(
                        self.github.token_source().clone(),
                        &format!("https://{host}/api/v3"),
                    )
                };
                ForgeProbe::repository_access(&rest, repository).await
            }
            may_config::forge::ForgeKind::GitLab => {
                crate::issue::gitlab::GitLabRest::new(
                    crate::issue::gitlab::ForgeGitLabAuth::new(
                        self.auth.clone(),
                        repository.host().as_str().to_string(),
                    ),
                    repository.host().as_str(),
                )
                .repository_access(repository)
                .await
            }
        }
    }
}

/// The default trust a forge access lookup gives a local checkout.
///
/// Owned with Issues is `full`, push access `prs-only`, anything else —
/// lookup failure included — `local-branch`.
#[must_use]
pub const fn default_for_checkout(access: Option<RepositoryAccess>) -> TrustLevel {
    match access {
        Some(access) if access.owned && access.has_issues => TrustLevel::Full,
        Some(access) if access.owned || access.can_push => TrustLevel::PrsOnly,
        Some(_) | None => TrustLevel::LocalBranch,
    }
}

/// The default trust a forge access lookup gives a remote-only binding.
///
/// Owned with Issues is `full`, anything else — lookup failure included —
/// `prs-only`, so remote cloning still works and issues stay internal.
#[must_use]
pub const fn default_for_remote(access: Option<RepositoryAccess>) -> TrustLevel {
    match access {
        Some(access) if access.owned && access.has_issues => TrustLevel::Full,
        Some(_) | None => TrustLevel::PrsOnly,
    }
}

/// The trust of the repository `worktree` belongs to.
///
/// The persisted level when `workspace.toml` holds one, else the derived
/// default — persisted when the forge access lookup succeeded, so a
/// concurrent `/permission` or `repo trust set` decision is never
/// clobbered.
///
/// # Errors
/// Fails when the config cannot be read or written, or the repository
/// identity cannot be resolved.
pub async fn resolve_checkout<P: ForgeProbe + Sync>(
    config_path: &Path,
    worktree: &Path,
    probe: &P,
) -> Result<TrustLevel, WorkspaceConfigError> {
    let (config, identity, kind) = {
        let (config_path, worktree) = (config_path.to_path_buf(), worktree.to_path_buf());
        // Loading matches a project by its checkout's remote, and reading
        // the kind and the identity runs git: off the executor.
        blocking::unblock(move || {
            let config = WorkspaceConfig::load(&config_path)?;
            let identity = crate::issue::repository_identity(&worktree)
                .map_err(|e| invalid_input(format!("repository identity: {e}")))?;
            let kind = repo_kind(&worktree);
            Ok::<_, WorkspaceConfigError>((config, identity, kind))
        })
        .await?
    };
    if let Some(level) = persisted_for(&config, &identity) {
        return Ok(level);
    }
    let RepoKind::Forge { repo } = &kind else {
        // A non-forge repository defaults to local-branch without
        // persisting: a forge `origin` added later is picked up.
        return Ok(TrustLevel::LocalBranch);
    };
    let repository = repo.clone();
    let access = match probe.repository_access(&repository).await {
        Ok(access) => access,
        Err(error) => {
            // Offline or unauthenticated: fall back without persisting so
            // a transient failure never locks in a degraded level.
            tracing::debug!(%error, repo = %repository.host_and_path(), "forge access lookup failed; defaulting trust");
            return Ok(TrustLevel::LocalBranch);
        }
    };
    let level = default_for_checkout(Some(access));
    let (config_path, worktree) = (config_path.to_path_buf(), worktree.to_path_buf());
    blocking::unblock(move || persist_checkout_decision(&config_path, &worktree, &kind, level))
        .await
}

/// The trust of a forge repository bound with no checkout.
///
/// The persisted level when one exists, else the derived default —
/// persisted when the access lookup succeeded; `may_config` matches a
/// self-hosted checkout's project entry.
///
/// # Errors
/// Fails when the config cannot be read or written.
pub async fn resolve_forge_remote<P: ForgeProbe + Sync>(
    config_path: &Path,
    repository: &may_config::forge::ForgeRepo,
    may_config: &may_config::config::MayConfig,
    probe: &P,
) -> Result<TrustLevel, WorkspaceConfigError> {
    let config = {
        let config_path = config_path.to_path_buf();
        blocking::unblock(move || WorkspaceConfig::load(&config_path)).await?
    };
    if let Some(level) = trust_for_forge(&config, repository, may_config) {
        return Ok(level);
    }
    let access = match probe.repository_access(repository).await {
        Ok(access) => access,
        Err(error) => {
            tracing::debug!(%error, repo = %repository.host_and_path(), "forge access lookup failed; defaulting trust");
            return Ok(TrustLevel::PrsOnly);
        }
    };
    let level = default_for_remote(Some(access));
    let (config_path, repository, may_config) = (
        config_path.to_path_buf(),
        repository.clone(),
        may_config.clone(),
    );
    blocking::unblock(move || {
        persist_remote_decision(&config_path, &repository, &may_config, level)
    })
    .await
}

/// Write `level` for `worktree`'s repository unless a decision landed
/// meanwhile; the level actually in force is returned.
fn persist_checkout_decision(
    config_path: &Path,
    worktree: &Path,
    kind: &RepoKind,
    level: TrustLevel,
) -> Result<TrustLevel, WorkspaceConfigError> {
    let mut config = WorkspaceConfig::load(config_path)?;
    let identity = crate::issue::repository_identity(worktree)
        .map_err(|e| invalid_input(format!("repository identity: {e}")))?;
    if let Some(existing) = persisted_for(&config, &identity) {
        return Ok(existing);
    }
    apply(&mut config, worktree, Some(kind), level)?;
    config.save(config_path)?;
    Ok(level)
}

/// Write `level` for `repository` unless a decision landed meanwhile; the
/// level actually in force is returned.
fn persist_remote_decision(
    config_path: &Path,
    repository: &may_config::forge::ForgeRepo,
    may_config: &may_config::config::MayConfig,
    level: TrustLevel,
) -> Result<TrustLevel, WorkspaceConfigError> {
    let mut config = WorkspaceConfig::load(config_path)?;
    if let Some(existing) = trust_for_forge(&config, repository, may_config) {
        return Ok(existing);
    }
    Decision {
        issues: issues_of(repository, level),
        backend: Some(repository.kind().binding_prefix().to_string()),
        screen: (level == TrustLevel::PrsOnly).then_some(true),
    }
    .store(
        &mut config,
        |p| p.covers_forge(repository, may_config),
        None,
        Some(repository.clone()),
    );
    config.save(config_path)?;
    Ok(level)
}

/// Persist a trust decision for a GitHub repository with no local checkout.
/// Only the levels that publish to GitHub apply: there is no checkout to
/// clone locally or write.
///
/// # Errors
/// Fails when the level does not publish to GitHub, `owner_repo` is not
/// `owner/repo`, or the config cannot be read or written.
pub fn persist_github(
    config_path: &Path,
    owner_repo: &str,
    level: TrustLevel,
) -> Result<(), WorkspaceConfigError> {
    let repository = owner_repo
        .parse::<GitHubRepository>()
        .map_err(|e| invalid_input(e.to_string()))?;
    persist_forge(config_path, &repository.forge_repo(), level)
}

/// Persist a trust decision for a forge repository with no local checkout.
/// Only the levels that publish to the forge apply: there is no checkout
/// to clone locally or write.
///
/// # Errors
/// Fails when the level does not publish to the forge, or the config
/// cannot be read or written.
pub fn persist_forge(
    config_path: &Path,
    repo: &may_config::forge::ForgeRepo,
    level: TrustLevel,
) -> Result<(), WorkspaceConfigError> {
    if !level.publishes_to_forge() {
        return Err(invalid_input(format!(
            "{} needs a local checkout; {} has none",
            level.as_str(),
            repo.host_and_path()
        )));
    }
    let mut config = WorkspaceConfig::load(config_path)?;
    let decision = Decision {
        issues: issues_of(repo, level),
        backend: Some(repo.kind().binding_prefix().to_string()),
        screen: (level == TrustLevel::PrsOnly).then_some(true),
    };
    let may_config = may_config::config::MayConfig::default();
    decision.store(
        &mut config,
        |p| p.covers_forge(repo, &may_config),
        None,
        Some(repo.clone()),
    );
    config.save(config_path)
}

/// The issue backend `level` gives a forge repository: its own issues at
/// `full`, may issues elsewhere.
fn issues_of(repo: &may_config::forge::ForgeRepo, level: TrustLevel) -> ProjectIssues {
    if level != TrustLevel::Full {
        return ProjectIssues::May;
    }
    match repo.kind() {
        may_config::forge::ForgeKind::GitHub => format!("{}/{}", repo.namespace(), repo.name())
            .parse()
            .map_or(ProjectIssues::May, |repository| ProjectIssues::Github {
                repository,
            }),
        may_config::forge::ForgeKind::GitLab => ProjectIssues::Gitlab {
            repository: repo.clone(),
        },
    }
}

fn invalid_input(message: String) -> WorkspaceConfigError {
    WorkspaceConfigError::Read(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

/// The fields of a project entry a trust decision owns.
struct Decision {
    issues: ProjectIssues,
    backend: Option<String>,
    screen: Option<bool>,
}

impl Decision {
    /// Write the decision into the project `matches` picks, or push a new
    /// entry at `path` for `repository` when none matches. The rest of an
    /// existing entry stays.
    fn store(
        self,
        config: &mut WorkspaceConfig,
        matches: impl Fn(&ProjectConfig) -> bool,
        path: Option<PathBuf>,
        repository: Option<may_config::forge::ForgeRepo>,
    ) {
        if let Some(entry) = config.projects.iter_mut().find(|p| matches(p)) {
            entry.issues = self.issues;
            entry.pr.backend = self.backend;
            entry.pr.screen = self.screen;
        } else {
            config.projects.push(ProjectConfig {
                path,
                repository,
                issues: self.issues,
                checks: None,
                pr: ProjectPr {
                    backend: self.backend,
                    screen: self.screen,
                    ..ProjectPr::default()
                },
            });
        }
    }
}

/// Apply a trust decision to `config`: only the fields trust owns (issue
/// backend, `pr.backend`, `pr.screen`) change; `checks` and the rest of
/// the entry stay. A repository without an entry gets one at `worktree`.
fn apply(
    config: &mut WorkspaceConfig,
    worktree: &Path,
    kind: Option<&RepoKind>,
    level: TrustLevel,
) -> Result<(), WorkspaceConfigError> {
    let identity = crate::issue::repository_identity(worktree)
        .map_err(|e| invalid_input(format!("repository identity: {e}")))?;
    let repo = match kind {
        Some(RepoKind::Forge { repo }) => Some(repo.clone()),
        _ => None,
    };
    let issues = repo
        .as_ref()
        .map_or(ProjectIssues::May, |repo| issues_of(repo, level));
    let backend = match (kind, level) {
        (_, TrustLevel::Local) => Some("checkout".to_string()),
        (Some(RepoKind::Forge { repo }), TrustLevel::LocalBranch) => Some(
            match repo.kind() {
                may_config::forge::ForgeKind::GitHub | may_config::forge::ForgeKind::GitLab => {
                    "may"
                }
            }
            .to_string(),
        ),
        (Some(RepoKind::Forge { repo }), _) => Some(repo.kind().binding_prefix().to_string()),
        _ => None,
    };
    let screen = (level == TrustLevel::PrsOnly).then_some(true);
    // A repository trusted before it had a checkout adopts this one.
    let adopted = repo.and_then(|repo| {
        config
            .projects
            .iter()
            .position(|p| p.path.is_none() && p.repository.as_ref() == Some(&repo))
    });
    if let Some(index) = adopted {
        config.projects[index].path = Some(worktree.to_path_buf());
    }
    Decision {
        issues,
        backend,
        screen,
    }
    .store(
        config,
        |p| p.has_identity(&identity),
        Some(worktree.to_path_buf()),
        None,
    );
    if level == TrustLevel::Local {
        config.add_workdir(worktree.to_path_buf(), Some("direct checkout".to_string()));
    } else {
        config.remove_workdir(worktree);
    }
    Ok(())
}

/// Persist a trust decision to `workspace.toml`.
///
/// The file is read afresh and the entry for the worktree's repository
/// identity is updated in place: only the fields trust owns (issue backend,
/// `pr.backend`, `pr.screen`) change; `checks` and the rest of the entry
/// stay. A repository without an entry gets one at `worktree`.
///
/// # Errors
/// Fails when the config cannot be read, parsed or written, or the
/// repository identity cannot be resolved.
pub fn persist(
    config_path: &Path,
    worktree: &Path,
    kind: Option<&RepoKind>,
    level: TrustLevel,
) -> Result<(), WorkspaceConfigError> {
    let mut config = WorkspaceConfig::load(config_path)?;
    apply(&mut config, worktree, kind, level)?;
    config.save(config_path)
}

/// A workspace config in which `worktree`'s repository is decided at
/// `level`, persisted through [`persist`] into `dir`.
///
/// # Panics
/// Panics when the trust cannot be persisted or reloaded.
#[must_use]
pub fn config_at(dir: &Path, worktree: &Path, level: TrustLevel) -> WorkspaceConfig {
    let path = dir.join("workspace.toml");
    persist(&path, worktree, Some(&repo_kind(worktree)), level).expect("persist trust");
    WorkspaceConfig::load(&path).expect("load trust")
}

#[derive(askama::Template)]
#[template(path = "repo_trust_question.txt", escape = "none")]
struct RepoTrustQuestion {
    forge_repo: Option<String>,
    worktree: String,
}

/// The `/permission` `ask_user` request for one repository.
///
/// # Panics
/// Panics if the question template fails to render, which typed input prevents.
#[must_use]
pub fn question_for(kind: &RepoKind, worktree: &Path) -> PersistedAskUserRequest {
    let options = match kind {
        RepoKind::Forge { repo } => {
            let forge = match repo.kind() {
                may_config::forge::ForgeKind::GitHub => "GitHub",
                may_config::forge::ForgeKind::GitLab => "GitLab",
            };
            vec![
                format!("full — Allow {forge} pull requests and issues"),
                format!(
                    "prs-only — {forge} pull requests only; issues stay internal, published text is screened"
                ),
                "local-branch — Isolated clone and local PRs; the checkout stays read-only"
                    .to_string(),
                "local — Write the checkout directly (not recommended)".to_string(),
            ]
        }
        RepoKind::Git => vec![
            "local-branch — Isolated clone and local PRs; the checkout stays read-only".to_string(),
            "local — Write the checkout directly (not recommended)".to_string(),
        ],
    };
    let question = RepoTrustQuestion {
        forge_repo: match kind {
            RepoKind::Forge { repo } => Some(repo.host_and_path()),
            RepoKind::Git => None,
        },
        worktree: worktree.display().to_string(),
    }
    .render()
    .expect("a repo trust question renders typed input");
    PersistedAskUserRequest {
        id: may_protocol::id::new(),
        questions: vec![PersistedAskUserQuestion {
            section: "repo-trust".to_string(),
            question,
            option: options,
            multi_select: false,
            secret: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::IssueError;
    use may_protocol::ask_user::{AnswerValue, QuestionAnswer};
    use std::process::Command;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        dir
    }

    /// `dir` is `dir` inside a git worktree whose `origin` names
    /// `https://github.com/acme/app.git`.
    fn github_checkout() -> tempfile::TempDir {
        let dir = git_repo();
        let status = Command::new("git")
            .args(["remote", "add", "origin", "https://github.com/acme/app.git"])
            .current_dir(dir.path())
            .status()
            .expect("git remote");
        assert!(status.success());
        dir
    }

    fn identity(path: &Path) -> std::path::PathBuf {
        crate::issue::repository_identity(path).expect("identity")
    }

    /// A [`ForgeProbe`] answering with `access`, or offline when `None`.
    struct Stub(Option<RepositoryAccess>);

    impl ForgeProbe for Stub {
        fn repository_access(
            &self,
            _repository: &may_config::forge::ForgeRepo,
        ) -> impl Future<Output = Result<RepositoryAccess, IssueError>> + Send {
            std::future::ready(
                self.0
                    .ok_or_else(|| IssueError::GitHubAuth("offline".to_string())),
            )
        }
    }

    const fn access(owned: bool, can_push: bool, has_issues: bool) -> RepositoryAccess {
        RepositoryAccess {
            owned,
            can_push,
            has_issues,
        }
    }

    #[test]
    fn persist_round_trip_github_levels() {
        let repo = git_repo();
        let worktree = repo.path();
        let config_path = repo.path().join("workspace.toml");
        let kind = github();
        persist(&config_path, worktree, Some(&kind), TrustLevel::Full).expect("full");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        let key = identity(worktree);
        assert_eq!(persisted_for(&loaded, &key), Some(TrustLevel::Full));

        persist(&config_path, worktree, Some(&kind), TrustLevel::LocalBranch)
            .expect("local-branch");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(persisted_for(&loaded, &key), Some(TrustLevel::LocalBranch));
        assert!(!loaded.has_write_access(worktree));

        persist(&config_path, worktree, Some(&kind), TrustLevel::Local).expect("local");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(persisted_for(&loaded, &key), Some(TrustLevel::Local));
        assert!(loaded.has_write_access(worktree));
        assert_eq!(loaded.projects[0].pr.backend.as_deref(), Some("checkout"));
    }

    fn forge_repo(binding: &str) -> may_config::forge::ForgeRepo {
        may_config::forge::ForgeRepo::parse_binding(binding).expect("binding")
    }

    fn github() -> RepoKind {
        RepoKind::Forge {
            repo: forge_repo("github:acme/app"),
        }
    }

    fn seed_project(config_path: &Path, repo: &Path) {
        let config = WorkspaceConfig {
            projects: vec![ProjectConfig {
                path: Some(repo.to_path_buf()),
                repository: None,
                issues: ProjectIssues::May,
                checks: Some("cargo test".to_string()),
                pr: ProjectPr {
                    auto_merge: Some(false),
                    require_review: Some(true),
                    ..ProjectPr::default()
                },
            }],
            ..WorkspaceConfig::default()
        };
        config.save(config_path).expect("seed");
    }

    #[test]
    fn re_setting_trust_keeps_checks_and_pr_policy() {
        let repo = git_repo();
        let config_path = repo.path().join("workspace.toml");
        seed_project(&config_path, repo.path());
        for level in [TrustLevel::Full, TrustLevel::Full, TrustLevel::PrsOnly] {
            persist(&config_path, repo.path(), Some(&github()), level).expect("persist");
            let loaded = WorkspaceConfig::load(&config_path).expect("load");
            assert_eq!(loaded.projects.len(), 1);
            let project = &loaded.projects[0];
            assert_eq!(project.checks.as_deref(), Some("cargo test"));
            assert_eq!(project.pr.auto_merge, Some(false));
            assert_eq!(project.pr.require_review, Some(true));
            assert_eq!(persisted_for(&loaded, &identity(repo.path())), Some(level));
        }
    }

    #[test]
    fn trust_set_from_a_linked_worktree_covers_the_main_checkout() {
        let repo = git_repo();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("git");
            assert!(status.success());
        };
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let linked_parent = tempfile::tempdir().expect("tempdir");
        let linked = linked_parent.path().join("linked");
        git(&["worktree", "add", "-q", linked.to_str().expect("utf8")]);
        let config_path = repo.path().join("workspace.toml");
        seed_project(&config_path, repo.path());

        persist(
            &config_path,
            &linked,
            Some(&github()),
            TrustLevel::LocalBranch,
        )
        .expect("persist");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].checks.as_deref(), Some("cargo test"));
        for dir in [repo.path(), linked.as_path()] {
            assert_eq!(
                persisted_for(&loaded, &identity(dir)),
                Some(TrustLevel::LocalBranch)
            );
        }
    }

    #[test]
    fn persist_keeps_fields_changed_in_the_file_since_the_caller_loaded() {
        let repo = git_repo();
        let config_path = repo.path().join("workspace.toml");
        persist(&config_path, repo.path(), Some(&github()), TrustLevel::Full).expect("first");
        let mut newer = WorkspaceConfig::load(&config_path).expect("load");
        newer.projects[0].checks = Some("cargo clippy".to_string());
        newer.save(&config_path).expect("save");
        persist(
            &config_path,
            repo.path(),
            Some(&github()),
            TrustLevel::PrsOnly,
        )
        .expect("second");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(loaded.projects[0].checks.as_deref(), Some("cargo clippy"));
    }

    #[test]
    fn several_entries_for_one_repository_fail_to_load() {
        let repo = git_repo();
        let config_path = repo.path().join("workspace.toml");
        let sub = repo.path().join("sub");
        std::fs::create_dir(&sub).expect("mkdir");
        let entry = |path: &Path| ProjectConfig {
            path: Some(path.to_path_buf()),
            repository: None,
            issues: ProjectIssues::May,
            checks: None,
            pr: ProjectPr::default(),
        };
        WorkspaceConfig {
            projects: vec![entry(repo.path()), entry(&sub)],
            ..WorkspaceConfig::default()
        }
        .save(&config_path)
        .expect("save");
        let error = WorkspaceConfig::load(&config_path).expect_err("duplicate");
        assert!(matches!(
            error,
            WorkspaceConfigError::DuplicateProject { .. }
        ));
        assert!(error.to_string().contains("sub"), "{error}");
    }

    #[test]
    fn legacy_may_backend_is_local_branch() {
        let repo = git_repo();
        let mut config = WorkspaceConfig::default();
        config.projects.push(ProjectConfig {
            path: Some(repo.path().to_path_buf()),
            repository: None,
            issues: ProjectIssues::May,
            checks: None,
            pr: ProjectPr {
                backend: Some("may".to_string()),
                ..ProjectPr::default()
            },
        });
        let key = identity(repo.path());
        assert_eq!(persisted_for(&config, &key), Some(TrustLevel::LocalBranch));
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(
            TrustLevel::parse("local-branch"),
            Some(TrustLevel::LocalBranch)
        );
        assert_eq!(
            TrustLevel::parse("local_branch"),
            Some(TrustLevel::LocalBranch)
        );
        assert_eq!(TrustLevel::parse("local"), Some(TrustLevel::Local));
        assert!(TrustLevel::LocalBranch.clones_work());
        assert!(!TrustLevel::Local.clones_work());
    }

    fn answer(value: AnswerValue) -> QuestionAnswer {
        QuestionAnswer {
            section: "repo-trust".to_string(),
            answer: value,
        }
    }

    #[test]
    fn a_gate_answer_maps_to_its_level() {
        for (option, level) in [
            ("full — Allow GitHub PRs and issues", TrustLevel::Full),
            (
                "prs-only — GitHub PRs only; issues stay internal",
                TrustLevel::PrsOnly,
            ),
            (
                "local-branch — Isolated clone and local PRs",
                TrustLevel::LocalBranch,
            ),
            (
                "local — Write the checkout directly (not recommended)",
                TrustLevel::Local,
            ),
        ] {
            let selected = answer(AnswerValue::Selected(vec![option.to_string()]));
            assert_eq!(level_for_answer(&selected), Some(level));
        }
        for value in [
            AnswerValue::Selected(vec!["exit — Do not bind this repository".to_string()]),
            AnswerValue::Custom("not a level".to_string()),
            AnswerValue::Skipped,
        ] {
            assert_eq!(level_for_answer(&answer(value)), None);
        }
        let custom = answer(AnswerValue::Custom("prs-only".to_string()));
        assert_eq!(level_for_answer(&custom), Some(TrustLevel::PrsOnly));
    }

    /// The default each GitHub access result gives, on a checkout and on a
    /// remote-only binding.
    #[test]
    fn github_access_maps_to_default_levels() {
        let owned = Some(access(true, true, true));
        let owned_fork = Some(access(true, true, false));
        let collaborator = Some(access(false, true, true));
        let read_only = Some(access(false, false, true));
        for (probe_result, checkout, remote) in [
            (owned, TrustLevel::Full, TrustLevel::Full),
            (owned_fork, TrustLevel::PrsOnly, TrustLevel::PrsOnly),
            (collaborator, TrustLevel::PrsOnly, TrustLevel::PrsOnly),
            (read_only, TrustLevel::LocalBranch, TrustLevel::PrsOnly),
            (None, TrustLevel::LocalBranch, TrustLevel::PrsOnly),
        ] {
            assert_eq!(
                default_for_checkout(probe_result),
                checkout,
                "{probe_result:?}"
            );
            assert_eq!(default_for_remote(probe_result), remote, "{probe_result:?}");
        }
    }

    fn config_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("workspace.toml")
    }

    fn level_on_disk(config_path: &Path, worktree: &Path) -> Option<TrustLevel> {
        let config = WorkspaceConfig::load(config_path).expect("load");
        persisted_for(&config, &identity(worktree))
    }

    /// An owned GitHub checkout resolves to `full` and persists it.
    #[tokio::test]
    async fn an_owned_github_checkout_resolves_full_and_persists() {
        let repo = github_checkout();
        let config_path = config_path(&repo);
        let probe = Stub(Some(access(true, true, true)));
        let level = resolve_checkout(&config_path, repo.path(), &probe)
            .await
            .expect("resolve");
        assert_eq!(level, TrustLevel::Full);
        assert_eq!(
            level_on_disk(&config_path, repo.path()),
            Some(TrustLevel::Full)
        );
    }

    /// An owned fork with Issues off, a collaborator and a read-only
    /// checkout resolve to `prs-only`, `prs-only` and `local-branch`.
    #[tokio::test]
    async fn github_access_decides_the_checkout_default() {
        for (probe_result, expected) in [
            (Some(access(true, true, false)), TrustLevel::PrsOnly),
            (Some(access(false, true, true)), TrustLevel::PrsOnly),
            (Some(access(false, false, true)), TrustLevel::LocalBranch),
        ] {
            let repo = github_checkout();
            let config_path = config_path(&repo);
            let probe = Stub(probe_result);
            let level = resolve_checkout(&config_path, repo.path(), &probe)
                .await
                .expect("resolve");
            assert_eq!(level, expected, "{probe_result:?}");
            assert_eq!(
                level_on_disk(&config_path, repo.path()),
                Some(expected),
                "{probe_result:?} persists"
            );
        }
    }

    /// An offline lookup falls back to `local-branch` without persisting,
    /// and a later online run resolves and persists `full`.
    #[tokio::test]
    async fn an_offline_lookup_falls_back_without_persisting() {
        let repo = github_checkout();
        let config_path = config_path(&repo);
        let offline = Stub(None);
        let level = resolve_checkout(&config_path, repo.path(), &offline)
            .await
            .expect("resolve");
        assert_eq!(level, TrustLevel::LocalBranch);
        assert_eq!(level_on_disk(&config_path, repo.path()), None);

        let online = Stub(Some(access(true, true, true)));
        let level = resolve_checkout(&config_path, repo.path(), &online)
            .await
            .expect("resolve");
        assert_eq!(level, TrustLevel::Full);
        assert_eq!(
            level_on_disk(&config_path, repo.path()),
            Some(TrustLevel::Full)
        );
    }

    /// A non-GitHub repository resolves to `local-branch` without
    /// persisting, so a GitHub `origin` added later upgrades it.
    #[tokio::test]
    async fn a_plain_git_checkout_resolves_local_branch_unpersisted() {
        let repo = git_repo();
        let config_path = config_path(&repo);
        let probe = Stub(Some(access(true, true, true)));
        let level = resolve_checkout(&config_path, repo.path(), &probe)
            .await
            .expect("resolve");
        assert_eq!(level, TrustLevel::LocalBranch);
        assert_eq!(level_on_disk(&config_path, repo.path()), None);

        // An `origin` added later is picked up and resolves by access.
        let status = Command::new("git")
            .args(["remote", "add", "origin", "https://github.com/acme/app.git"])
            .current_dir(repo.path())
            .status()
            .expect("git remote");
        assert!(status.success());
        let level = resolve_checkout(&config_path, repo.path(), &probe)
            .await
            .expect("resolve");
        assert_eq!(level, TrustLevel::Full);
        assert_eq!(
            level_on_disk(&config_path, repo.path()),
            Some(TrustLevel::Full)
        );
    }

    /// An explicit decision is never overwritten by auto-detection.
    #[tokio::test]
    async fn a_persisted_decision_is_never_overwritten() {
        for level in [
            TrustLevel::Local,
            TrustLevel::LocalBranch,
            TrustLevel::PrsOnly,
            TrustLevel::Full,
        ] {
            let repo = github_checkout();
            let config_path = config_path(&repo);
            persist(&config_path, repo.path(), Some(&github()), level).expect("persist");
            // The probe reports different access than the decision needs.
            let probe = Stub(Some(access(false, false, false)));
            let resolved = resolve_checkout(&config_path, repo.path(), &probe)
                .await
                .expect("resolve");
            assert_eq!(resolved, level, "{level:?} survives the probe");
            assert_eq!(level_on_disk(&config_path, repo.path()), Some(level));
        }
    }

    /// A remote-only `github:owner/repo` binding resolves by access: owned
    /// with Issues persists `full`, read-only access `prs-only`, and an
    /// offline lookup falls back to `prs-only` unpersisted.
    #[tokio::test]
    async fn a_remote_binding_resolves_by_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = config_path(&dir);
        for (probe_result, expected) in [
            (Some(access(true, true, true)), TrustLevel::Full),
            (Some(access(true, true, false)), TrustLevel::PrsOnly),
            (Some(access(false, true, true)), TrustLevel::PrsOnly),
            (Some(access(false, false, true)), TrustLevel::PrsOnly),
        ] {
            let probe = Stub(probe_result);
            let level = resolve_forge_remote(
                &config_path,
                &forge_repo("github:acme/app"),
                &may_config::config::MayConfig::default(),
                &probe,
            )
            .await
            .expect("resolve");
            assert_eq!(level, expected, "{probe_result:?}");
            let config = WorkspaceConfig::load(&config_path).expect("load");
            assert_eq!(
                trust_for_github(&config, "acme/app"),
                Some(expected),
                "{probe_result:?} persists"
            );
            std::fs::remove_file(&config_path).expect("clean");
        }
        let offline = Stub(None);
        let level = resolve_forge_remote(
            &config_path,
            &forge_repo("github:acme/app"),
            &may_config::config::MayConfig::default(),
            &offline,
        )
        .await
        .expect("resolve");
        assert_eq!(level, TrustLevel::PrsOnly);
        let config = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(trust_for_github(&config, "acme/app"), None);
    }

    /// A decision recorded for the remote between the lookup and the write
    /// wins over the derived default.
    #[tokio::test]
    async fn a_concurrent_remote_decision_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = config_path(&dir);
        persist_github(&config_path, "acme/app", TrustLevel::PrsOnly).expect("persist");
        let probe = Stub(Some(access(true, true, true)));
        let level = resolve_forge_remote(
            &config_path,
            &forge_repo("github:acme/app"),
            &may_config::config::MayConfig::default(),
            &probe,
        )
        .await
        .expect("resolve");
        assert_eq!(level, TrustLevel::PrsOnly);
    }
}
