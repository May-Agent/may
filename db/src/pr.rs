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

/// Where a session PR stands. Stored in `prs.state` and sent over the wire
/// under the same `snake_case` names.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    skyzen::ToSchema,
    skyzen::Column,
)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    /// Opened or re-pinned; nothing has run against it yet.
    Open,
    /// Waiting on the checks of its pinned commit.
    ChecksPending,
    /// A check failed, or the merge queue dropped it; `pr merge` retries.
    Failed,
    /// The merge hit a conflict; `pr update` then `pr merge` retries.
    Rejected,
    /// Checks passed and the merge is landing.
    Merging,
    /// Landed.
    Merged,
    /// Abandoned, or closed on the remote.
    Closed,
}

impl PrState {
    /// The stored and wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::ChecksPending => "checks_pending",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Merging => "merging",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    /// Landed or abandoned: no transition leaves it.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Closed)
    }

    /// Not terminal: the session still owns work in this PR.
    #[must_use]
    pub const fn is_live(self) -> bool {
        !self.is_terminal()
    }

    /// Open, checks pending or merging: the states GitHub still reports
    /// checks on, and the ones the sidebar lists.
    #[must_use]
    pub const fn is_in_flight(self) -> bool {
        matches!(self, Self::Open | Self::ChecksPending | Self::Merging)
    }
}

impl std::fmt::Display for PrState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which PRs `pr list --state` selects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrFilter {
    /// Every PR.
    #[default]
    All,
    /// Every PR that is not merged or closed ([`PrState::is_live`]).
    Open,
    /// Exactly [`PrState::ChecksPending`].
    ChecksPending,
    /// Exactly [`PrState::Failed`].
    Failed,
    /// Exactly [`PrState::Rejected`].
    Rejected,
    /// Exactly [`PrState::Merging`].
    Merging,
    /// Exactly [`PrState::Merged`].
    Merged,
    /// Exactly [`PrState::Closed`].
    Closed,
}

impl PrFilter {
    /// Whether a PR in `state` is selected.
    #[must_use]
    pub const fn selects(self, state: PrState) -> bool {
        match self {
            Self::All => true,
            Self::Open => state.is_live(),
            Self::ChecksPending => matches!(state, PrState::ChecksPending),
            Self::Failed => matches!(state, PrState::Failed),
            Self::Rejected => matches!(state, PrState::Rejected),
            Self::Merging => matches!(state, PrState::Merging),
            Self::Merged => matches!(state, PrState::Merged),
            Self::Closed => matches!(state, PrState::Closed),
        }
    }
}

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
    /// Where the PR stands.
    pub state: PrState,
    /// Local backend: the durable ref holding the PR's commit in the checkout
    /// (`refs/may/pr/<number>`); GitHub: the session branch on `upstream`.
    pub branch: String,
    /// Base commit the PR was opened against.
    pub base_sha: String,
    /// The commit the PR owns: pinned when opened, moved only by `pr update`.
    pub head_sha: Option<String>,
    /// `may`, `github` or `gitlab`.
    pub backend: String,
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

/// Reference sigil for a PR/MR backend (`!` for GitLab merge requests, `#` otherwise).
#[must_use]
pub fn pr_sigil(backend: &str) -> char {
    if backend == "gitlab" { '!' } else { '#' }
}

impl PrRow {
    /// Reference sigil (`#` or `!`) for this PR's backend.
    #[must_use]
    pub fn sigil(&self) -> char {
        pr_sigil(&self.backend)
    }
}

/// A `prs` row joined with its repository key and whether its owning session
/// has been deleted.
#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PrSummaryRow {
    /// Surrogate id.
    pub id: i64,
    /// Owning `issue_projects` row.
    pub project_id: i64,
    /// Repository identity (`owner/repo` or local `.git` path).
    pub repo_key: String,
    /// Whether the owning session row no longer exists.
    pub orphaned: bool,
    /// Upstream PR/MR number (or per-project sequential number on the local `may` backend).
    pub number: i64,
    /// Session that opened (or last adopted) the PR.
    pub session_id: String,
    /// PR title.
    pub title: String,
    /// PR body.
    pub body: String,
    /// Where the PR stands.
    pub state: PrState,
    /// Local durable ref or remote branch.
    pub branch: String,
    /// Base commit the PR was opened against.
    pub base_sha: String,
    /// Pinned head commit.
    pub head_sha: Option<String>,
    /// `may`, `github` or `gitlab`.
    pub backend: String,
    /// Merge as soon as checks pass vs. hold for explicit merge.
    pub auto_merge: bool,
    /// Associated issue number, when linked.
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

impl PrSummaryRow {
    /// Project into the plain [`PrRow`] shape.
    #[must_use]
    pub fn into_pr_row(self) -> PrRow {
        PrRow {
            id: self.id,
            project_id: self.project_id,
            number: self.number,
            session_id: self.session_id,
            title: self.title,
            body: self.body,
            state: self.state,
            branch: self.branch,
            base_sha: self.base_sha,
            head_sha: self.head_sha,
            backend: self.backend,
            auto_merge: self.auto_merge,
            issue_number: self.issue_number,
            merge_commit_sha: self.merge_commit_sha,
            created_at: self.created_at,
            updated_at: self.updated_at,
            merged_at: self.merged_at,
        }
    }
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
    /// Backfills legacy `prs.external_number` values into `prs.number` so
    /// remote PRs carry their upstream number directly.
    pub(super) async fn migrate_pr_external_numbers(
        db: &skyzen_services::Db,
    ) -> Result<(), ChatDbError> {
        let has_external = query!(
            db,
            "SELECT 1 AS value FROM pragma_table_info('prs') WHERE name = 'external_number'"
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .is_some();
        if !has_external {
            return Ok(());
        }
        let pending = query!(
            db,
            "SELECT 1 AS value FROM prs WHERE external_number IS NOT NULL LIMIT 1"
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .is_some();
        let mut tx = db.begin().await?;
        query!(tx, "DROP INDEX IF EXISTS idx_prs_backend_external")
            .execute()
            .await?;
        if pending {
            // Park migrating rows at negative surrogate ids first so swapping
            // local sequential numbers for upstream numbers cannot hit a
            // transient UNIQUE (project_id, number) collision mid-statement.
            query!(
                tx,
                "UPDATE prs SET number = -id WHERE external_number IS NOT NULL"
            )
            .execute()
            .await?;
            query!(
                tx,
                "UPDATE prs SET number = external_number, external_number = NULL WHERE external_number IS NOT NULL"
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

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

    /// Create a PR for `repo_key`: `explicit_number` for remote backends
    /// (`Some(upstream_number)`), or numbered next within the project (`None`)
    /// for the local `may` backend.
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
        explicit_number: Option<i64>,
    ) -> Result<PrRow, ChatDbError> {
        let project_id = self.ensure_issue_project(repo_key).await?;
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        if let Some(number) = explicit_number {
            query!(
                tx,
                "INSERT INTO prs (project_id, number, session_id, title, body, state, branch, base_sha, backend, auto_merge, issue_number, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                project_id,
                number,
                session_id,
                title,
                body,
                PrState::Open.as_str(),
                branch,
                base_sha,
                backend,
                auto_merge,
                issue_number,
                now.clone(),
                now
            )
            .execute()
            .await?;
        } else {
            query!(
                tx,
                "INSERT INTO prs (project_id, number, session_id, title, body, state, branch, base_sha, backend, auto_merge, issue_number, created_at, updated_at) SELECT ?, COALESCE(MAX(number), 0) + 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? FROM prs WHERE project_id = ?",
                project_id,
                session_id,
                title,
                body,
                PrState::Open.as_str(),
                branch,
                base_sha,
                backend,
                auto_merge,
                issue_number,
                now.clone(),
                now,
                project_id
            )
            .execute()
            .await?;
        }
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

    /// The session's open PR in workspace `dir` on `repo_key`, when one exists.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn open_pr_for_workspace(
        &self,
        repo_key: &str,
        session_id: &str,
        dir: &str,
    ) -> Result<Option<PrRow>, ChatDbError> {
        let Some(project_id) = self.find_project(repo_key).await? else {
            return Ok(None);
        };
        Ok(query!(
            self.db,
            "SELECT p.* FROM prs AS p INNER JOIN pr_workspaces AS w ON w.pr_id = p.id WHERE p.project_id = ? AND p.session_id = ? AND w.dir = ?",
            project_id,
            session_id,
            dir
        )
        .fetch_all::<PrRow>()
        .await?
        .into_iter()
        .find(|row| row.state.is_live()))
    }

    /// Record the clone directory name under `<session>/repos/` bound to `pr_id`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn set_pr_workspace(&self, pr_id: i64, dir: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO pr_workspaces (pr_id, dir) VALUES (?, ?) ON CONFLICT (pr_id) DO UPDATE SET dir = excluded.dir",
            pr_id,
            dir
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The clone directory name under `<session>/repos/` bound to `pr_id`, if
    /// recorded.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn pr_workspace(&self, pr_id: i64) -> Result<Option<String>, ChatDbError> {
        #[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
        struct DirRow {
            dir: String,
        }
        let row = query!(
            self.db,
            "SELECT dir FROM pr_workspaces WHERE pr_id = ?",
            pr_id
        )
        .fetch_optional::<DirRow>()
        .await?;
        Ok(row.map(|r| r.dir))
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
            "SELECT * FROM prs WHERE project_id = ? AND session_id = ?",
            project_id,
            session_id
        )
        .fetch_all::<PrRow>()
        .await?
        .into_iter()
        .find(|row| row.state.is_live()))
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

    /// Look up one PR by its surrogate row id.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn get_pr_by_id(&self, id: i64) -> Result<Option<PrRow>, ChatDbError> {
        Ok(query!(self.db, "SELECT * FROM prs WHERE id = ?", id)
            .fetch_optional::<PrRow>()
            .await?)
    }

    /// PRs for a project or session (or across all projects), newest first,
    /// joined with their `repo_key` and orphaned status.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_prs_with_repo(
        &self,
        repo_key: Option<&str>,
        session_id: Option<&str>,
        filter: PrFilter,
        orphaned_only: bool,
    ) -> Result<Vec<PrSummaryRow>, ChatDbError> {
        let rows = match (repo_key, session_id) {
            (Some(key), _) => {
                query!(
                    self.db,
                    "SELECT p.*, ip.repo_key, (s.id IS NULL) AS orphaned \
                 FROM prs p \
                 JOIN issue_projects ip ON ip.id = p.project_id \
                 LEFT JOIN sessions s ON s.id = p.session_id \
                 WHERE ip.repo_key = ? \
                 ORDER BY p.id DESC",
                    key
                )
                .fetch_all::<PrSummaryRow>()
                .await?
            }
            (None, Some(sid)) => {
                query!(
                    self.db,
                    "SELECT p.*, ip.repo_key, (s.id IS NULL) AS orphaned \
                 FROM prs p \
                 JOIN issue_projects ip ON ip.id = p.project_id \
                 LEFT JOIN sessions s ON s.id = p.session_id \
                 WHERE p.session_id = ? \
                 ORDER BY p.id DESC",
                    sid
                )
                .fetch_all::<PrSummaryRow>()
                .await?
            }
            (None, None) => {
                query!(
                    self.db,
                    "SELECT p.*, ip.repo_key, (s.id IS NULL) AS orphaned \
                 FROM prs p \
                 JOIN issue_projects ip ON ip.id = p.project_id \
                 LEFT JOIN sessions s ON s.id = p.session_id \
                 ORDER BY p.id DESC"
                )
                .fetch_all::<PrSummaryRow>()
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .filter(|row| filter.selects(row.state) && (!orphaned_only || row.orphaned))
            .collect())
    }

    /// PRs for a project (or across all projects), newest first.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_prs(
        &self,
        repo_key: Option<&str>,
        filter: PrFilter,
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
        Ok(rows
            .into_iter()
            .filter(|row| filter.selects(row.state))
            .collect())
    }

    /// Update a PR's mutable landing fields.
    ///
    /// # Errors
    /// Fails on database errors or when the row does not exist.
    pub async fn update_pr(
        &self,
        id: i64,
        state: PrState,
        head_sha: Option<&str>,
        merge_commit_sha: Option<&str>,
        merged_at: Option<&str>,
    ) -> Result<PrRow, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE prs SET state = ?, head_sha = COALESCE(?, head_sha), merge_commit_sha = COALESCE(?, merge_commit_sha), merged_at = COALESCE(?, merged_at), updated_at = ? WHERE id = ?",
            state.as_str(),
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

    /// The PR's recorded fork point advances to `base_sha`: the base an
    /// update actually verified and pushed over, so the next `pr show`
    /// and `pr update` read the true base, not the one `pr open` pinned.
    ///
    /// # Errors
    /// Fails on a database error.
    pub async fn update_pr_base(&self, id: i64, base_sha: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE prs SET base_sha = ?, updated_at = ? WHERE id = ?",
            base_sha,
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Close a PR: the only transition to `closed` that abandons work, and
    /// the one place that refuses a terminal PR. The state check and the
    /// write share one transaction.
    ///
    /// # Errors
    /// Fails with [`ChatDbError::PrTerminal`] when the PR is `merged` or
    /// `closed`, and on database errors.
    pub async fn close_pr(&self, id: i64) -> Result<PrRow, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        let row = query!(tx, "SELECT * FROM prs WHERE id = ?", id)
            .fetch_one::<PrRow>()
            .await?;
        if row.state.is_terminal() {
            return Err(ChatDbError::PrTerminal { state: row.state });
        }
        query!(
            tx,
            "UPDATE prs SET state = ?, updated_at = ? WHERE id = ?",
            PrState::Closed.as_str(),
            now,
            id
        )
        .execute()
        .await?;
        let row = query!(tx, "SELECT * FROM prs WHERE id = ?", id)
            .fetch_one::<PrRow>()
            .await?;
        tx.commit().await?;
        Ok(row)
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

#[cfg(test)]
mod tests {
    use super::{PrFilter, PrState};
    use crate::{ChatDb, query};

    const STORED: [(&str, PrState); 7] = [
        ("open", PrState::Open),
        ("checks_pending", PrState::ChecksPending),
        ("failed", PrState::Failed),
        ("rejected", PrState::Rejected),
        ("merging", PrState::Merging),
        ("merged", PrState::Merged),
        ("closed", PrState::Closed),
    ];

    #[tokio::test]
    async fn a_row_written_with_each_stored_name_reads_as_its_variant() {
        let dir = tempfile::tempdir().unwrap();
        let db = ChatDb::open(&dir.path().join("history.db")).await.unwrap();
        let pr = db
            .create_pr(
                "github:o/r",
                "s",
                "t",
                "",
                "b",
                "base",
                "github",
                true,
                None,
                Some(42),
            )
            .await
            .unwrap();
        assert_eq!(pr.number, 42);
        for (name, state) in STORED {
            query!(db.db, "UPDATE prs SET state = ? WHERE id = ?", name, pr.id)
                .execute()
                .await
                .unwrap();
            let row = db.get_pr("github:o/r", pr.number).await.unwrap().unwrap();
            assert_eq!(row.state, state, "{name}");
            // Written through the typed path, the column holds the same name.
            db.update_pr(pr.id, state, None, None, None).await.unwrap();
            let stored: String =
                query!(db.db, "SELECT state AS value FROM prs WHERE id = ?", pr.id)
                    .fetch_one::<crate::common::StringValueRow>()
                    .await
                    .unwrap()
                    .value;
            assert_eq!(stored, name);
        }
    }

    #[test]
    fn each_variant_serializes_as_its_stored_name() {
        for (name, state) in STORED {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(
                serde_json::from_str::<PrState>(&format!("\"{name}\"")).unwrap(),
                state
            );
            assert_eq!(state.as_str(), name);
        }
    }

    #[test]
    fn predicates_and_filters_partition_the_states() {
        for (_, state) in STORED {
            assert_eq!(state.is_live(), !state.is_terminal());
            assert!(PrFilter::All.selects(state));
            assert_eq!(PrFilter::Open.selects(state), state.is_live());
        }
        assert!(PrState::Merging.is_in_flight() && !PrState::Failed.is_in_flight());
        assert!(
            PrFilter::Merged.selects(PrState::Merged) && !PrFilter::Merged.selects(PrState::Closed)
        );
    }

    #[tokio::test]
    async fn migrates_legacy_external_numbers_without_unique_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let raw = ChatDb::connect(&path).await.unwrap();
        query!(
            raw,
            "CREATE TABLE issue_projects (id INTEGER PRIMARY KEY AUTOINCREMENT, repo_key TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL)"
        )
        .execute()
        .await
        .unwrap();
        query!(
            raw,
            "INSERT INTO issue_projects (id, repo_key, created_at) VALUES (1, 'github:o/r', '2026-01-01T00:00:00Z'), (2, '/tmp/local/.git', '2026-01-01T00:00:00Z')"
        )
        .execute()
        .await
        .unwrap();
        query!(
            raw,
            "CREATE TABLE prs (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, number INTEGER NOT NULL, session_id TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '', state TEXT NOT NULL, branch TEXT NOT NULL, base_sha TEXT NOT NULL, head_sha TEXT, backend TEXT NOT NULL, external_number INTEGER, auto_merge BOOLEAN NOT NULL DEFAULT 1, issue_number INTEGER, merge_commit_sha TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, merged_at TEXT, UNIQUE (project_id, number))"
        )
        .execute()
        .await
        .unwrap();
        query!(
            raw,
            "CREATE INDEX idx_prs_backend_external ON prs(backend, external_number) WHERE external_number IS NOT NULL"
        )
        .execute()
        .await
        .unwrap();
        // Row 1 had local number 1 and upstream #2, while row 2 already held local number 2 and upstream #765.
        query!(
            raw,
            "INSERT INTO prs (id, project_id, number, session_id, title, body, state, branch, base_sha, backend, external_number, auto_merge, created_at, updated_at) VALUES \
             (1, 1, 1, 's1', 'First', '', 'open', 'may/s1', 'a', 'github', 2, 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'), \
             (2, 1, 2, 's2', 'Second', '', 'merged', 'may/s2', 'b', 'github', 765, 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'), \
             (3, 2, 1, 's3', 'Local', '', 'open', 'refs/may/pr/1', 'c', 'may', NULL, 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
        )
        .execute()
        .await
        .unwrap();
        drop(raw);

        let db = ChatDb::open(&path).await.unwrap();
        let first = db.get_pr("github:o/r", 2).await.unwrap().unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(first.title, "First");
        let second = db.get_pr("github:o/r", 765).await.unwrap().unwrap();
        assert_eq!(second.id, 2);
        assert_eq!(second.title, "Second");
        assert!(db.get_pr("github:o/r", 1).await.unwrap().is_none());
        let local = db.get_pr("/tmp/local/.git", 1).await.unwrap().unwrap();
        assert_eq!(local.id, 3);

        // Reopening is idempotent.
        drop(db);
        let db2 = ChatDb::open(&path).await.unwrap();
        assert_eq!(db2.get_pr("github:o/r", 765).await.unwrap().unwrap().id, 2);
    }
}
