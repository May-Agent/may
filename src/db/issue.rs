//! Issue CRUD for the may issue store on [`ChatDb`].
//!
//! Issues are scoped to an `issue_projects` row keyed by the repository
//! identity (canonicalized git common dir, or the canonical directory for
//! non-git workspaces), so every session and worktree of one repository on
//! this machine reads and writes the same issues.

use crate::issue::{
    IssueComment, IssueDetail, IssueListState, IssueRelationKind, IssueState, IssueSummary,
    RelatedIssue,
};

use super::common::{I64ValueRow, row_not_found, storage_error};
use super::entity::issues::{CommentModel, Model};
use super::{ChatDb, ChatDbError, query};

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct LabelNameRow {
    name: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct RelatedIssueRow {
    number: i64,
    title: String,
    state: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct LabelIdRow {
    id: i64,
}

impl ChatDb {
    /// Fetch (creating if needed) the project row for `repo_key`.
    async fn ensure_issue_project(&self, repo_key: &str) -> Result<i64, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT OR IGNORE INTO issue_projects (repo_key, created_at) VALUES (?, ?)",
            repo_key,
            now
        )
        .execute()
        .await?;
        query!(
            self.db,
            "SELECT id AS value FROM issue_projects WHERE repo_key = ?",
            repo_key
        )
        .fetch_one::<I64ValueRow>()
        .await
        .map(|row| row.value)
        .map_err(Into::into)
    }

    /// Fetch the project row id for `repo_key`, when it exists.
    async fn find_issue_project(&self, repo_key: &str) -> Result<Option<i64>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT id AS value FROM issue_projects WHERE repo_key = ?",
            repo_key
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .map(|row| row.value))
    }

    async fn issue_row(&self, project_id: i64, number: i64) -> Result<Option<Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM issues WHERE project_id = ? AND number = ?",
            project_id,
            number
        )
        .fetch_optional::<Model>()
        .await?)
    }

    async fn require_issue(&self, project_id: i64, number: i64) -> Result<Model, ChatDbError> {
        self.issue_row(project_id, number)
            .await?
            .ok_or_else(|| row_not_found(format!("issue #{number}")))
    }

    async fn issue_labels(&self, issue_id: i64) -> Result<Vec<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT label.name AS name FROM issue_labels AS label INNER JOIN issue_label_assignments AS assignment ON assignment.label_id = label.id WHERE assignment.issue_id = ? ORDER BY label.name ASC",
            issue_id
        )
        .fetch_all::<LabelNameRow>()
        .await?
        .into_iter()
        .map(|row| row.name)
        .collect())
    }

    async fn issue_comments(&self, issue_id: i64) -> Result<Vec<IssueComment>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM issue_comments WHERE issue_id = ? ORDER BY id ASC",
            issue_id
        )
        .fetch_all::<CommentModel>()
        .await?
        .into_iter()
        .map(|row| IssueComment {
            author: None,
            body: row.body,
            created_at: row.created_at,
        })
        .collect())
    }

    async fn issue_related(
        &self,
        issue_id: i64,
        kind: IssueRelationKind,
    ) -> Result<Vec<RelatedIssue>, ChatDbError> {
        query!(
            self.db,
            "SELECT issue.number AS number, issue.title AS title, issue.state AS state FROM issue_relations AS relation INNER JOIN issues AS issue ON issue.id = relation.related_issue_id WHERE relation.issue_id = ? AND relation.kind = ? ORDER BY issue.number ASC",
            issue_id,
            kind.as_str()
        )
        .fetch_all::<RelatedIssueRow>()
        .await?
        .into_iter()
        .map(|row| {
            Ok(RelatedIssue {
                number: row.number,
                title: row.title,
                state: row.state.parse().map_err(|error: String| {
                    storage_error(crate::db::common::db_custom_error(
                        "invalid issue state: ",
                        &error,
                    ))
                })?,
            })
        })
        .collect()
    }

    async fn issue_detail(&self, row: Model) -> Result<IssueDetail, ChatDbError> {
        let (labels, comments, sub_issues, blocked_by) = futures_lite::future::try_zip(
            futures_lite::future::try_zip(self.issue_labels(row.id), self.issue_comments(row.id)),
            futures_lite::future::try_zip(
                self.issue_related(row.id, IssueRelationKind::SubIssue),
                self.issue_related(row.id, IssueRelationKind::BlockedBy),
            ),
        )
        .await
        .map(|((labels, comments), (sub_issues, blocked_by))| {
            (labels, comments, sub_issues, blocked_by)
        })?;
        Ok(IssueDetail {
            number: row.number,
            title: row.title,
            body: row.body,
            state: row.state.parse().map_err(|error: String| {
                storage_error(crate::db::common::db_custom_error(
                    "invalid issue state: ",
                    &error,
                ))
            })?,
            labels,
            comments,
            sub_issues,
            blocked_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    /// Fetch (creating if needed) a label row within an existing transaction.
    ///
    /// Runs on the caller's transaction rather than `self.db`: writing through
    /// another pool connection while the transaction holds SQLite's write lock
    /// deadlocks into `database is locked`.
    async fn ensure_issue_label_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        project_id: i64,
        name: &str,
    ) -> Result<i64, ChatDbError> {
        query!(
            tx,
            "INSERT OR IGNORE INTO issue_labels (project_id, name) VALUES (?, ?)",
            project_id,
            name
        )
        .execute()
        .await?;
        query!(
            tx,
            "SELECT id FROM issue_labels WHERE project_id = ? AND name = ?",
            project_id,
            name
        )
        .fetch_one::<LabelIdRow>()
        .await
        .map(|row| row.id)
        .map_err(Into::into)
    }

    /// Create an issue in the project keyed by `repo_key`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn create_issue(
        &self,
        repo_key: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<IssueDetail, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO issues (project_id, number, title, body, state, created_at, updated_at) SELECT ?, COALESCE(MAX(number), 0) + 1, ?, ?, 'open', ?, ? FROM issues WHERE project_id = ?",
            project_id,
            title,
            body,
            now.clone(),
            now,
            project_id
        )
        .execute()
        .await?;
        let row = tx
            .query(
                "SELECT * FROM issues WHERE project_id = ? AND number = (SELECT MAX(number) FROM issues WHERE project_id = ?)",
            )
            .bind(project_id)
            .bind(project_id)
            .fetch_one::<Model>()
            .await?;
        for label in labels {
            let label_id = self
                .ensure_issue_label_in_tx(&mut tx, project_id, label.as_str())
                .await?;
            query!(
                tx,
                "INSERT OR IGNORE INTO issue_label_assignments (issue_id, label_id) VALUES (?, ?)",
                row.id,
                label_id
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        self.issue_detail(row).await
    }

    /// Fetch one issue with labels, comments and relations.
    ///
    /// Returns `Ok(None)` when the project or the issue does not exist.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_issue(
        &self,
        repo_key: &str,
        number: i64,
    ) -> Result<Option<IssueDetail>, ChatDbError> {
        let Some(project_id) = self.find_issue_project(repo_key).await? else {
            return Ok(None);
        };
        let Some(row) = self.issue_row(project_id, number).await? else {
            return Ok(None);
        };
        self.issue_detail(row).await.map(Some)
    }

    /// Issues of `repo_key` updated after `since` (any time when `None`),
    /// oldest update first.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn issues_updated_since(
        &self,
        repo_key: &str,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<IssueSummary>, ChatDbError> {
        let Some(project_id) = self.find_issue_project(repo_key).await? else {
            return Ok(Vec::new());
        };
        let rows = query!(
            self.db,
            "SELECT * FROM issues WHERE project_id = ? AND updated_at > ? ORDER BY updated_at, number LIMIT ?",
            project_id,
            since.unwrap_or(""),
            limit
        )
        .fetch_all::<Model>()
        .await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            summaries.push(IssueSummary {
                number: row.number,
                title: row.title,
                state: row.state.parse().map_err(|error: String| {
                    storage_error(crate::db::common::db_custom_error(
                        "invalid issue state: ",
                        &error,
                    ))
                })?,
                labels: self.issue_labels(row.id).await?,
                updated_at: row.updated_at,
            });
        }
        Ok(summaries)
    }

    /// List issues in the project keyed by `repo_key`, newest first.
    ///
    /// `labels` filters to issues carrying every listed label.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_issues(
        &self,
        repo_key: &str,
        state: IssueListState,
        labels: &[String],
        limit: i64,
    ) -> Result<Vec<IssueSummary>, ChatDbError> {
        let Some(project_id) = self.find_issue_project(repo_key).await? else {
            return Ok(Vec::new());
        };
        let rows = match state {
            IssueListState::All => {
                query!(
                    self.db,
                    "SELECT * FROM issues WHERE project_id = ? ORDER BY number DESC LIMIT ?",
                    project_id,
                    limit
                )
                .fetch_all::<Model>()
                .await?
            }
            _ => {
                query!(
                    self.db,
                    "SELECT * FROM issues WHERE project_id = ? AND state = ? ORDER BY number DESC LIMIT ?",
                    project_id,
                    state.as_str(),
                    limit
                )
                .fetch_all::<Model>()
                .await?
            }
        };
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let issue_labels = self.issue_labels(row.id).await?;
            if !labels.is_empty()
                && !labels
                    .iter()
                    .all(|wanted| issue_labels.iter().any(|label| label == wanted))
            {
                continue;
            }
            summaries.push(IssueSummary {
                number: row.number,
                title: row.title,
                state: row.state.parse().map_err(|error: String| {
                    storage_error(crate::db::common::db_custom_error(
                        "invalid issue state: ",
                        &error,
                    ))
                })?,
                labels: issue_labels,
                updated_at: row.updated_at,
            });
        }
        Ok(summaries)
    }

    /// Update an issue's title, body and/or state.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RowNotFound`] when the issue does not exist.
    pub async fn update_issue(
        &self,
        repo_key: &str,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<IssueState>,
    ) -> Result<IssueDetail, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let row = self.require_issue(project_id, number).await?;
        let title = title.unwrap_or(row.title.as_str()).to_string();
        let body = body.unwrap_or(row.body.as_str()).to_string();
        let state = state.map_or_else(|| row.state.clone(), |s| s.as_str().to_string());
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE issues SET title = ?, body = ?, state = ?, updated_at = ? WHERE id = ?",
            title,
            body,
            state,
            now,
            row.id
        )
        .execute()
        .await?;
        self.issue_detail(self.require_issue(project_id, number).await?)
            .await
    }

    /// Add a comment to an issue and return the refreshed detail.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RowNotFound`] when the issue does not exist.
    pub async fn add_issue_comment(
        &self,
        repo_key: &str,
        number: i64,
        body: &str,
    ) -> Result<IssueDetail, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let row = self.require_issue(project_id, number).await?;
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO issue_comments (issue_id, body, created_at) VALUES (?, ?, ?)",
            row.id,
            body,
            now
        )
        .execute()
        .await?;
        self.issue_detail(row).await
    }

    /// Add and/or remove labels on an issue and return the refreshed detail.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RowNotFound`] when the issue does not exist.
    pub async fn edit_issue_labels(
        &self,
        repo_key: &str,
        number: i64,
        add: &[String],
        remove: &[String],
    ) -> Result<IssueDetail, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let row = self.require_issue(project_id, number).await?;
        let mut tx = self.db.begin().await?;
        for label in add {
            let label_id = self
                .ensure_issue_label_in_tx(&mut tx, project_id, label.as_str())
                .await?;
            query!(
                tx,
                "INSERT OR IGNORE INTO issue_label_assignments (issue_id, label_id) VALUES (?, ?)",
                row.id,
                label_id
            )
            .execute()
            .await?;
        }
        for label in remove {
            query!(
                tx,
                "DELETE FROM issue_label_assignments WHERE issue_id = ? AND label_id IN (SELECT id FROM issue_labels WHERE project_id = ? AND name = ?)",
                row.id,
                project_id,
                label.as_str()
            )
            .execute()
            .await?;
        }
        let now = jiff::Timestamp::now().to_string();
        query!(
            tx,
            "UPDATE issues SET updated_at = ? WHERE id = ?",
            now,
            row.id
        )
        .execute()
        .await?;
        tx.commit().await?;
        self.issue_detail(row).await
    }

    /// Add or remove a sub-issue / blocked-by edge between two issues.
    ///
    /// `kind` is interpreted from the subject issue's perspective:
    /// `SubIssue` records `target` as a child of `number`, `BlockedBy`
    /// records `number` as blocked by `target`.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RowNotFound`] when either issue does not exist,
    /// and [`ChatDbError`] on storage failure.
    pub async fn edit_issue_relation(
        &self,
        repo_key: &str,
        number: i64,
        kind: IssueRelationKind,
        target_number: i64,
        add: bool,
    ) -> Result<IssueDetail, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let row = self.require_issue(project_id, number).await?;
        let target = self.require_issue(project_id, target_number).await?;
        if row.id == target.id {
            return Err(storage_error(
                "issue relation rejected: an issue cannot relate to itself".to_string(),
            ));
        }
        if add {
            query!(
                self.db,
                "INSERT OR IGNORE INTO issue_relations (issue_id, related_issue_id, kind) VALUES (?, ?, ?)",
                row.id,
                target.id,
                kind.as_str()
            )
            .execute()
            .await?;
        } else {
            query!(
                self.db,
                "DELETE FROM issue_relations WHERE issue_id = ? AND related_issue_id = ? AND kind = ?",
                row.id,
                target.id,
                kind.as_str()
            )
            .execute()
            .await?;
        }
        self.issue_detail(row).await
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::issue::IssueListState;

    async fn test_db() -> (tempfile::TempDir, ChatDb) {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        (dir, db)
    }

    #[tokio::test]
    async fn issues_are_scoped_by_repo_key() {
        let (_dir, db) = test_db().await;
        let created = db
            .create_issue("repo-a", "first", "body", &["bug".to_string()])
            .await
            .expect("create");
        assert_eq!(created.number, 1);
        let second = db
            .create_issue("repo-a", "second", "", &[])
            .await
            .expect("create");
        assert_eq!(second.number, 2);

        // A different repository identity sees none of repo-a's issues.
        assert!(
            db.list_issues("repo-b", IssueListState::All, &[], 100)
                .await
                .expect("list")
                .is_empty()
        );
        assert!(db.get_issue("repo-b", 1).await.expect("get").is_none());
        // Numbering is per project.
        let foreign = db
            .create_issue("repo-b", "other repo", "", &[])
            .await
            .expect("create");
        assert_eq!(foreign.number, 1);
    }

    #[tokio::test]
    async fn issue_lifecycle_round_trip() {
        let (_dir, db) = test_db().await;
        db.create_issue("repo", "parent", "p", &[])
            .await
            .expect("create");
        db.create_issue("repo", "child", "c", &[])
            .await
            .expect("create");
        db.create_issue("repo", "blocker", "b", &[])
            .await
            .expect("create");

        let detail = db
            .edit_issue_relation("repo", 1, IssueRelationKind::SubIssue, 2, true)
            .await
            .expect("sub-issue add");
        assert_eq!(detail.sub_issues.len(), 1);
        assert_eq!(detail.sub_issues[0].number, 2);

        let detail = db
            .edit_issue_relation("repo", 1, IssueRelationKind::BlockedBy, 3, true)
            .await
            .expect("blocked-by add");
        assert_eq!(detail.blocked_by.len(), 1);
        assert_eq!(detail.blocked_by[0].number, 3);

        let detail = db
            .edit_issue_labels("repo", 1, &["area:x".to_string()], &[])
            .await
            .expect("label add");
        assert_eq!(detail.labels, ["area:x".to_string()]);

        let detail = db
            .add_issue_comment("repo", 1, "a comment")
            .await
            .expect("comment");
        assert_eq!(detail.comments.len(), 1);
        assert_eq!(detail.comments[0].body, "a comment");

        let detail = db
            .update_issue("repo", 1, Some("renamed"), None, Some(IssueState::Closed))
            .await
            .expect("update");
        assert_eq!(detail.title, "renamed");
        assert_eq!(detail.state, IssueState::Closed);

        let open = db
            .list_issues("repo", IssueListState::Open, &[], 100)
            .await
            .expect("list open");
        assert_eq!(open.len(), 2);
        let filtered = db
            .list_issues("repo", IssueListState::All, &["area:x".to_string()], 100)
            .await
            .expect("list filtered");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].number, 1);

        let detail = db
            .edit_issue_relation("repo", 1, IssueRelationKind::SubIssue, 2, false)
            .await
            .expect("sub-issue remove");
        assert!(detail.sub_issues.is_empty());
    }

    #[tokio::test]
    async fn missing_issue_and_self_relation_fail() {
        let (_dir, db) = test_db().await;
        db.create_issue("repo", "only", "", &[])
            .await
            .expect("create");
        assert!(matches!(
            db.update_issue("repo", 9, None, None, None).await,
            Err(ChatDbError::RowNotFound(_))
        ));
        assert!(matches!(
            db.edit_issue_relation("repo", 1, IssueRelationKind::BlockedBy, 1, true)
                .await,
            Err(ChatDbError::Storage(_))
        ));
    }
}
