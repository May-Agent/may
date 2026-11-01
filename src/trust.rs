//! Repository trust: the first-contact decision, resolved per repo.
//!
//! A session must not bind a repository it does not know — the first
//! meeting asks the user once, the answer persists to `[[projects]]` in
//! `workspace.toml`, and every later session reuses it. Levels:
//!
//! - `full` — GitHub PRs and GitHub issues
//! - `prs_only` — GitHub PRs, may issues; every published text is screened
//! - `local` — may PR + may issue; nothing leaves the machine

use std::path::{Path, PathBuf};

use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::workspace::{
    GitHubRepository, ProjectConfig, ProjectIssues, ProjectPr, WorkspaceConfig,
    WorkspaceConfigError,
};

/// The persisted trust level for one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// GitHub PRs and issues.
    Full,
    /// GitHub PRs, internal may issues, outbound screen on published text.
    PrsOnly,
    /// May PR + may issue; the session never publishes.
    Local,
}

impl TrustLevel {
    /// Parse a `repo trust set` argument.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "prs-only" | "prs_only" => Some(Self::PrsOnly),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// The stored representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::PrsOnly => "prs-only",
            Self::Local => "local",
        }
    }
}

/// Whether the launch repo is GitHub-backed: decides the option set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    /// Has a GitHub remote — the four-option dialog.
    Github {
        /// `owner/repo` of the GitHub repository.
        owner_repo: String,
    },
    /// Any other git repository — the two-option dialog.
    Git,
}

/// Detect the repo kind for a worktree: GitHub remote → `Github`,
/// otherwise `Git`. Call only inside a repository.
#[must_use]
pub fn repo_kind(worktree: &Path) -> RepoKind {
    let url = std::process::Command::new("git")
        .args(["-C"])
        .arg(worktree)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let owner_repo = url.and_then(|u| {
        let u = u.trim_end_matches(".git");
        let tail = u
            .strip_prefix("https://github.com/")
            .or_else(|| u.strip_prefix("git@github.com:"))
            .or_else(|| u.strip_prefix("ssh://git@github.com/"))
            .or_else(|| u.strip_prefix("http://github.com/"))?;
        let mut parts = tail.split('/');
        Some(format!("{}/{}", parts.next()?, parts.next()?))
    });
    owner_repo.map_or(RepoKind::Git, |owner_repo| RepoKind::Github { owner_repo })
}

/// The GitHub remote URL of a worktree, when it has one.
#[must_use]
pub fn remote_url(worktree: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["-C"])
        .arg(worktree)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The persisted trust for a repository identity, when a `[[projects]]`
/// entry exists for it.
#[must_use]
pub fn trust_for(
    config: &WorkspaceConfig,
    repo_key: &Path,
    _worktree: &Path,
) -> Option<TrustLevel> {
    let project = config.projects.iter().find(|p| {
        crate::issue::repository_identity(&p.path)
            .map(|k| k.as_path() == repo_key)
            .unwrap_or(false)
    })?;
    Some(match (project.pr.backend.as_deref(), &project.issues) {
        (Some("github"), ProjectIssues::Github { .. }) => TrustLevel::Full,
        (Some("github"), ProjectIssues::May) => TrustLevel::PrsOnly,
        _ => TrustLevel::Local,
    })
}

/// Persist a trust decision to `workspace.toml`.
///
/// # Errors
/// Fails when the config cannot be written.
pub fn persist(
    config: &WorkspaceConfig,
    config_path: &Path,
    worktree: &Path,
    kind: Option<&RepoKind>,
    level: TrustLevel,
) -> Result<(), WorkspaceConfigError> {
    let mut config = config.clone();
    let entry = ProjectConfig {
        path: worktree.to_path_buf(),
        issues: match (kind, level) {
            (Some(RepoKind::Github { owner_repo }), TrustLevel::Full) => ProjectIssues::Github {
                repository: owner_repo.parse::<GitHubRepository>().map_err(|e| {
                    WorkspaceConfigError::Read(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        e.to_string(),
                    ))
                })?,
            },
            _ => ProjectIssues::May,
        },
        checks: None,
        pr: ProjectPr {
            backend: match (kind, level) {
                (Some(RepoKind::Github { .. }), TrustLevel::Local) => Some("may".to_string()),
                (Some(RepoKind::Github { .. }), _) => Some("github".to_string()),
                _ => None,
            },
            screen: match level {
                TrustLevel::PrsOnly => Some(true),
                _ => None,
            },
            auto_merge: None,
            require_review: None,
        },
    };
    config.projects.retain(|p| p.path != worktree);
    config.projects.push(entry);
    config.save(config_path)
}

/// The first-contact ask_user request for one repository.
#[must_use]
pub fn question_for(kind: &RepoKind, worktree: &PathBuf) -> PersistedAskUserRequest {
    let (question, options) = match kind {
        RepoKind::Github { owner_repo } => (
            format!(
                "May can open pull requests on {owner_repo}. How should it work with this repository?"
            ),
            vec![
                "full — Allow GitHub PRs and issues".to_string(),
                "prs-only — GitHub PRs only; issues stay internal, published text is screened"
                    .to_string(),
                "local — Work only in the local checkout (may PR + may issue)".to_string(),
                "exit — Do not bind this repository".to_string(),
            ],
        ),
        RepoKind::Git => (
            format!(
                "May works on a private clone of {}. Allow reading and modifying this checkout?",
                worktree.display()
            ),
            vec![
                "local — Allow".to_string(),
                "exit — Do not bind this repository".to_string(),
            ],
        ),
    };
    PersistedAskUserRequest {
        id: crate::id::new(),
        questions: vec![PersistedAskUserQuestion {
            section: "repo-trust".to_string(),
            question,
            option: options,
            multi_select: false,
        }],
    }
}
