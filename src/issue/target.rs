//! Issue stores of other projects: the `--repo` target of the read
//! subcommands of `issue`.

use std::path::{Path, PathBuf};

use super::github::{GitHubRest, GitHubTokenSource};
use super::{
    GitHubIssueStore, IssueError, MayIssueStore, project_github_repository, repository_identity,
};
use crate::db::ChatDb;
use crate::workspace::GitHubRepository;

/// A project's issue store, named by a `--repo` target.
#[derive(Debug)]
pub enum TargetStore<T: GitHubTokenSource> {
    /// Issues of a local repository, kept in the history database.
    May(MayIssueStore),
    /// Issues of a GitHub repository.
    GitHub(GitHubIssueStore<T>),
}

/// Resolves `--repo` targets to issue stores.
///
/// A target is a filesystem path (absolute, or starting with `.`, `..` or
/// `~`, relative to the session's working directory) naming a local
/// repository, or `OWNER/REPO` naming a GitHub repository. A path whose
/// project the workspace config keeps on GitHub resolves to that repository's
/// store, as the session's own store does.
#[derive(Debug, Clone)]
pub struct IssueTargets<T> {
    db: ChatDb,
    cwd: PathBuf,
    workspace_config: PathBuf,
    rest: GitHubRest<T>,
}

impl<T: GitHubTokenSource + Clone> IssueTargets<T> {
    /// Targets whose local repositories read through `db` and whose GitHub
    /// repositories read through `rest`; relative paths resolve against `cwd`,
    /// and the project at a path is looked up in the workspace config at
    /// `workspace_config`.
    #[must_use]
    pub const fn new(
        db: ChatDb,
        cwd: PathBuf,
        workspace_config: PathBuf,
        rest: GitHubRest<T>,
    ) -> Self {
        Self {
            db,
            cwd,
            workspace_config,
            rest,
        }
    }

    /// The store `target` names.
    ///
    /// # Errors
    /// Returns [`IssueError::Invalid`] when `target` is neither a path nor
    /// `OWNER/REPO`, and [`IssueError::Identity`] when a named path is not
    /// a directory.
    pub async fn resolve(&self, target: &str) -> Result<TargetStore<T>, IssueError> {
        if is_path(target) {
            let path = self.cwd.join(expand_home(target)?);
            let workspace_config = self.workspace_config.clone();
            let (identity, repository) = blocking::unblock(move || {
                let identity = repository_identity(&path)?;
                let repository = project_github_repository(&identity, &workspace_config)?;
                Ok::<_, IssueError>((identity, repository))
            })
            .await?;
            return Ok(repository.map_or_else(
                || {
                    TargetStore::May(MayIssueStore::new(
                        self.db.clone(),
                        identity.display().to_string(),
                    ))
                },
                |repository| {
                    TargetStore::GitHub(GitHubIssueStore::with_rest(repository, self.rest.clone()))
                },
            ));
        }
        let repository: GitHubRepository = target
            .parse()
            .map_err(|error| IssueError::Invalid(format!("--repo {target}: {error}")))?;
        Ok(TargetStore::GitHub(GitHubIssueStore::with_rest(
            repository,
            self.rest.clone(),
        )))
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
    use crate::db::ChatDb;
    use crate::issue::GhCliTokenSource;
    use crate::issue::github::GitHubRest;
    use crate::workspace::{ProjectConfig, ProjectIssues, ProjectPr, WorkspaceConfig};

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
            GitHubRest::new(GhCliTokenSource),
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
}
