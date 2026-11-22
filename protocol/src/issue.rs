//! The issue-tracking wire types: states, summaries, comments and targets.

use std::fmt;
use std::str::FromStr;

/// Whether an issue is open or closed.
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

/// Which issues a listing includes.
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

/// One comment on an issue.
#[derive(Debug, Clone)]
pub struct IssueComment {
    /// Comment id on its backend.
    pub id: i64,
    /// Author handle; `None` for may issues (they have no authors).
    pub author: Option<String>,
    /// Comment body (markdown).
    pub body: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
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

/// Relation kinds tracked between issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRelationKind {
    /// The related issue is a sub-issue (child) of the subject.
    SubIssue,
    /// The subject is blocked by the related issue.
    BlockedBy,
}

/// A related issue in compact form.
#[derive(Debug, Clone)]
pub struct RelatedIssue {
    /// `owner/repo` the issue lives in, which a GitHub relation may take
    /// outside the store's own repository; `None` in the may backend, whose
    /// relations stay inside its one repository.
    pub repository: Option<String>,
    /// Issue number within the repository.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue state.
    pub state: IssueState,
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
