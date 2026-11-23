//! The kind of forge a repository lives on, and how a remote maps to it.

use std::path::Path;

/// Whether the launch repo is forge-backed: decides the option set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    /// Has a forge remote — `full` / `prs-only` / `local-branch` / `local` / exit.
    Forge {
        /// The forge repository the remote names.
        repo: crate::forge::ForgeRepo,
    },
    /// Any other git repository — the two-option dialog.
    Git,
}

/// Detect the repo kind for a worktree, without `[forge]` configuration.
///
/// A `github.com` or `gitlab.com` remote is a `Forge`, anything else `Git`.
/// Call only inside a repository; self-hosted forges need
/// [`repo_kind_with_config`].
#[must_use]
pub fn repo_kind(worktree: &Path) -> RepoKind {
    repo_kind_with_config(worktree, &crate::config::MayConfig::default())
}

/// The GitHub remote URL of a worktree, when it has one.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous git remote helper; async callers wrap in blocking::unblock"
)]
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

/// Detect the repo kind for a worktree as `config` sees it: a remote on
/// `github.com`, `gitlab.com` or a `[forge."<host>"]` entry makes it a
/// `Forge`, anything else `Git`. Call only inside a repository.
#[must_use]
pub fn repo_kind_with_config(worktree: &Path, config: &crate::config::MayConfig) -> RepoKind {
    remote_url(worktree)
        .and_then(|url| crate::forge::ForgeRepo::from_remote_url(&url, config).ok())
        .map_or(RepoKind::Git, |repo| RepoKind::Forge { repo })
}
