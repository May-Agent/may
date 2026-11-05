//! PR rows and event history for the may PR store on [`ChatDb`].
//!
//! PRs are scoped to an `issue_projects` row keyed by repository identity —
//! the same project an issue lives under — and numbered per project, so a
//! repository's `pr #3` is unambiguous across sessions and worktrees.
//! Every transition lands a `pr_events` row carrying the session, the agent
//! role, and UTC time, so any merged PR traces back to the conversation
//! that made it.

use super::common::I64ValueRow;
use super::{ChatDb, ChatDbError, query};

/// One `prs` row.
#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PrRow {
    /// Surrogate id.
    pub id: i64,
    /// Owning `issue_projects` row.
    pub project_id: i64,
    /// Per-project number, like an issue number.
    pub number: i64,
    /// Session that opened the PR — the only session allowed to write it.
    pub session_id: String,
    /// PR title.
    pub title: String,
    /// PR body.
    pub body: String,
    /// `open`, `checks_pending`, `failed`, `rejected`, `merging`, `merged`
    /// or `closed`.
    pub state: String,
    /// Local backend: the durable ref holding the PR's commit in the checkout
    /// (`refs/may/pr/<number>`); GitHub: the session branch on `upstream`.
    pub branch: String,
    /// Base commit the PR was opened against.
    pub base_sha: String,
    /// The commit the PR owns: pinned when opened, moved only by `pr update`.
    pub head_sha: Option<String>,
    /// `may` or `github`.
    pub backend: String,
    /// External PR number (GitHub), when the backend created one.
    pub external_number: Option<i64>,
    /// Merge as soon as checks pass (default) vs. hold for `pr merge`.
    pub auto_merge: bool,
    /// Associated issue number, when the PR was opened with `--issue`.
    pub issue_number: Option<i64>,
    /// Commit that merged the PR.
    pub merge_commit_sha: Option<String>,
    /// UTC creation time.
    pub created_at: String,
    /// UTC of the last update.
    pub updated_at: String,
    /// UTC of the merge.
    pub merged_at: Option<String>,
}

/// One `pr_events` row.
#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PrEventRow {
    /// Surrogate id.
    pub id: i64,
    /// Owning `prs` row.
    pub pr_id: i64,
    /// `opened`, `checks_pending`, `check_failed`, `rejected`, `queued`,
    /// `merging`, `merged`, `closed`, `issue_linked`, `rewound`,
    /// `orphaned`, `adopted`, `updated`.
    pub kind: String,
    /// Session that caused the transition.
    pub session_id: String,
    /// `main`, `user` or `scheduler`.
    pub agent_id: String,
    /// History row of the tool call, when the caller's context could
    /// resolve one — IPC commands do not see it yet.
    pub message_id: Option<i64>,
    /// Event payload: conflict files, failure excerpt, merge sha.
    pub detail: Option<String>,
    /// UTC time of the event.
    pub created_at: String,
}

impl ChatDb {
    async fn find_project(&self, repo_key: &str) -> Result<Option<i64>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT id AS value FROM issue_projects WHERE repo_key = ?",
            repo_key
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .map(|row| row.value))
    }

    /// Create a PR for `repo_key`, numbered next within the project.
    ///
    /// # Errors
    /// Fails on database errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pr(
        &self,
        repo_key: &str,
        session_id: &str,
        title: &str,
        body: &str,
        branch: &str,
        base_sha: &str,
        backend: &str,
        auto_merge: bool,
        issue_number: Option<i64>,
        external_number: Option<i64>,
    ) -> Result<PrRow, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO prs (project_id, number, session_id, title, body, state, branch, base_sha, backend, external_number, auto_merge, issue_number, created_at, updated_at) SELECT ?, COALESCE(MAX(number), 0) + 1, ?, ?, ?, 'open', ?, ?, ?, ?, ?, ?, ?, ? FROM prs WHERE project_id = ?",
            project_id,
            session_id,
            title,
            body,
            branch,
            base_sha,
            backend,
            external_number,
            auto_merge,
            issue_number,
            now.clone(),
            now,
            project_id
        )
        .execute()
        .await?;
        let id = query!(tx, "SELECT last_insert_rowid() AS value")
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        let row = query!(tx, "SELECT * FROM prs WHERE id = ?", id)
            .fetch_one::<PrRow>()
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    /// One PR by its per-project number.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn get_pr(&self, repo_key: &str, number: i64) -> Result<Option<PrRow>, ChatDbError> {
        let Some(project_id) = self.find_project(repo_key).await? else {
            return Ok(None);
        };
        Ok(query!(
            self.db,
            "SELECT * FROM prs WHERE project_id = ? AND number = ?",
            project_id,
            number
        )
        .fetch_optional::<PrRow>()
        .await?)
    }

    /// The session's open PR on a project, when one exists.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn open_pr_for(
        &self,
        repo_key: &str,
        session_id: &str,
    ) -> Result<Option<PrRow>, ChatDbError> {
        let Some(project_id) = self.find_project(repo_key).await? else {
            return Ok(None);
        };
        Ok(query!(
            self.db,
            "SELECT * FROM prs WHERE project_id = ? AND session_id = ? AND state IN ('open','checks_pending','failed','rejected','merging')",
            project_id,
            session_id
        )
        .fetch_optional::<PrRow>()
        .await?)
    }

    /// Name the durable ref a local PR's commit lives under.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn set_pr_branch(&self, pr_id: i64, branch: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE prs SET branch = ? WHERE id = ?",
            branch,
            pr_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Transfer a PR to another session — the manual orphan-recovery op.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn adopt_pr(&self, pr_id: i64, session_id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE prs SET session_id = ?, updated_at = ? WHERE id = ?",
            session_id,
            now,
            pr_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// PRs for a project (or across all projects), newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_prs(
        &self,
        repo_key: Option<&str>,
        state: Option<&str>,
    ) -> Result<Vec<PrRow>, ChatDbError> {
        let rows = if let Some(key) = repo_key {
            query!(
                self.db,
                "SELECT p.* FROM prs p JOIN issue_projects ip ON ip.id = p.project_id AND ip.repo_key = ? ORDER BY p.id DESC",
                key
            )
            .fetch_all::<PrRow>()
            .await?
        } else {
            query!(self.db, "SELECT * FROM prs p ORDER BY p.id DESC")
                .fetch_all::<PrRow>()
                .await?
        };
        let state = state.unwrap_or("all");
        Ok(rows
            .into_iter()
            .filter(|row| match state {
                "open" => matches!(
                    row.state.as_str(),
                    "open" | "checks_pending" | "failed" | "rejected" | "merging"
                ),
                "all" => true,
                other => row.state == other,
            })
            .collect())
    }

    /// Update a PR's mutable landing fields.
    ///
    /// # Errors
    /// Fails on database errors or when the row does not exist.
    pub async fn update_pr(
        &self,
        id: i64,
        state: &str,
        head_sha: Option<&str>,
        merge_commit_sha: Option<&str>,
        merged_at: Option<&str>,
    ) -> Result<PrRow, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE prs SET state = ?, head_sha = COALESCE(?, head_sha), merge_commit_sha = COALESCE(?, merge_commit_sha), merged_at = COALESCE(?, merged_at), updated_at = ? WHERE id = ?",
            state,
            head_sha,
            merge_commit_sha,
            merged_at,
            now,
            id
        )
        .execute()
        .await?;
        Ok(query!(self.db, "SELECT * FROM prs WHERE id = ?", id)
            .fetch_one::<PrRow>()
            .await?)
    }

    /// Append a provenance event to a PR.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn add_pr_event(
        &self,
        pr_id: i64,
        kind: &str,
        session_id: &str,
        agent_id: &str,
        message_id: Option<i64>,
        detail: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO pr_events (pr_id, kind, session_id, agent_id, message_id, detail, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            pr_id,
            kind,
            session_id,
            agent_id,
            message_id,
            detail,
            now
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Every PR a session opened, newest first — the session sidebar's PR list.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on query failure.
    pub async fn list_session_prs(&self, session_id: &str) -> Result<Vec<PrRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM prs WHERE session_id = ? ORDER BY updated_at DESC",
            session_id
        )
        .fetch_all::<PrRow>()
        .await?)
    }

    /// A PR's event history, oldest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn pr_events(&self, pr_id: i64) -> Result<Vec<PrEventRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM pr_events WHERE pr_id = ? ORDER BY id",
            pr_id
        )
        .fetch_all::<PrEventRow>()
        .await?)
    }
}
