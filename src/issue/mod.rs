//! Issue tracking: may issues by default, GitHub when configured.
//!
//! [`IssueStore`] is the issue backend contract. The may backend
//! ([`MayIssueStore`]) stores issues in the session's history database
//! ([`crate::db`]) keyed by the repository identity, so every session and
//! worktree of one repository on this machine shares the same issues. The
//! GitHub backend ([`GitHubIssueStore`]) acts on the repository named by the
//! project's entry in `workspace.toml` (`[[projects]]` with
//! `issues.backend = "github"`). [`resolve_issue_store`] picks the backend
//! and returns it as a [`ResolvedIssueStore`].

pub mod github;
pub mod may;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub use github::{GhCliTokenSource, GitHubIssueStore, GitHubTokenSource};
pub use may::MayIssueStore;

use crate::db::ChatDb;
use crate::workspace::WorkspaceConfigError;

/// Lifecycle state of an issue.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    /// Open issue.
    Open,
    /// Closed issue.
    Closed,
}

impl IssueState {
    /// Persisted / wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

impl fmt::Display for IssueState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IssueState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            other => Err(format!("unknown issue state: {other}")),
        }
    }
}

/// State filter for `issue list`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum IssueListState {
    /// Open issues only.
    Open,
    /// Closed issues only.
    Closed,
    /// Every issue regardless of state.
    All,
}

impl IssueListState {
    /// Persisted / wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

/// Relation kinds tracked between issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRelationKind {
    /// The related issue is a sub-issue (child) of the subject.
    SubIssue,
    /// The subject is blocked by the related issue.
    BlockedBy,
}

impl IssueRelationKind {
    /// Persisted / wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SubIssue => "sub_issue",
            Self::BlockedBy => "blocked_by",
        }
    }
}

/// An issue as shown in `issue list`.
#[derive(Debug, Clone)]
pub struct IssueSummary {
    /// Issue number within the repository.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue state.
    pub state: IssueState,
    /// Label names on the issue.
    pub labels: Vec<String>,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}

/// One comment on an issue.
#[derive(Debug, Clone)]
pub struct IssueComment {
    /// Author handle; `None` for may issues (they have no authors).
    pub author: Option<String>,
    /// Comment body (markdown).
    pub body: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
}

/// A related issue in compact form.
#[derive(Debug, Clone)]
pub struct RelatedIssue {
    /// Issue number within the repository.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue state.
    pub state: IssueState,
}

/// An issue with its labels, comments and relations.
#[derive(Debug, Clone)]
pub struct IssueDetail {
    /// Issue number within the repository.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue body (markdown).
    pub body: String,
    /// Issue state.
    pub state: IssueState,
    /// Label names on the issue.
    pub labels: Vec<String>,
    /// Comments oldest-first.
    pub comments: Vec<IssueComment>,
    /// Sub-issues of this issue.
    pub sub_issues: Vec<RelatedIssue>,
    /// Issues this issue is blocked by.
    pub blocked_by: Vec<RelatedIssue>,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}

/// Error type for issue-backend operations.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    /// Storage failure in the may backend.
    #[error(transparent)]
    Database(#[from] crate::db::ChatDbError),
    /// HTTP transport or API error in the GitHub backend.
    #[error("GitHub API error: {0}")]
    GitHub(String),
    /// The `gh` CLI could not provide a token.
    #[error("gh authentication failed: {0}")]
    GitHubAuth(String),
    /// The referenced issue does not exist.
    #[error("issue #{0} not found")]
    NotFound(i64),
    /// The operation was rejected (for example a self-relation).
    #[error("{0}")]
    Invalid(String),
    /// The operation is reserved for the main agent; the text is rendered from
    /// the refusal template.
    #[error("{0}")]
    Reserved(String),
    /// Workspace configuration could not be read.
    #[error(transparent)]
    WorkspaceConfig(#[from] WorkspaceConfigError),
    /// A workspace directory's repository identity could not be resolved.
    #[error("failed to resolve the repository identity for {}: {source}", .path.display())]
    Identity {
        /// Directory whose identity was being resolved.
        path: PathBuf,
        /// The canonicalize or git-discovery failure.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Backend identity surfaced to the base system prompt.
#[derive(Debug, Clone)]
pub struct IssueBackendInfo {
    /// Short backend name (`may` or `github`).
    pub name: &'static str,
    /// `owner/repo` when the backend is GitHub.
    pub repository: Option<String>,
}

/// An issue store: the backend a session reads and writes issues through.
///
/// Implemented by [`MayIssueStore`] and [`GitHubIssueStore`]; call sites
/// hold the type-erased [`ResolvedIssueStore`] returned by
/// [`resolve_issue_store`].
pub trait IssueStore: Send + Sync + fmt::Debug {
    /// Backend identity for the system prompt.
    fn info(&self) -> IssueBackendInfo;

    /// Create an issue; returns the created detail.
    fn create(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> impl Future<Output = Result<IssueDetail, IssueError>> + Send;

    /// Fetch one issue with labels, comments and relations.
    fn get(&self, number: i64) -> impl Future<Output = Result<IssueDetail, IssueError>> + Send;

    /// List issues, newest first.
    fn list(
        &self,
        state: IssueListState,
        labels: &[String],
        limit: i64,
    ) -> impl Future<Output = Result<Vec<IssueSummary>, IssueError>> + Send;

    /// Update title, body and/or state.
    fn edit(
        &self,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<IssueState>,
    ) -> impl Future<Output = Result<IssueDetail, IssueError>> + Send;

    /// Add a comment; returns the refreshed detail.
    fn comment(
        &self,
        number: i64,
        body: &str,
    ) -> impl Future<Output = Result<IssueDetail, IssueError>> + Send;

    /// Add and/or remove labels; returns the refreshed detail.
    fn edit_labels(
        &self,
        number: i64,
        add: &[String],
        remove: &[String],
    ) -> impl Future<Output = Result<IssueDetail, IssueError>> + Send;

    /// Add or remove a sub-issue / blocked-by edge; returns the refreshed
    /// detail of the subject issue.
    fn edit_relation(
        &self,
        number: i64,
        kind: IssueRelationKind,
        target_number: i64,
        add: bool,
    ) -> impl Future<Output = Result<IssueDetail, IssueError>> + Send;
}

/// The store a session resolved from its workspace configuration.
///
/// The backend is chosen at runtime from `workspace.toml`, so the choice
/// is held here; every method forwards to the selected [`IssueStore`].
#[derive(Debug, Clone)]
pub enum ResolvedIssueStore {
    /// Issues in the session's history database.
    May(MayIssueStore),
    /// Issues of the configured GitHub repository.
    GitHub(GitHubIssueStore<GhCliTokenSource>),
}

impl IssueStore for ResolvedIssueStore {
    fn info(&self) -> IssueBackendInfo {
        match self {
            Self::May(store) => store.info(),
            Self::GitHub(store) => store.info(),
        }
    }

    async fn create(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<IssueDetail, IssueError> {
        match self {
            Self::May(store) => store.create(title, body, labels).await,
            Self::GitHub(store) => store.create(title, body, labels).await,
        }
    }

    async fn get(&self, number: i64) -> Result<IssueDetail, IssueError> {
        match self {
            Self::May(store) => store.get(number).await,
            Self::GitHub(store) => store.get(number).await,
        }
    }

    async fn list(
        &self,
        state: IssueListState,
        labels: &[String],
        limit: i64,
    ) -> Result<Vec<IssueSummary>, IssueError> {
        match self {
            Self::May(store) => store.list(state, labels, limit).await,
            Self::GitHub(store) => store.list(state, labels, limit).await,
        }
    }

    async fn edit(
        &self,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<IssueState>,
    ) -> Result<IssueDetail, IssueError> {
        match self {
            Self::May(store) => store.edit(number, title, body, state).await,
            Self::GitHub(store) => store.edit(number, title, body, state).await,
        }
    }

    async fn comment(&self, number: i64, body: &str) -> Result<IssueDetail, IssueError> {
        match self {
            Self::May(store) => store.comment(number, body).await,
            Self::GitHub(store) => store.comment(number, body).await,
        }
    }

    async fn edit_labels(
        &self,
        number: i64,
        add: &[String],
        remove: &[String],
    ) -> Result<IssueDetail, IssueError> {
        match self {
            Self::May(store) => store.edit_labels(number, add, remove).await,
            Self::GitHub(store) => store.edit_labels(number, add, remove).await,
        }
    }

    async fn edit_relation(
        &self,
        number: i64,
        kind: IssueRelationKind,
        target_number: i64,
        add: bool,
    ) -> Result<IssueDetail, IssueError> {
        match self {
            Self::May(store) => store.edit_relation(number, kind, target_number, add).await,
            Self::GitHub(store) => store.edit_relation(number, kind, target_number, add).await,
        }
    }
}

/// Resolve the issue store for a workspace.
///
/// `cwd` is the session's working directory, `workspace_config_path` the
/// workspace.toml holding per-project entries, and `db` the session's
/// already-open chat database the may store writes through. A `[[projects]]`
/// entry whose path resolves to the same repository identity selects the
/// GitHub store; everything else falls back to the local store.
///
/// # Errors
/// Returns [`IssueError::WorkspaceConfig`] when the workspace config cannot
/// be loaded and [`IssueError::Identity`] when a repository identity cannot
/// be resolved.
pub fn resolve_issue_store(
    cwd: &Path,
    workspace_config_path: &Path,
    db: ChatDb,
) -> Result<ResolvedIssueStore, IssueError> {
    let identity = repository_identity(cwd)?;
    let config = crate::workspace::WorkspaceConfig::load(workspace_config_path)?;
    for project in &config.projects {
        if repository_identity(&project.path)? != identity {
            continue;
        }
        if let crate::workspace::ProjectIssues::Github { repository } = &project.issues {
            return Ok(ResolvedIssueStore::GitHub(GitHubIssueStore::new(
                repository.clone(),
                GhCliTokenSource,
            )));
        }
    }
    Ok(ResolvedIssueStore::May(MayIssueStore::new(
        db,
        identity.display().to_string(),
    )))
}

/// The repository identity an issue namespace is keyed by.
///
/// The canonicalized git common dir of `dir`
/// (`git rev-parse --git-common-dir`), so all worktrees of one repository
/// share one key, or the canonicalized directory itself when it is not
/// inside a git repository.
///
/// # Errors
/// Returns [`IssueError::Identity`] when `dir` or the discovered git common
/// dir cannot be canonicalized, or when git discovery fails for a reason
/// other than the directory not being a repository.
pub fn repository_identity(dir: &Path) -> Result<PathBuf, IssueError> {
    let canonical = dunce::canonicalize(dir).map_err(|error| IssueError::Identity {
        path: dir.to_path_buf(),
        source: Box::new(error),
    })?;
    match git2::Repository::discover(&canonical) {
        Ok(repo) => dunce::canonicalize(repo.commondir()).map_err(|error| IssueError::Identity {
            path: repo.commondir().to_path_buf(),
            source: Box::new(error),
        }),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(canonical),
        Err(error) => Err(IssueError::Identity {
            path: dir.to_path_buf(),
            source: Box::new(error),
        }),
    }
}
