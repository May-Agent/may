//! The repository a session is bound to.
//!
//! A binding is stored on the session row beside `cwd`, keyed by repository
//! identity. A forge-bound session attaches to the abstract repository: its
//! clone comes from the remote and a local checkout is only a sync target. A
//! local-bound session attaches to a checkout's repository identity. A
//! session with no binding is a repo-less chat, a first-class bucket that
//! stays ungrouped.
//!
//! The stored form is the repository's canonical binding —
//! `github:owner/repo`, `github:<host>/owner/repo` or
//! `gitlab:<host>/<namespace/...>/<repo>` — beside `local:<identity>`.

use std::path::{Path, PathBuf};

use crate::trust::{RepoKind, TrustLevel};

/// What a session is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoBinding {
    /// The forge repository itself (its canonical binding).
    Forge(crate::forge::ForgeRepo),
    /// The repository a local checkout belongs to (`local:<identity>`), where
    /// the identity is [`crate::issue::repository_identity`].
    Local(PathBuf),
}

/// A stored binding that is neither a forge binding nor `local:<path>`.
#[derive(Debug, thiserror::Error)]
#[error("invalid repository binding '{0}'")]
pub struct BindingError(String);

impl RepoBinding {
    /// Parses the stored form: a canonical forge binding or
    /// `local:<identity>`.
    ///
    /// # Errors
    /// Fails when `stored` is not a binding.
    pub fn parse(stored: &str) -> Result<Self, BindingError> {
        let invalid = || BindingError(stored.to_string());
        if let Some(identity) = stored.strip_prefix("local:") {
            return if identity.is_empty() {
                Err(invalid())
            } else {
                Ok(Self::Local(PathBuf::from(identity)))
            };
        }
        crate::forge::ForgeRepo::parse_binding(stored)
            .map(Self::Forge)
            .map_err(|_| invalid())
    }

    /// The stored form.
    #[must_use]
    pub fn to_stored(&self) -> String {
        match self {
            Self::Forge(repo) => repo.canonical_binding(),
            Self::Local(identity) => format!("local:{}", identity.display()),
        }
    }

    /// How a client names the project: `owner/repo` for `github.com`,
    /// `<host>/<namespace>/<repo>` everywhere else, and the checkout's path
    /// for a local binding.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Forge(repo) => repo.cache_key(),
            Self::Local(_) => self
                .checkout()
                .map_or_else(String::new, |path| path.display().to_string()),
        }
    }

    /// What a client passes to start a new session on this binding: the
    /// stored form for a forge repository, the checkout's path for a local
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
            Self::Forge(repo) => repo.cache_key(),
            Self::Local(identity) => identity.to_string_lossy().into_owned(),
        }
    }

    /// The forge repository of a forge binding.
    #[must_use]
    pub const fn forge_repo(&self) -> Option<&crate::forge::ForgeRepo> {
        match self {
            Self::Forge(repo) => Some(repo),
            Self::Local(_) => None,
        }
    }

    /// The checkout a local binding names. The identity is the git common
    /// directory: `<checkout>/.git` for a worktree-bearing repository, the
    /// directory itself for a bare or non-git one. A forge binding names
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

    /// The binding `level` gives the repository of `worktree`: the forge
    /// repository for the levels that publish to its remote, the checkout's
    /// repository identity otherwise. `may_config` resolves a self-hosted
    /// remote's kind.
    ///
    /// # Errors
    /// Fails when the checkout's repository identity cannot be read.
    pub fn for_trust_with_config(
        worktree: &Path,
        level: TrustLevel,
        may_config: &crate::config::MayConfig,
    ) -> Result<Self, std::io::Error> {
        if level.publishes_to_forge()
            && let RepoKind::Forge { repo } =
                crate::trust::repo_kind_with_config(worktree, may_config)
        {
            return Ok(Self::Forge(repo));
        }
        crate::issue::repository_identity(worktree)
            .map(Self::Local)
            .map_err(std::io::Error::other)
    }

    /// [`for_trust_with_config`](Self::for_trust_with_config) without a
    /// `[forge]` map: `github.com` and `gitlab.com` remotes are still
    /// recognized.
    ///
    /// # Errors
    /// Fails when the checkout's repository identity cannot be read.
    pub fn for_trust(worktree: &Path, level: TrustLevel) -> Result<Self, std::io::Error> {
        Self::for_trust_with_config(worktree, level, &crate::config::MayConfig::default())
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
    fn a_gitlab_project_is_labelled_by_its_coordinate() {
        assert_eq!(
            project_label("gitlab:gitlab.com/group/sub/repo"),
            "gitlab.com/group/sub/repo"
        );
        assert_eq!(
            project_label("gitlab:git.corp.example.com/team/app"),
            "git.corp.example.com/team/app"
        );
    }

    #[test]
    fn stored_form_round_trips() {
        for stored in [
            "github:acme/app",
            "github:ghe.corp.example.com/acme/app",
            "gitlab:gitlab.com/group/sub/repo",
            "gitlab:localhost:8443/team/app",
            "local:/work/app/.git",
        ] {
            assert_eq!(
                RepoBinding::parse(stored).unwrap().to_stored(),
                stored,
                "{stored}"
            );
        }
    }

    #[test]
    fn malformed_bindings_are_refused() {
        for stored in [
            "acme/app",
            "github:acme",
            "local:",
            "github:a/b/c/d",
            "gitlab:only",
        ] {
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
            RepoBinding::parse("gitlab:gitlab.com/group/sub/repo")
                .unwrap()
                .clone_key(),
            "gitlab.com/group/sub/repo"
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
            RepoBinding::parse("gitlab:gitlab.com/group/repo")
                .unwrap()
                .launch_spec(),
            "gitlab:gitlab.com/group/repo"
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
