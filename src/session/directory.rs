//! Where a session works on the host.

use std::path::{Path, PathBuf};

use crate::db::entity::sessions;

use super::{MayHomePaths, SessionStore};

/// The directory a session runs in, resolved from its row alone.
///
/// The daemon's own directory is never a candidate: it is wherever the first
/// client happened to start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDirectory {
    /// A checkout on this host: the directory the client named, else the
    /// local checkout of the repository the session is bound to.
    Checkout(PathBuf),
    /// No checkout on this host (a repo-less chat, or a GitHub-bound session
    /// that works in the clone it makes inside its sandbox): the session's
    /// sandbox directory.
    Sandbox(PathBuf),
}

impl SessionDirectory {
    /// Resolves `session`'s directory under the May home `may_dir`.
    #[must_use]
    pub fn of(session: &sessions::Model, may_dir: &Path) -> Self {
        if let Some(cwd) = session.cwd.as_deref() {
            return Self::Checkout(PathBuf::from(cwd));
        }
        if let Some(checkout) = session.repo.as_deref().and_then(local_checkout) {
            return Self::Checkout(checkout);
        }
        Self::Sandbox(
            SessionStore::new(MayHomePaths::new(may_dir.to_path_buf()))
                .sandbox_dir(&session.id, session.profile_id.as_deref()),
        )
    }

    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Checkout(path) | Self::Sandbox(path) => path,
        }
    }

    /// The directory itself, owned.
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        match self {
            Self::Checkout(path) | Self::Sandbox(path) => path,
        }
    }

    /// The host checkout, when the session has one.
    #[must_use]
    pub fn checkout(&self) -> Option<&Path> {
        match self {
            Self::Checkout(path) => Some(path),
            Self::Sandbox(_) => None,
        }
    }
}

/// The checkout a `local:<repository identity>` binding names. The identity
/// is the git common directory: `<checkout>/.git` for a worktree-bearing
/// repository, the directory itself for a bare or non-git one.
fn local_checkout(binding: &str) -> Option<PathBuf> {
    let identity = PathBuf::from(binding.strip_prefix("local:")?);
    if identity.file_name().is_some_and(|name| name == ".git") {
        identity.parent().map(Path::to_path_buf)
    } else {
        Some(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cwd: Option<&str>, repo: Option<&str>) -> sessions::Model {
        sessions::Model {
            id: "s1".into(),
            title: None,
            model: None,
            preview: None,
            message_count: 0,
            profile_id: None,
            pinned: false,
            archived: false,
            folder_id: None,
            pin_sort_order: None,
            created_at: String::new(),
            updated_at: String::new(),
            reasoning_effort: None,
            cwd: cwd.map(Into::into),
            repo: repo.map(Into::into),
            non_interactive: false,
            rules_import: None,
        }
    }

    #[test]
    fn cwd_wins_over_the_repo_binding() {
        let dir = SessionDirectory::of(
            &row(Some("/work/b"), Some("local:/work/a/.git")),
            Path::new("/may"),
        );
        assert_eq!(dir, SessionDirectory::Checkout("/work/b".into()));
    }

    #[test]
    fn local_binding_resolves_to_its_checkout() {
        let dir = SessionDirectory::of(&row(None, Some("local:/work/a/.git")), Path::new("/may"));
        assert_eq!(dir, SessionDirectory::Checkout("/work/a".into()));
    }

    #[test]
    fn repo_less_and_remote_bound_sessions_use_the_sandbox() {
        for repo in [None, Some("github:o/r")] {
            let dir = SessionDirectory::of(&row(None, repo), Path::new("/may"));
            assert!(matches!(&dir, SessionDirectory::Sandbox(path) if path.starts_with("/may")));
            assert_eq!(dir.checkout(), None);
        }
    }
}
