//! Issue stores of other projects: the `--repo` target of the read
//! subcommands of `issue`.

use std::path::{Path, PathBuf};

use super::github::{GitHubRest, GitHubTokenSource};
use super::{
    ForgeGitLabAuth, GitHubIssueStore, GitLabIssueStore, IssueError, IssueTargets, MayIssueStore,
    repository_identity,
};
use crate::workspace::GitHubRepository;
use may_config::forge::{ForgeHost, ForgeKind, ForgeRepo};
use may_db::ChatDb;

/// A project's issue store, named by a `--repo` target.
#[derive(Debug)]
pub enum TargetStore<T: GitHubTokenSource> {
    /// Issues of a local repository, kept in the history database.
    May(MayIssueStore),
    /// Issues of a GitHub repository.
    GitHub(GitHubIssueStore<T>),
    /// Issues of a GitLab project.
    GitLab(GitLabIssueStore<ForgeGitLabAuth>),
}

impl<T: GitHubTokenSource + Clone> IssueTargets<T> {
    /// Targets whose local repositories read through `db`, whose GitHub
    /// repositories read through `rest`, and whose GitLab repositories read
    /// through `forge_auth` so a credential set later still applies;
    /// `may_config` resolves a self-hosted target's forge kind. Relative
    /// paths resolve against `cwd`, and the project at a path is looked up
    /// in the workspace config at `workspace_config`.
    #[must_use]
    pub const fn new(
        db: ChatDb,
        cwd: PathBuf,
        workspace_config: PathBuf,
        may_config: std::sync::Arc<may_config::config::MayConfig>,
        rest: GitHubRest<T>,
        forge_auth: may_config::forge::LiveForgeAuth,
    ) -> Self {
        Self {
            db,
            cwd,
            workspace_config,
            may_config,
            rest,
            forge_auth,
        }
    }

    fn gitlab_store(&self, repository: ForgeRepo) -> GitLabIssueStore<ForgeGitLabAuth> {
        let auth = ForgeGitLabAuth::new(
            self.forge_auth.clone(),
            repository.host().as_str().to_string(),
        );
        GitLabIssueStore::new(repository, auth)
    }

    fn github_store(&self, repository: &ForgeRepo) -> Result<TargetStore<T>, IssueError> {
        let github: GitHubRepository = repository.full_path().parse().map_err(
            |error: crate::workspace::GitHubRepositoryError| {
                IssueError::Invalid(format!("--repo {}: {error}", repository.host_and_path()))
            },
        )?;
        let rest = if repository.host().as_str() == may_config::forge::GITHUB_HOST {
            self.rest.clone()
        } else {
            GitHubRest::with_api_base(
                self.rest.token_source().clone(),
                &format!("https://{}/api/v3", repository.host().as_str()),
            )
        };
        Ok(TargetStore::GitHub(GitHubIssueStore::with_rest(
            github, rest,
        )))
    }

    /// The store `target` names.
    ///
    /// # Errors
    /// Returns [`IssueError::Invalid`] when `target` is neither a path nor a
    /// forge coordinate, and [`IssueError::Identity`] when a named path is
    /// not a directory.
    pub async fn resolve(&self, target: &str) -> Result<TargetStore<T>, IssueError> {
        if is_path(target) {
            let path = self.cwd.join(expand_home(target)?);
            let workspace_config = self.workspace_config.clone();
            let (identity, backend) = blocking::unblock(move || {
                let identity = repository_identity(&path)?;
                let config = crate::workspace::WorkspaceConfig::load(&workspace_config)?;
                let backend = config
                    .project_for_identity(&identity)
                    .map(|project| project.issues.clone());
                Ok::<_, IssueError>((identity, backend))
            })
            .await?;
            return Ok(match backend {
                Some(crate::workspace::ProjectIssues::Github { repository }) => {
                    self.github_store(&repository.forge_repo())?
                }
                Some(crate::workspace::ProjectIssues::Gitlab { repository }) => {
                    TargetStore::GitLab(self.gitlab_store(repository))
                }
                _ => TargetStore::May(MayIssueStore::new(
                    self.db.clone(),
                    identity.display().to_string(),
                )),
            });
        }
        let repository = ForgeRepo::parse_binding(target)
            .or_else(|_| parse_unprefixed(target, self.may_config.as_ref()))
            .map_err(|error| IssueError::Invalid(format!("--repo {target}: {error}")))?;
        match repository.kind() {
            ForgeKind::GitHub => self.github_store(&repository),
            ForgeKind::GitLab => Ok(TargetStore::GitLab(self.gitlab_store(repository))),
        }
    }
}

/// The forge repository an unprefixed `--repo` target names: `owner/repo`
/// is GitHub on `github.com`, `host/namespace/repo` the configured forge of
/// `host`, and any other multi-segment path a `gitlab.com` namespace.
fn parse_unprefixed(
    target: &str,
    may_config: &may_config::config::MayConfig,
) -> Result<ForgeRepo, may_config::forge::ForgeRepoError> {
    let segments: Vec<&str> = target.split('/').collect();
    match segments.as_slice() {
        [owner, repo] => ForgeRepo::parse_binding(&format!("github:{owner}/{repo}")),
        [first, rest @ ..] if rest.len() >= 2 && may_config::forge::looks_like_host(first) => {
            let host = ForgeHost::new(first).map_err(|source| {
                may_config::forge::ForgeRepoError::Host((*first).to_string(), source)
            })?;
            let kind = may_config
                .forge_kind(host.as_str())
                .ok_or_else(|| may_config::forge::ForgeRepoError::UnknownKind(first.to_string()))?;
            let (name, namespace) = rest
                .split_last()
                .ok_or_else(|| may_config::forge::ForgeRepoError::Binding(target.to_string()))?;
            ForgeRepo::new(kind, host, &namespace.join("/"), name)
        }
        _ => ForgeRepo::parse_binding(&format!("gitlab:{target}")),
    }
}

fn is_path(target: &str) -> bool {
    Path::new(target).is_absolute()
        || target == "."
        || target == ".."
        || target.starts_with("./")
        || target.starts_with("../")
        || target == "~"
        || target.starts_with("~/")
}

fn expand_home(target: &str) -> Result<PathBuf, IssueError> {
    let Some(rest) = target.strip_prefix('~') else {
        return Ok(PathBuf::from(target));
    };
    let home = dirs::home_dir()
        .ok_or_else(|| IssueError::Invalid(format!("--repo {target}: no home directory")))?;
    Ok(home.join(rest.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::{IssueTargets, TargetStore};
    use crate::issue::GhCliTokenSource;
    use crate::issue::github::GitHubRest;
    use crate::workspace::{ProjectConfig, ProjectIssues, ProjectPr, WorkspaceConfig};
    use may_db::ChatDb;

    /// A path naming a project the workspace config keeps on GitHub resolves
    /// to that repository's store; a path without a project entry resolves to
    /// the local store.
    #[tokio::test]
    async fn a_path_target_follows_the_projects_issue_backend() {
        let dir = tempfile::tempdir().expect("temp dir");
        let configured = dir.path().join("configured");
        let unconfigured = dir.path().join("unconfigured");
        for repo in [&configured, &unconfigured] {
            git2::Repository::init(repo).expect("git init");
        }
        let config_path = dir.path().join("workspace.toml");
        WorkspaceConfig {
            projects: vec![ProjectConfig {
                path: Some(configured.clone()),
                repository: None,
                issues: ProjectIssues::Github {
                    repository: "owner/repo".parse().expect("owner/repo"),
                },
                checks: None,
                pr: ProjectPr::default(),
            }],
            ..WorkspaceConfig::default()
        }
        .save(&config_path)
        .expect("save workspace config");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let targets = IssueTargets::new(
            db,
            dir.path().to_path_buf(),
            config_path,
            std::sync::Arc::new(may_config::config::MayConfig::default()),
            GitHubRest::new(GhCliTokenSource),
            may_config::forge::LiveForgeAuth::default(),
        );

        let resolved = targets
            .resolve(configured.to_str().expect("utf-8"))
            .await
            .expect("configured project");
        let TargetStore::GitHub(store) = resolved else {
            panic!("a GitHub project resolves to its GitHub store");
        };
        assert_eq!(store.repository().to_string(), "owner/repo");

        let resolved = targets
            .resolve(unconfigured.to_str().expect("utf-8"))
            .await
            .expect("unconfigured path");
        assert!(matches!(resolved, TargetStore::May(_)));
    }

    /// `OWNER/REPO`, a `gitlab:` binding and a `host/namespace/repo`
    /// coordinate resolve to the remote store of their forge.
    #[tokio::test]
    async fn forge_targets_resolve_to_their_backend() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let mut may_config = may_config::config::MayConfig::default();
        may_config.forge.insert(
            "git.acme.example".to_string(),
            may_config::forge::ForgeHostConfig {
                kind: Some(may_config::forge::ForgeKind::GitLab),
                mode: may_config::forge::ForgeAuthMode::Cli,
                client_id: None,
            },
        );
        let targets = IssueTargets::new(
            db,
            dir.path().to_path_buf(),
            dir.path().join("workspace.toml"),
            std::sync::Arc::new(may_config),
            GitHubRest::new(GhCliTokenSource),
            may_config::forge::LiveForgeAuth::default(),
        );

        let resolved = targets.resolve("owner/repo").await.expect("github");
        let TargetStore::GitHub(store) = resolved else {
            panic!("owner/repo resolves to a GitHub store");
        };
        assert_eq!(store.repository().to_string(), "owner/repo");

        let resolved = targets
            .resolve("gitlab:gitlab.example/group/sub/repo")
            .await
            .expect("gitlab binding");
        let TargetStore::GitLab(store) = resolved else {
            panic!("a gitlab: binding resolves to a GitLab store");
        };
        assert_eq!(
            store.repository().host_and_path(),
            "gitlab.example/group/sub/repo"
        );

        let resolved = targets
            .resolve("git.acme.example/team/app")
            .await
            .expect("configured host");
        assert!(
            matches!(resolved, TargetStore::GitLab(_)),
            "a configured host's coordinate resolves to its forge store"
        );

        let resolved = targets
            .resolve("group/sub/repo")
            .await
            .expect("gitlab.com namespace");
        assert!(matches!(resolved, TargetStore::GitLab(_)));
    }
}
