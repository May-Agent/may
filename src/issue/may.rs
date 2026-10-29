//! The may issue store: issues in the session's history database.
//!
//! Issues are keyed by the repository identity
//! ([`crate::issue::repository_identity`]), so every session and worktree
//! of one repository on this machine reads and writes the same issues.

use crate::db::{ChatDb, ChatDbError};

use super::{
    IssueBackendInfo, IssueDetail, IssueError, IssueListState, IssueRelationKind, IssueState,
    IssueStore, IssueSummary,
};

/// Issue store over the session's [`ChatDb`].
#[derive(Debug, Clone)]
pub struct MayIssueStore {
    db: ChatDb,
    repo_key: String,
}

impl MayIssueStore {
    /// Store issues for the repository identity `repo_key` in `db`.
    #[must_use]
    pub const fn new(db: ChatDb, repo_key: String) -> Self {
        Self { db, repo_key }
    }
}

impl IssueStore for MayIssueStore {
    fn info(&self) -> IssueBackendInfo {
        IssueBackendInfo {
            name: "may",
            repository: None,
        }
    }

    async fn create(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<IssueDetail, IssueError> {
        Ok(self
            .db
            .create_issue(&self.repo_key, title, body, labels)
            .await?)
    }

    async fn get(&self, number: i64) -> Result<IssueDetail, IssueError> {
        self.db
            .get_issue(&self.repo_key, number)
            .await?
            .ok_or(IssueError::NotFound(number))
    }

    async fn list(
        &self,
        state: IssueListState,
        labels: &[String],
        limit: i64,
    ) -> Result<Vec<IssueSummary>, IssueError> {
        Ok(self
            .db
            .list_issues(&self.repo_key, state, labels, limit)
            .await?)
    }

    async fn edit(
        &self,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<IssueState>,
    ) -> Result<IssueDetail, IssueError> {
        self.db
            .update_issue(&self.repo_key, number, title, body, state)
            .await
            .map_err(db_not_found(number))
    }

    async fn comment(&self, number: i64, body: &str) -> Result<IssueDetail, IssueError> {
        self.db
            .add_issue_comment(&self.repo_key, number, body)
            .await
            .map_err(db_not_found(number))
    }

    async fn edit_labels(
        &self,
        number: i64,
        add: &[String],
        remove: &[String],
    ) -> Result<IssueDetail, IssueError> {
        self.db
            .edit_issue_labels(&self.repo_key, number, add, remove)
            .await
            .map_err(db_not_found(number))
    }

    async fn edit_relation(
        &self,
        number: i64,
        kind: IssueRelationKind,
        target_number: i64,
        add: bool,
    ) -> Result<IssueDetail, IssueError> {
        self.db
            .edit_issue_relation(&self.repo_key, number, kind, target_number, add)
            .await
            .map_err(db_not_found(number))
    }
}

fn db_not_found(number: i64) -> impl FnOnce(ChatDbError) -> IssueError {
    move |error| match error {
        ChatDbError::RowNotFound(_) => IssueError::NotFound(number),
        other => IssueError::Database(other),
    }
}
