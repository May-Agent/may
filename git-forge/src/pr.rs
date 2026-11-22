//! The forge side of `pr` — the session's landing object.
//!
//! [`checks`] renders a PR's checks for the agent and the daemon's poller;
//! [`gitlab`] is the merge-request backend; [`merge_watch`] follows a queued
//! PR until it lands or leaves the queue; [`pull_sync`] pulls the landed
//! work into the checkouts that recorded it.

pub mod checks;
pub mod gitlab;
pub mod merge_watch;
pub mod pull_sync;

/// Error type for `pr` operations.
#[derive(Debug, thiserror::Error)]
pub enum PrError {
    /// Database layer failure.
    #[error(transparent)]
    Db(#[from] may_db::ChatDbError),
    /// Clone layer failure.
    #[error(transparent)]
    RepoClone(#[from] may_client::repo_clone::RepoCloneError),
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
    Signing(#[from] may_config::commit_signing::SigningError),
    /// Filesystem failure while staging.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A subagent called an action only the main agent has.
    #[error("`pr {0}` is the main agent's; a subagent has `pr list` and `pr show`")]
    MainAgentOnly(&'static str),
    /// The user's approval to open the PR was refused or could not be had.
    #[error(transparent)]
    Publish(#[from] crate::publish::PublishError),
}

/// What became of a merged PR's branch on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBranch {
    /// A local PR has no remote branch.
    Local,
    /// The branch is gone from the remote.
    Deleted(String),
    /// The branch could not be deleted, and why.
    NotDeleted(String, String),
}
