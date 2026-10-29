//! Issue row models for the may issue store.
//!
//! Every issue row is scoped to an `issue_projects` row keyed by the
//! repository identity, so all worktrees of one repository share a single
//! issue namespace.

/// Row in `issue_projects`: one record per repository identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct ProjectModel {
    /// Project row id.
    pub id: i64,
    /// Repository identity key (canonicalized git common dir, or the
    /// canonical project directory for non-git workspaces).
    pub repo_key: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
}

/// Row in `issues`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Issue row id.
    pub id: i64,
    /// Owning project row id.
    pub project_id: i64,
    /// Per-project issue number shown to users and the agent.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue body (markdown).
    pub body: String,
    /// Issue state (`open` | `closed`).
    pub state: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}

/// Row in `issue_comments`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct CommentModel {
    /// Comment row id.
    pub id: i64,
    /// Issue row the comment belongs to.
    pub issue_id: i64,
    /// Comment body (markdown).
    pub body: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
}
