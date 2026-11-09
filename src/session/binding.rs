//! The repository a session is bound to.
//!
//! A binding is stored on the session row beside `cwd`, keyed by repository
//! identity. A GitHub-bound session attaches to the abstract repository: its
//! clone comes from the remote and a local checkout is only a sync target. A
//! local-bound session attaches to a checkout's repository identity. A
//! session with no binding is a repo-less chat, a first-class bucket that
//! stays ungrouped.

use std::path::{Path, PathBuf};

use crate::trust::{RepoKind, TrustLevel};
use crate::workspace::GitHubRepository;

/// What a session is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoBinding {
    /// The GitHub repository itself (`github:owner/repo`).
    Github(GitHubRepository),
    /// The repository a local checkout belongs to (`local:<identity>`), where
    /// the identity is [`crate::issue::repository_identity`].
    Local(PathBuf),
}

/// A stored binding that is neither `github:owner/repo` nor `local:<path>`.
#[derive(Debug, thiserror::Error)]
#[error("invalid repository binding '{0}'")]
pub struct BindingError(String);

impl RepoBinding {
    /// Parses the stored form.
    ///
    /// # Errors
    /// Fails when `stored` is not a binding.
    pub fn parse(stored: &str) -> Result<Self, BindingError> {
        let invalid = || BindingError(stored.to_string());
        if let Some(github) = stored.strip_prefix("github:") {
            return github.parse().map(Self::Github).map_err(|_| invalid());
        }
        match stored.strip_prefix("local:") {
            Some(identity) if !identity.is_empty() => Ok(Self::Local(PathBuf::from(identity))),
            _ => Err(invalid()),
        }
    }

    /// The stored form.
    #[must_use]
    pub fn to_stored(&self) -> String {
        match self {
            Self::Github(repository) => format!("github:{repository}"),
            Self::Local(identity) => format!("local:{}", identity.display()),
        }
    }

    /// How a client names the project: `owner/repo` for GitHub, never a
    /// path, and the checkout's path for a local binding.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Github(repository) => repository.to_string(),
            Self::Local(_) => self
                .checkout()
                .map_or_else(String::new, |path| path.display().to_string()),
        }
    }

    /// What a client passes to start a new session on this binding: the
    /// stored form for a GitHub repository, the checkout's path for a local
    /// one.
    #[must_use]
    pub fn launch_spec(&self) -> String {
        self.checkout().map_or_else(
            || self.to_stored(),
            |checkout| checkout.display().to_string(),
        )
    }

    /// The registry key of the session clone serving this binding: the same
    /// key [`crate::repo_clone::CloneSource::key`] gives.
    #[must_use]
    pub fn clone_key(&self) -> String {
        match self {
            Self::Github(repository) => repository.to_string(),
            Self::Local(identity) => identity.to_string_lossy().into_owned(),
        }
    }

    /// The checkout a local binding names. The identity is the git common
    /// directory: `<checkout>/.git` for a worktree-bearing repository, the
    /// directory itself for a bare or non-git one. A GitHub binding names
    /// none.
    #[must_use]
    pub fn checkout(&self) -> Option<PathBuf> {
        let Self::Local(identity) = self else {
            return None;
        };
        if identity.file_name().is_some_and(|name| name == ".git") {
            identity.parent().map(Path::to_path_buf)
        } else {
            Some(identity.clone())
        }
    }

    /// The binding `level` gives the repository of `worktree`: GitHub for the
    /// levels that publish to a GitHub remote, the checkout's repository
    /// identity otherwise.
    ///
    /// # Errors
    /// Fails when the checkout's repository identity cannot be read.
    pub fn for_trust(worktree: &Path, level: TrustLevel) -> Result<Self, std::io::Error> {
        if level.publishes_to_github()
            && let RepoKind::Github { owner_repo } = crate::trust::repo_kind(worktree)
            && let Ok(repository) = owner_repo.parse()
        {
            return Ok(Self::Github(repository));
        }
        crate::issue::repository_identity(worktree)
            .map(Self::Local)
            .map_err(std::io::Error::other)
    }
}

/// The project a stored binding names, as a client shows it. A binding that
/// does not parse is shown as stored.
#[must_use]
pub fn project_label(stored: &str) -> String {
    RepoBinding::parse(stored).map_or_else(|_| stored.to_string(), |binding| binding.label())
}

/// Orders sessions for a list that groups by project: repo-less sessions
/// first and ungrouped, then one run per project sorted by its label. The
/// order inside each run is kept.
#[must_use]
pub fn group_sessions<T>(sessions: Vec<T>, repo_of: impl Fn(&T) -> Option<&str>) -> Vec<T> {
    let mut keyed: Vec<(Option<String>, T)> = sessions
        .into_iter()
        .map(|session| (repo_of(&session).map(project_label), session))
        .collect();
    // Stable: `None` sorts before every label.
    keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
    keyed.into_iter().map(|(_, session)| session).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_group_by_project_with_repo_less_ones_first() {
        let sessions = vec![
            ("a", Some("github:zed/app")),
            ("b", None),
            ("c", Some("local:/work/app/.git")),
            ("d", Some("github:zed/app")),
            ("e", None),
        ];
        let grouped = group_sessions(sessions, |(_, repo)| *repo);
        let ids: Vec<_> = grouped.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, ["b", "e", "c", "a", "d"]);
    }

    #[test]
    fn a_github_project_is_labelled_owner_repo_and_a_local_one_by_path() {
        assert_eq!(project_label("github:acme/app"), "acme/app");
        assert_eq!(project_label("local:/w/app/.git"), "/w/app");
    }

    #[test]
    fn stored_form_round_trips() {
        for stored in ["github:acme/app", "local:/work/app/.git"] {
            assert_eq!(RepoBinding::parse(stored).unwrap().to_stored(), stored);
        }
    }

    #[test]
    fn malformed_bindings_are_refused() {
        for stored in ["acme/app", "github:acme", "local:", "github:a/b/c"] {
            assert!(RepoBinding::parse(stored).is_err(), "{stored}");
        }
    }

    #[test]
    fn clone_keys_match_the_clone_registry() {
        assert_eq!(
            RepoBinding::parse("github:acme/app").unwrap().clone_key(),
            "acme/app"
        );
        assert_eq!(
            RepoBinding::parse("local:/w/.git").unwrap().clone_key(),
            "/w/.git"
        );
    }

    #[test]
    fn a_new_session_under_a_binding_starts_from_its_launch_spec() {
        assert_eq!(
            RepoBinding::parse("github:acme/app").unwrap().launch_spec(),
            "github:acme/app"
        );
        assert_eq!(
            RepoBinding::parse("local:/w/app/.git")
                .unwrap()
                .launch_spec(),
            "/w/app"
        );
    }

    #[test]
    fn only_a_local_binding_names_a_checkout() {
        assert_eq!(RepoBinding::parse("github:a/b").unwrap().checkout(), None);
        assert_eq!(
            RepoBinding::parse("local:/w/app/.git").unwrap().checkout(),
            Some(PathBuf::from("/w/app"))
        );
    }
}
