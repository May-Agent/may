//! Issue stores of other projects: the `--repo` target of the read
//! subcommands of `issue`.

use std::path::{Path, PathBuf};

use super::github::{GitHubRest, GitHubTokenSource};
use super::{GitHubIssueStore, IssueError, MayIssueStore, repository_identity};
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
/// repository, or `OWNER/REPO` naming a GitHub repository.
#[derive(Debug, Clone)]
pub struct IssueTargets<T> {
    db: ChatDb,
    cwd: PathBuf,
    rest: GitHubRest<T>,
}

impl<T: GitHubTokenSource + Clone> IssueTargets<T> {
    /// Targets whose local repositories read through `db` and whose GitHub
    /// repositories read through `rest`; relative paths resolve against `cwd`.
    #[must_use]
    pub const fn new(db: ChatDb, cwd: PathBuf, rest: GitHubRest<T>) -> Self {
        Self { db, cwd, rest }
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
            let identity = blocking::unblock(move || repository_identity(&path)).await?;
            return Ok(TargetStore::May(MayIssueStore::new(
                self.db.clone(),
                identity.display().to_string(),
            )));
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
