//! Chat history database backed by SQLite.
//!
//! Single global database at `~/.may/history.db` containing all sessions and messages.

mod cache_misses;
mod common;
mod context_snapshot;
pub mod entity;
mod issue;
mod issue_provenance;
mod issue_search;
mod logs;
mod memory;
mod message;
mod notification;
mod open_questions;
mod pr;
mod profile;
mod repo_clone;
mod repo_usage;
mod search_outbox;
mod session;
mod session_intervals;
mod session_run;
mod sleep_log;
mod sql;
mod task;

use std::path::Path;

use skyzen_services::Db as SkyDb;
use skyzen_services::sql::sqlite::{SqliteConnectOptions, SqliteJournalMode};

use crate::PermissionHistoryMode;
use sql as db_sql;

pub use cache_misses::PromptCacheRequest;
pub use common::{
    AssistantCacheUsageSummary, ChatDbError, ContextDebugSnapshot, ContextDebugSnapshotMessage,
    CreateProfileParams, CreateScheduledTaskParams, MessageStatsParams, NetworkAccessLogParams,
    NetworkAccessMode, NetworkAccessSource, NotificationCallbackConsumeResult,
    PendingWorkspaceLookup, ProfileEnvironmentKind, RequestLogParams, SessionContextSnapshotUpsert,
    StoredSessionContextSnapshot, TranscriptEntry, UpdateProfileParams, UpdateScheduledTaskParams,
};
pub use context_snapshot::ContextDebugSnapshotWrite;
pub use issue_provenance::{IssueProvenanceRecord, NewIssueProvenance, UserMessageAt};
pub use issue_search::IssueSearchDoc;
pub use message::MessageMeta;
pub use open_questions::{CommittedAskUserAnswer, PendingAskUserQuestion};
pub use pr::{PrEventRow, PrRow};
pub use repo_clone::RepoCloneRow;
pub use repo_usage::RepoUsage;
pub use search_outbox::{IndexedMessageRow, SearchOutboxEntry};
pub use session_intervals::SessionIntervals;
pub use session_run::RunFence;

use common::{
    AskUserRequestMessageRow, AskUserRequestQuestionOptionRow, AskUserRequestQuestionRow,
    I64ValueRow, PermissionRequestMessageRow, SnapshotMigrationRow, SqliteTableInfoRow,
    db_custom_error, overflow_error, row_not_found, storage_error, to_i32, to_i64_from_u64,
};

macro_rules! query {
    ($db:expr, $sql:expr $(, $param:expr)* $(,)?) => {{
        $db.query($sql)$(.bind($param))*
    }};
}
pub(super) use query;

/// Chat history database.
///
#[derive(Debug, Clone)]
pub struct ChatDb {
    pub(super) db: SkyDb,
    /// Test seam: suspends a run claim or finish at its await.
    #[cfg(test)]
    pub(crate) run_gates: session_run::RunGates,
}

impl ChatDb {
    /// Connect to the history database file, creating it when missing. The
    /// file is in WAL mode, so [`SkyDb`] serves reads on read-only connections
    /// while its one writer connection queues writes.
    pub(super) async fn connect(db_path: &Path) -> Result<SkyDb, ChatDbError> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        Ok(SkyDb::connect_sqlite_with(options).await?)
    }

    /// Open (or create) the chat history database at the given path.
    ///
    /// Creates the current schema if the tables don't exist.
    ///
    /// # Errors
    ///
    /// Returns [`ChatDbError`] when connecting to the SQLite database or creating
    /// the schema fails.
    pub async fn open(db_path: &Path) -> Result<Self, ChatDbError> {
        let db = Self::connect(db_path).await?;
        Self::initialize_schema(&db).await?;
        Self::ensure_profile_preference_columns(&db).await?;
        Self::ensure_session_columns(&db).await?;
        Self::ensure_message_columns(&db).await?;
        Self::ensure_ask_user_blocking_column(&db).await?;
        Self::ensure_ask_user_secret_columns(&db).await?;
        Self::ensure_pending_permission_agent_id_column(&db).await?;
        Self::ensure_scheduled_task_columns(&db).await?;
        Self::ensure_session_context_snapshot_table(&db).await?;
        Self::ensure_pending_message_claim_columns(&db).await?;
        Self::ensure_session_run_layout(&db).await?;
        Self::drop_legacy_pending_tables(&db).await?;
        Self::migrate_legacy_remote_profiles(&db).await?;
        Self::migrate_removed_waiting_terminal_input(&db).await?;
        Self::ensure_search_outbox(&db).await?;
        Self::ensure_session_interval_triggers(&db).await?;
        Ok(Self {
            db,
            #[cfg(test)]
            run_gates: session_run::RunGates::default(),
        })
    }

    /// Run `sql` as given, so a test can break the storage a commit needs.
    #[cfg(test)]
    pub(crate) async fn execute_for_test(&self, sql: &str) -> Result<(), ChatDbError> {
        query!(self.db, sql).execute().await?;
        Ok(())
    }

    async fn ensure_session_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(db, "PRAGMA table_info(sessions)")
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        let existing = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        if !existing.contains("reasoning_effort") {
            query!(db, "ALTER TABLE sessions ADD COLUMN reasoning_effort TEXT")
                .execute()
                .await?;
        }
        if !existing.contains("cwd") {
            query!(db, "ALTER TABLE sessions ADD COLUMN cwd TEXT")
                .execute()
                .await?;
        }
        if !existing.contains("repo") {
            query!(db, "ALTER TABLE sessions ADD COLUMN repo TEXT")
                .execute()
                .await?;
        }
        if !existing.contains("non_interactive") {
            query!(
                db,
                "ALTER TABLE sessions ADD COLUMN non_interactive BOOLEAN NOT NULL DEFAULT 0"
            )
            .execute()
            .await?;
        }
        if !existing.contains("rules_import") {
            query!(db, "ALTER TABLE sessions ADD COLUMN rules_import TEXT")
                .execute()
                .await?;
        }
        Ok(())
    }

    async fn ensure_ask_user_blocking_column(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(
            db,
            "PRAGMA table_info(session_pending_ask_user_requests_v2)"
        )
        .fetch_all::<SqliteTableInfoRow>()
        .await?;
        if !columns.iter().any(|column| column.name == "blocking") {
            // Rows written before the column existed are the prompts a run
            // was waiting on, which is what `blocking` marks.
            query!(
                db,
                "ALTER TABLE session_pending_ask_user_requests_v2 ADD COLUMN blocking BOOLEAN NOT NULL DEFAULT 1"
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    async fn ensure_ask_user_secret_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(
            db,
            "PRAGMA table_info(session_pending_ask_user_questions_v2)"
        )
        .fetch_all::<SqliteTableInfoRow>()
        .await?;
        for column in ["secret_name", "secret_scope"] {
            if !columns.iter().any(|existing| existing.name == column) {
                let sql = format!(
                    "ALTER TABLE session_pending_ask_user_questions_v2 ADD COLUMN {column} TEXT"
                );
                query!(db, sql.as_str()).execute().await?;
            }
        }
        Ok(())
    }

    async fn ensure_pending_permission_agent_id_column(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(
            db,
            "SELECT name FROM pragma_table_info('session_pending_permissions')"
        )
        .fetch_all::<SqliteTableInfoRow>()
        .await?;
        if !columns.iter().any(|column| column.name == "agent_id") {
            query!(
                db,
                "ALTER TABLE session_pending_permissions ADD COLUMN agent_id TEXT"
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    async fn ensure_scheduled_task_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(db, "PRAGMA table_info(scheduled_tasks)")
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        if !columns.iter().any(|column| column.name == "timeout_secs") {
            // SQLite needs a default to add a NOT NULL column; it only fills
            // rows created before the column existed, and operators change it
            // per task. New tasks always state their own timeout.
            query!(
                db,
                "ALTER TABLE scheduled_tasks ADD COLUMN timeout_secs INTEGER NOT NULL DEFAULT 600"
            )
            .execute()
            .await?;
        }
        for column in ["webhook_signature_secret", "webhook_signature_header"] {
            if !columns.iter().any(|existing| existing.name == column) {
                let sql = format!("ALTER TABLE scheduled_tasks ADD COLUMN {column} TEXT");
                query!(db, sql.as_str()).execute().await?;
            }
        }
        Self::remint_weak_webhook_tokens(db).await
    }

    /// Webhook tokens minted before they were 256-bit random secrets are
    /// replaced, so no weak token is accepted; the new path shows in the task.
    async fn remint_weak_webhook_tokens(db: &SkyDb) -> Result<(), ChatDbError> {
        let tasks = query!(
            db,
            "SELECT * FROM scheduled_tasks WHERE trigger_kind = ?",
            crate::task_trigger::TRIGGER_KIND_WEBHOOK
        )
        .fetch_all::<entity::scheduled_tasks::Model>()
        .await?;
        for task in tasks {
            if crate::task_trigger::is_strong_webhook_token(&task.trigger_type) {
                continue;
            }
            let token = crate::task_trigger::new_webhook_token();
            query!(
                db,
                "UPDATE scheduled_tasks SET trigger_type = ? WHERE id = ?",
                token,
                task.id.clone()
            )
            .execute()
            .await?;
            tracing::warn!(
                task_id = %task.id,
                "re-minted a weak webhook token; the task's webhook path changed"
            );
        }
        Ok(())
    }

    async fn ensure_message_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(db, "PRAGMA table_info(messages)")
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        let existing = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        for column in [
            "attachments",
            "reasoning",
            "tool_calls",
            "translation",
            "thinking",
        ] {
            if !existing.contains(column) {
                let sql = format!("ALTER TABLE messages ADD COLUMN {column} TEXT");
                query!(db, sql.as_str()).execute().await?;
            }
        }
        Ok(())
    }

    /// Idempotent data migration: any row with `environment='remote'` is
    /// re-homed to `'local'` and, if it referenced an `ssh_server`, the
    /// server name is inserted into the new `profile_remote_servers` junction
    /// table so the attachment is preserved.
    async fn migrate_legacy_remote_profiles(db: &SkyDb) -> Result<(), ChatDbError> {
        let legacy = query!(
            db,
            "SELECT id, remote_ssh_server_id FROM profiles WHERE environment = 'remote'"
        )
        .fetch_all::<LegacyRemoteProfileRow>()
        .await?;
        if legacy.is_empty() {
            return Ok(());
        }
        let count = legacy.len();
        let now = jiff::Timestamp::now().to_string();
        for row in legacy {
            if let Some(server_name) = row
                .remote_ssh_server_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                query!(
                    db,
                    "INSERT OR IGNORE INTO profile_remote_servers (profile_id, ssh_server_name, sort_order, created_at) VALUES (?, ?, 0, ?)",
                    row.id.as_str(),
                    server_name,
                    now.as_str()
                )
                .execute()
                .await?;
            }
            query!(
                db,
                "UPDATE profiles SET environment = 'local', remote_ssh_server_id = NULL WHERE id = ?",
                row.id.as_str()
            )
            .execute()
            .await?;
        }
        tracing::info!(
            count,
            "migrated legacy remote profiles to local home + attached SSH servers",
        );
        Ok(())
    }

    /// Idempotent data migration: `waiting_terminal_input` was removed as a
    /// session runtime state. A session stored in it has no running turn, so
    /// it loads as idle with its terminal metadata cleared, as restart
    /// recovery treats any session whose turn is not running.
    async fn migrate_removed_waiting_terminal_input(db: &SkyDb) -> Result<(), ChatDbError> {
        query!(
            db,
            "UPDATE session_runtimes SET state = 'idle', terminal_task_id = NULL, terminal_output_file = NULL, terminal_output_preview = NULL, terminal_input_notice = NULL, terminal_background_reason = NULL WHERE state = 'waiting_terminal_input'"
        )
        .execute()
        .await?;
        Ok(())
    }

    async fn initialize_schema(db: &SkyDb) -> Result<(), ChatDbError> {
        for statement in db_sql::statements(db_sql::CHAT_SCHEMA) {
            query!(db, statement).execute().await?;
        }
        Ok(())
    }

    async fn ensure_profile_preference_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(db, "PRAGMA table_info(profiles)")
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        let existing = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        if !existing.contains("remote_ssh_server_id") {
            query!(
                db,
                "ALTER TABLE profiles ADD COLUMN remote_ssh_server_id TEXT"
            )
            .execute()
            .await?;
        }
        // Custom container images were retired with the published workspace
        // image; every profile now uses it.
        if existing.contains("container_image") {
            query!(db, "ALTER TABLE profiles DROP COLUMN container_image")
                .execute()
                .await?;
        }
        if existing.contains("container_dockerfile") {
            query!(db, "ALTER TABLE profiles DROP COLUMN container_dockerfile")
                .execute()
                .await?;
        }
        Ok(())
    }

    /// Drop the retired `session_run_leases` table and give
    /// `session_runtimes` the `run_id` fencing column. Run ownership lived
    /// there before it moved into the daemon's memory.
    /// Pending messages gained a claim lifecycle: `state`
    /// (`pending`/`claimed`/`committed`/`failed`), `claimed_run_id` fencing
    /// the claim to a run, and `claim_error` recording a `failed` entry.
    async fn ensure_pending_message_claim_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(
            db,
            "SELECT name FROM pragma_table_info('session_pending_message_entries')"
        )
        .fetch_all::<SqliteTableInfoRow>()
        .await?;
        if !columns.iter().any(|column| column.name == "state") {
            query!(
                db,
                "ALTER TABLE session_pending_message_entries ADD COLUMN state TEXT NOT NULL DEFAULT 'pending'"
            )
            .execute()
            .await?;
        }
        if !columns.iter().any(|column| column.name == "claimed_run_id") {
            query!(
                db,
                "ALTER TABLE session_pending_message_entries ADD COLUMN claimed_run_id TEXT"
            )
            .execute()
            .await?;
        }
        if !columns.iter().any(|column| column.name == "claim_error") {
            query!(
                db,
                "ALTER TABLE session_pending_message_entries ADD COLUMN claim_error TEXT"
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    /// Tables an older build created and the current one never reads. Their
    /// foreign keys onto `sessions` block deleting a session, and their rows
    /// were never carried into the tables that replaced them
    /// (`session_pending_message_entries`, `session_pending_ask_user_*_v2`).
    async fn drop_legacy_pending_tables(db: &SkyDb) -> Result<(), ChatDbError> {
        for statement in db_sql::statements(db_sql::DROP_LEGACY_PENDING_TABLES) {
            query!(db, statement).execute().await?;
        }
        Ok(())
    }

    async fn ensure_session_run_layout(db: &SkyDb) -> Result<(), ChatDbError> {
        query!(db, "DROP TABLE IF EXISTS session_run_leases")
            .execute()
            .await?;
        let columns = query!(db, "SELECT name FROM pragma_table_info('session_runtimes')")
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        if !columns.iter().any(|column| column.name == "run_id") {
            query!(db, "ALTER TABLE session_runtimes ADD COLUMN run_id TEXT")
                .execute()
                .await?;
        }
        Ok(())
    }

    async fn ensure_session_context_snapshot_table(db: &SkyDb) -> Result<(), ChatDbError> {
        // The pre-versioning layout: it gains `format_version` defaulting to
        // 1 ("every blob written before versioning existed") so its rows are
        // then invalidated below.
        const EXPECTED_COLUMNS: [&str; 5] = [
            "session_id",
            "checkpoint_blob",
            "applied_seq",
            "updated_at",
            "format_version",
        ];
        let columns = query!(db, db_sql::SESSION_CONTEXT_SNAPSHOT_COLUMNS)
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        let actual_columns = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>();

        let has_message_anchor = actual_columns.contains(&"last_message_db_id");
        if has_message_anchor {
            // Pre-#10 layout: keep every snapshot. `applied_seq` and the
            // transcript entries are recovered from the blobs themselves —
            // aither assigns item seqs, so they are the anchor's true values.
            if !actual_columns.contains(&"format_version") {
                query!(
                    db,
                    "ALTER TABLE session_context_snapshots ADD COLUMN format_version INTEGER NOT NULL DEFAULT 1"
                )
                .execute()
                .await?;
            }
            let mut tx = db.begin().await?;
            query!(
                tx,
                "ALTER TABLE session_context_snapshots ADD COLUMN applied_seq INTEGER NOT NULL DEFAULT 0"
            )
            .execute()
            .await?;
            let rows = query!(
                tx,
                "SELECT session_id, checkpoint_blob FROM session_context_snapshots"
            )
            .fetch_all::<SnapshotMigrationRow>()
            .await?;
            for row in rows {
                let checkpoint: aither_agent::AgentCheckpoint =
                    rmp_serde::from_slice(&row.checkpoint_blob).map_err(|e| {
                        storage_error(format!(
                            "checkpoint blob for {} undecodable during snapshot migration: {e}",
                            row.session_id
                        ))
                    })?;
                let applied_seq = crate::session::checkpoint_watermark(&checkpoint.context);
                for item in checkpoint.context.items() {
                    let blob = rmp_serde::to_vec_named(item.message()).map_err(|e| {
                        storage_error(format!("transcript message encode failed: {e}"))
                    })?;
                    let seq = i64::try_from(item.seq())
                        .map_err(|e| storage_error(format!("transcript seq overflow: {e}")))?;
                    let committed_at = item.committed_at().to_string();
                    query!(
                        tx,
                        "INSERT OR IGNORE INTO session_transcript_entries (session_id, seq, message_blob, committed_at) VALUES (?, ?, ?, ?)",
                        row.session_id.as_str(),
                        seq,
                        blob,
                        committed_at
                    )
                    .execute()
                    .await?;
                }
                query!(
                    tx,
                    "UPDATE session_context_snapshots SET applied_seq = ? WHERE session_id = ?",
                    applied_seq,
                    row.session_id.as_str()
                )
                .execute()
                .await?;
            }
            query!(
                tx,
                "ALTER TABLE session_context_snapshots DROP COLUMN last_message_db_id"
            )
            .execute()
            .await?;
            tx.commit().await?;
        }
        if actual_columns.is_empty() {
            for statement in db_sql::statements(db_sql::SESSION_CONTEXT_SNAPSHOT_SCHEMA) {
                query!(db, statement).execute().await?;
            }
        } else if !has_message_anchor && actual_columns != EXPECTED_COLUMNS {
            tracing::warn!(
                columns = ?actual_columns,
                "session_context_snapshots has an unexpected column layout; dropping and recreating it"
            );
            query!(db, db_sql::SESSION_CONTEXT_SNAPSHOT_DROP)
                .execute()
                .await?;
            for statement in db_sql::statements(db_sql::SESSION_CONTEXT_SNAPSHOT_SCHEMA) {
                query!(db, statement).execute().await?;
            }
        }

        // Invalidate every snapshot whose blob was written under a different
        // `AgentCheckpoint` shape: `deny_unknown_fields` makes it undecodable.
        // Runs on every open so a bumped `CONTEXT_SNAPSHOT_FORMAT` also
        // invalidates rows whose table layout did not change.
        let stale_versions = query!(
            db,
            "SELECT DISTINCT format_version AS value FROM session_context_snapshots WHERE format_version != ?",
            context_snapshot::CONTEXT_SNAPSHOT_FORMAT
        )
        .fetch_all::<I64ValueRow>()
        .await?;
        if !stale_versions.is_empty() {
            let deleted = query!(
                db,
                "DELETE FROM session_context_snapshots WHERE format_version != ?",
                context_snapshot::CONTEXT_SNAPSHOT_FORMAT
            )
            .execute()
            .await?;
            tracing::info!(
                invalidated = deleted.rows_written,
                previous_formats = ?stale_versions.iter().map(|row| row.value).collect::<Vec<_>>(),
                current_format = context_snapshot::CONTEXT_SNAPSHOT_FORMAT,
                "invalidated session context snapshots written under an older blob format"
            );
        }

        Ok(())
    }

    // ── Permission / AskUser Queries ─────────────────────────────

    /// Find unanswered permission requests in a session.
    ///
    /// # Errors
    ///
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn pending_permission_requests(
        &self,
        session_id: &str,
    ) -> Result<Vec<PermissionRequestPayload>, ChatDbError> {
        query!(
            self.db,
            // `mode != 'network'` excludes rows persisted before the network
            // escalation mode was removed; they can no longer be answered.
            "SELECT request.request_id AS request_id, request.mode AS mode, request.script AS script FROM messages AS message INNER JOIN message_permission_requests AS request ON request.message_id = message.id WHERE message.session_id = ? AND request.mode != 'network' AND NOT EXISTS (SELECT 1 FROM message_permission_responses AS response INNER JOIN messages AS response_message ON response_message.id = response.message_id WHERE response.request_id = request.request_id AND response_message.session_id = message.session_id) ORDER BY message.id ASC",
            session_id
        )
        .fetch_all::<PermissionRequestMessageRow>()
        .await?
        .into_iter()
        .map(|row| {
            Ok(PermissionRequestPayload {
                id: row.request_id,
                mode: row.mode.parse().map_err(|error| {
                    storage_error(db_custom_error("invalid permission history mode: ", &error))
                })?,
                script: row.script,
            })
        })
        .collect()
    }

    /// Find unanswered ask-user requests in a session.
    ///
    /// # Errors
    ///
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn pending_ask_user_requests(
        &self,
        session_id: &str,
    ) -> Result<Vec<AskUserRequestPayload>, ChatDbError> {
        let request_rows = query!(
            self.db,
            "SELECT request.message_id AS message_id, request.request_id AS request_id FROM messages AS message INNER JOIN message_ask_user_requests AS request ON request.message_id = message.id WHERE message.session_id = ? AND NOT EXISTS (SELECT 1 FROM message_ask_user_responses AS response INNER JOIN messages AS response_message ON response_message.id = response.message_id WHERE response.request_id = request.request_id AND response_message.session_id = message.session_id) ORDER BY message.id ASC",
            session_id
        )
        .fetch_all::<AskUserRequestMessageRow>()
        .await?;
        let question_rows = query!(
            self.db,
            "SELECT request.message_id AS message_id, question.ordinal AS ordinal, question.section AS section, question.question AS question, question.multi_select AS multi_select FROM messages AS message INNER JOIN message_ask_user_requests AS request ON request.message_id = message.id INNER JOIN message_ask_user_request_questions AS question ON question.message_id = request.message_id WHERE message.session_id = ? ORDER BY request.message_id ASC, question.ordinal ASC",
            session_id
        )
        .fetch_all::<AskUserRequestQuestionRow>()
        .await?;
        let option_rows = query!(
            self.db,
            "SELECT request.message_id AS message_id, option.question_ordinal AS question_ordinal, option.option_ordinal AS option_ordinal, option.value AS value FROM messages AS message INNER JOIN message_ask_user_requests AS request ON request.message_id = message.id INNER JOIN message_ask_user_request_options AS option ON option.message_id = request.message_id WHERE message.session_id = ? ORDER BY request.message_id ASC, option.question_ordinal ASC, option.option_ordinal ASC",
            session_id
        )
        .fetch_all::<AskUserRequestQuestionOptionRow>()
        .await?;
        let mut questions_by_message = question_rows.into_iter().fold(
            std::collections::BTreeMap::<i32, Vec<AskUserRequestQuestionRow>>::new(),
            |mut grouped, row| {
                grouped.entry(row.message_id).or_default().push(row);
                grouped
            },
        );
        let mut options_by_message = option_rows.into_iter().fold(
            std::collections::BTreeMap::<i32, Vec<AskUserRequestQuestionOptionRow>>::new(),
            |mut grouped, row| {
                grouped.entry(row.message_id).or_default().push(row);
                grouped
            },
        );
        request_rows
            .into_iter()
            .map(|request| {
                let question_rows = questions_by_message
                    .remove(&request.message_id)
                    .unwrap_or_default();
                let option_rows = options_by_message
                    .remove(&request.message_id)
                    .unwrap_or_default();
                let mut options_by_question = option_rows.into_iter().fold(
                    std::collections::BTreeMap::<i64, Vec<AskUserRequestQuestionOptionRow>>::new(),
                    |mut grouped, row| {
                        grouped.entry(row.question_ordinal).or_default().push(row);
                        grouped
                    },
                );
                Ok(AskUserRequestPayload {
                    id: request.request_id,
                    questions: question_rows
                        .into_iter()
                        .map(|question| crate::ask_user::Question {
                            section: question.section,
                            question: question.question,
                            option: options_by_question
                                .remove(&question.ordinal)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|option| option.value)
                                .collect(),
                            multi_select: question.multi_select,
                        })
                        .collect(),
                })
            })
            .collect()
    }
}

const PREVIEW_MAX_CHARS: usize = 100;

/// Truncate content to a preview string.
pub(super) fn truncate_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.char_indices().nth(PREVIEW_MAX_CHARS) {
        Some((pos, _)) => format!("{}...", &trimmed[..pos]),
        None => trimmed.to_string(),
    }
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct LegacyRemoteProfileRow {
    id: String,
    remote_ssh_server_id: Option<String>,
}

/// Persisted permission request from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionRequestPayload {
    /// ID of the permission request.
    pub id: String,
    /// How the request was presented to the user.
    pub mode: PermissionHistoryMode,
    /// Command or script the permission request covers.
    pub script: String,
}

/// Persisted permission response from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct PermissionResponsePayload {
    /// ID of the permission request this answers.
    pub id: String,
    /// Whether the user approved the request.
    pub approved: bool,
}

/// Persisted ask-user request from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserRequestPayload {
    /// ID of the ask-user request.
    pub id: String,
    /// Questions that were presented to the user.
    pub questions: Vec<crate::ask_user::Question>,
}

/// Persisted ask-user response from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserResponsePayload {
    /// ID of the ask-user request this answers.
    pub id: String,
    /// Answers supplied by the user, one per question.
    pub answers: Vec<crate::ask_user::QuestionAnswer>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::PermissionRequestMode;
    use crate::db::ProfileEnvironmentKind;
    use crate::db::entity::scheduled_task_runs;
    use crate::profile::ProfileEnvironment;
    use crate::session::{
        PendingMessageState, PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
        PersistedAskUserRequest, PersistedPermissionRequest, PersistedPermissionRequestPayload,
        PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, SessionPlanState,
        SessionRuntimeState, TerminalRuntimeState,
    };
    use crate::task_trigger::PersistedTaskTrigger;

    use super::{ChatDb, CreateProfileParams, CreateScheduledTaskParams, RunFence};

    fn test_pending_message(content: &str, mode: PendingUserMessageMode) -> PendingUserMessage {
        PendingUserMessage {
            id: None,
            state: PendingMessageState::default(),
            error: None,
            content: content.to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: mode,
        }
    }

    #[tokio::test]
    async fn archive_and_unarchive_reset_pin_state_and_capacity() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let profile_id = "profile-test";
        db.create_profile(CreateProfileParams {
            id: profile_id,
            name: "Test",
            environment: ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");

        let session_id = "session-test";
        db.create_session_in_profile(
            session_id,
            Some("Test session"),
            Some("test-model"),
            Some(profile_id),
        )
        .await
        .expect("session should be created");

        db.pin_session(session_id)
            .await
            .expect("session should be pinned");
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            1
        );

        db.archive_session(session_id)
            .await
            .expect("session should be archived");
        let archived = db
            .get_session(session_id)
            .await
            .expect("session fetch should succeed")
            .expect("session should exist");
        assert!(archived.archived, "session must be archived");
        assert!(
            !archived.pinned,
            "archived session must be removed from pinned list"
        );
        assert!(
            archived.folder_id.is_none(),
            "archived session must not keep folder assignment"
        );
        assert!(
            archived.pin_sort_order.is_none(),
            "archived session must not keep pin order"
        );
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            0
        );

        db.unarchive_session(session_id)
            .await
            .expect("session should be unarchived");
        let reopened = db
            .get_session(session_id)
            .await
            .expect("session fetch should succeed")
            .expect("session should exist");
        assert!(
            !reopened.archived,
            "unarchived session must return to regular list"
        );
        assert!(
            !reopened.pinned,
            "unarchived session must not auto-return to pinned"
        );
        assert!(
            reopened.folder_id.is_none(),
            "unarchived session must not keep folder assignment"
        );
        assert!(
            reopened.pin_sort_order.is_none(),
            "unarchived session must not keep pin order"
        );
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            0
        );
    }

    #[tokio::test]
    async fn move_to_folder_requires_pinned_and_non_archived_session() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let profile_id = "profile-test";
        db.create_profile(CreateProfileParams {
            id: profile_id,
            name: "Test",
            environment: ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");

        let session_id = "session-test";
        db.create_session_in_profile(
            session_id,
            Some("Test session"),
            Some("test-model"),
            Some(profile_id),
        )
        .await
        .expect("session should be created");

        let folder_id = "folder-test";
        db.create_folder(folder_id, profile_id, "Pinned Folder")
            .await
            .expect("folder should be created");

        let unpinned_err = db
            .move_session_to_folder(session_id, Some(folder_id))
            .await
            .expect_err("moving unpinned session into folder must fail");
        assert!(
            unpinned_err
                .to_string()
                .contains("cannot move unpinned session"),
            "unexpected error: {unpinned_err}"
        );

        db.pin_session(session_id)
            .await
            .expect("session should be pinned");
        db.archive_session(session_id)
            .await
            .expect("session should be archived");

        let archived_err = db
            .move_session_to_folder(session_id, Some(folder_id))
            .await
            .expect_err("moving archived session into folder must fail");
        assert!(
            archived_err
                .to_string()
                .contains("cannot move archived session"),
            "unexpected error: {archived_err}"
        );
    }

    #[tokio::test]
    async fn ensure_default_profiles_preserve_existing_profiles_by_slug() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_profile(CreateProfileParams {
            id: "daily-id",
            name: "Custom Daily",
            environment: ProfileEnvironmentKind::Container,
            slug: Some("daily"),
            icon: Some("D"),
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("daily profile should be created");
        db.create_profile(CreateProfileParams {
            id: "build-id",
            name: "Custom Build",
            environment: ProfileEnvironmentKind::Local,
            slug: Some("build"),
            icon: Some("B"),
            persona: None,
            preferences: None,
            location_aware: true,
        })
        .await
        .expect("build profile should be created");

        let fallback_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        assert_eq!(fallback_id, "build-id");

        let all = db.list_profiles().await.expect("profiles should be listed");
        assert_eq!(
            all.len(),
            3,
            "existing slugged profiles are reused and the missing default is created"
        );

        let daily = db
            .get_profile_by_slug("daily")
            .await
            .expect("daily query should succeed")
            .expect("daily profile should exist");
        assert_eq!(daily.id, "daily-id");
        assert_eq!(daily.name, "Custom Daily");
        assert_eq!(daily.environment, "container");
        assert_eq!(daily.icon.as_deref(), Some("D"));
        assert!(!daily.location_aware);

        let build = db
            .get_profile_by_slug("build")
            .await
            .expect("build query should succeed")
            .expect("build profile should exist");
        assert_eq!(build.id, "build-id");
        assert_eq!(build.name, "Custom Build");
        assert_eq!(build.environment, "local");
        assert_eq!(build.icon.as_deref(), Some("B"));
        assert!(build.location_aware);

        let work = db
            .get_profile_by_slug("work")
            .await
            .expect("work query should succeed")
            .expect("work profile should exist");
        assert_eq!(work.id, "work");
        assert_eq!(work.name, "Work");
        assert_eq!(work.environment, "local");
        assert!(!work.location_aware);
    }

    #[tokio::test]
    async fn preserves_older_steer_messages_when_new_steer_arrives() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");

        db.set_pending_immediate_message(
            session_id,
            test_pending_message("first steer", PendingUserMessageMode::Steer),
        )
        .await
        .expect("first steer should persist");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("second steer", PendingUserMessageMode::Steer),
        )
        .await
        .expect("second steer should persist");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(
            runtime
                .pending_immediate
                .as_ref()
                .map(|message| message.content.as_str()),
            Some("second steer")
        );
        assert_eq!(runtime.queued_messages.len(), 1);
        assert_eq!(runtime.queued_messages[0].content, "first steer");
        assert_eq!(
            runtime.queued_messages[0].delivery_mode,
            PendingUserMessageMode::Steer
        );

        let first = db
            .claim_next_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("first pending should load")
            .expect("first pending should exist");
        assert_eq!(first.content, "second steer");

        let second = db
            .claim_next_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("second pending should load")
            .expect("second pending should exist");
        assert_eq!(second.content, "first steer");
        assert_eq!(second.delivery_mode, PendingUserMessageMode::Steer);
    }

    #[tokio::test]
    async fn rejects_archiving_active_sessions() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_session_runtime_state(session_id, SessionRuntimeState::Running)
            .await
            .expect("runtime state should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("active session must not archive");
        assert!(
            error.to_string().contains("cannot archive active session"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn browser_takeover_state_persists_and_is_consumable() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_a = "session-a";
        let session_b = "session-b";
        db.create_session(session_a, Some("Session A"), Some("test-model"))
            .await
            .expect("first session should be created");
        db.create_session(session_b, Some("Session B"), Some("test-model"))
            .await
            .expect("second session should be created");

        db.set_browser_takeover_owner(session_a)
            .await
            .expect("browser takeover should persist");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("takeover owner should load"),
            Some(session_a.to_string())
        );
        assert!(
            db.session_has_browser_takeover(session_a)
                .await
                .expect("session takeover state should load")
        );

        db.clear_browser_takeover_owner(session_a)
            .await
            .expect("browser takeover release should persist");
        assert!(
            db.take_browser_takeover_alert_pending(session_a)
                .await
                .expect("pending takeover alert should be consumable")
        );
        assert!(
            !db.take_browser_takeover_alert_pending(session_a)
                .await
                .expect("pending takeover alert should clear after consumption")
        );

        db.set_browser_takeover_owner(session_b)
            .await
            .expect("browser takeover owner should switch");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("switched takeover owner should load"),
            Some(session_b.to_string())
        );
        assert!(
            !db.session_has_browser_takeover(session_a)
                .await
                .expect("previous takeover owner should be cleared")
        );

        db.recover_session_runtimes_after_restart()
            .await
            .expect("runtime recovery should clear stale takeover ownership");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("no active takeover should remain after restart"),
            None
        );
        assert!(
            db.take_browser_takeover_alert_pending(session_b)
                .await
                .expect("restart recovery should preserve takeover alert as pending")
        );
    }

    #[tokio::test]
    async fn takeover_resume_becomes_dispatchable_after_cancelled_run_settles() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");
        let session_id = "takeover-race";
        db.create_session(session_id, Some("Takeover race"), Some("test-model"))
            .await
            .expect("session should be created");
        let fence = RunFence {
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
        };
        assert!(
            db.claim_session_run(&fence)
                .await
                .expect("run claim should succeed")
        );
        db.set_browser_takeover_owner(session_id)
            .await
            .expect("takeover should be active");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("", PendingUserMessageMode::Steer),
        )
        .await
        .expect("resume message should be queued");

        let released = db
            .clear_browser_takeover_owner(session_id)
            .await
            .expect("takeover should be released");
        assert!(
            !released.can_dispatch_follow_up(),
            "the still-running cancelled turn must retain the dispatch gate"
        );

        assert!(
            db.finish_session_run(&fence)
                .await
                .expect("cancelled run should settle")
        );
        let settled = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert!(
            settled.can_dispatch_follow_up(),
            "resume must become dispatchable after the cancelled run settles"
        );
        assert!(settled.interactions.browser.takeover_alert_pending);
    }

    #[tokio::test]
    async fn request_logs_persist_in_database() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.append_request_log(super::RequestLogParams {
            session_id: "session-test",
            timestamp: "2026-03-14T00:00:00Z",
            model: "gpt-5",
            provider_id: "openai",
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_tokens: Some(5),
            cache_write_tokens: Some(0),
            context_window: Some(200_000),
            elapsed_ms: 42,
            ok: false,
            error: Some("boom"),
        })
        .await
        .expect("request log should persist");

        let entries = db
            .list_request_logs(10)
            .await
            .expect("request logs should load");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "session-test");
        assert_eq!(entries[0].model, "gpt-5");
        assert_eq!(entries[0].provider_id, "openai");
        assert_eq!(entries[0].prompt_tokens, Some(10));
        assert_eq!(entries[0].completion_tokens, Some(20));
        assert_eq!(entries[0].total_tokens, Some(30));
        assert_eq!(entries[0].cache_read_tokens, Some(5));
        assert_eq!(entries[0].context_window, Some(200_000));
        assert_eq!(entries[0].elapsed_ms, 42);
        assert!(!entries[0].ok);
        assert_eq!(entries[0].error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn rejects_archiving_sessions_with_pending_follow_up_work() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("steer me", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("session with pending work must not archive");
        assert!(
            error.to_string().contains("pending follow-up work exists"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn rejects_archiving_sessions_waiting_on_workspace_access() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_waiting_workspace(
            session_id,
            Some(PersistedWorkspaceRequest {
                id: "workspace-1".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "Need write access".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting workspace should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("session with pending workspace request must not archive");
        assert!(
            error.to_string().contains("cannot archive active session"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn profile_remote_attachments_round_trip() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "local-profile",
                name: "Local",
                environment: ProfileEnvironmentKind::Local,
                slug: None,
                icon: None,
                persona: None,
                preferences: None,
                location_aware: false,
            })
            .await
            .expect("local profile should be created");

        db.set_profile_remote_servers(
            &model.id,
            &[
                "prod".to_string(),
                "staging".to_string(),
                "edge".to_string(),
            ],
        )
        .await
        .expect("attachments should persist");

        let info = db
            .profile_info_from_model(model)
            .await
            .expect("profile should deserialize");
        assert!(matches!(info.environment.kind(), ProfileEnvironment::Local));
        assert_eq!(
            info.remote_attachments,
            vec![
                "prod".to_string(),
                "staging".to_string(),
                "edge".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn profile_remote_attachments_replace_on_rewrite() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "attach-profile",
                name: "Attach",
                environment: ProfileEnvironmentKind::Local,
                slug: None,
                icon: None,
                persona: None,
                preferences: None,
                location_aware: false,
            })
            .await
            .expect("profile should be created");

        db.set_profile_remote_servers(&model.id, &["a".to_string(), "b".to_string()])
            .await
            .expect("initial attachments");
        db.set_profile_remote_servers(&model.id, &["c".to_string()])
            .await
            .expect("rewrite attachments");
        let attachments = db
            .list_profile_remote_servers(&model.id)
            .await
            .expect("attachments should load");
        assert_eq!(attachments, vec!["c".to_string()]);
    }

    #[tokio::test]
    async fn legacy_remote_environment_migrates_to_local_with_attachment() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");

        {
            let db = ChatDb::open(&db_path)
                .await
                .expect("initial open should succeed");
            // Manufacture a legacy row by writing directly: the newer
            // create_profile path refuses `remote`.
            let now = jiff::Timestamp::now().to_string();
            super::query!(
                db.db,
                "INSERT INTO profiles (id, slug, name, icon, environment, container_id, persona, remote_ssh_server_id, location_aware, pinned_limit, sort_order, created_at, updated_at) VALUES (?, ?, ?, ?, 'remote', NULL, NULL, 'prod', 0, 10, 99, ?, ?)",
                "legacy-remote",
                Option::<String>::None,
                "Legacy Remote",
                Option::<String>::None,
                now.as_str(),
                now.as_str()
            )
            .execute()
            .await
            .expect("legacy row should insert");
        }

        let db = ChatDb::open(&db_path)
            .await
            .expect("second open triggers migration");
        let model = db
            .get_profile("legacy-remote")
            .await
            .expect("lookup")
            .expect("legacy profile preserved");
        assert_eq!(model.environment, "local");
        assert_eq!(model.remote_ssh_server_id, None);
        let attachments = db
            .list_profile_remote_servers("legacy-remote")
            .await
            .expect("attachments lookup");
        assert_eq!(attachments, vec!["prod".to_string()]);
    }

    #[tokio::test]
    async fn restart_recovery_preserves_waiting_requests_and_pending_dispatch() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("steer me", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");
        db.set_waiting_permission(
            session_id,
            Some(PersistedPermissionRequest {
                id: "perm-1".to_string(),
                request: PersistedPermissionRequestPayload {
                    mode: PermissionRequestMode::Default,
                    script: "echo hi".to_string(),
                    session_id: Some(session_id.to_string()),
                    agent_id: None,
                },
            }),
        )
        .await
        .expect("waiting permission should persist");
        db.set_waiting_ask_user(
            session_id,
            Some(PersistedAskUserRequest {
                id: "ask-1".to_string(),
                questions: vec![PersistedAskUserQuestion {
                    section: "default".to_string(),
                    question: "Proceed?".to_string(),
                    option: vec!["yes".to_string()],
                    multi_select: false,
                    secret: None,
                }],
            }),
        )
        .await
        .expect("waiting ask-user should persist");
        db.set_waiting_workspace(
            session_id,
            Some(PersistedWorkspaceRequest {
                id: "workspace-1".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "Need write access".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting workspace should persist");

        db.recover_session_runtimes_after_restart()
            .await
            .expect("runtime recovery should succeed");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(runtime.state, SessionRuntimeState::WaitingPermission);
        assert!(runtime.waiting_permission.is_some());
        assert!(runtime.waiting_workspace.is_some());
        assert!(runtime.waiting_ask_user.is_some());
        assert!(runtime.has_pending_immediate());
        assert_eq!(runtime.queued_count, 1);

        let resumable = db
            .sessions_requiring_pending_dispatch()
            .await
            .expect("pending dispatch lookup should succeed");
        assert_eq!(resumable, [] as [std::string::String; 0]);
    }

    #[tokio::test]
    async fn removed_waiting_terminal_input_state_migrates_to_idle() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let session_id = "session-test";

        {
            let db = ChatDb::open(&db_path)
                .await
                .expect("db should initialize successfully");
            db.create_session(session_id, Some("Test session"), Some("test-model"))
                .await
                .expect("session should be created");
            db.set_session_runtime_state(session_id, SessionRuntimeState::Running)
                .await
                .expect("runtime row should persist");
            // Manufacture the row an older build wrote.
            super::query!(
                db.db,
                "UPDATE session_runtimes SET state = 'waiting_terminal_input', terminal_task_id = 'amber-forest-thunder-pearl', terminal_output_file = 'outputs/amber-forest-thunder-pearl.txt', terminal_output_preview = 'preview', terminal_input_notice = 'input needed' WHERE session_id = ?",
                session_id
            )
            .execute()
            .await
            .expect("legacy row should be written");
        }

        let db = ChatDb::open(&db_path)
            .await
            .expect("reopen should migrate the removed state");
        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(runtime.state, SessionRuntimeState::Idle);
        assert_eq!(
            runtime.interactions.terminal,
            TerminalRuntimeState::default()
        );
    }

    #[tokio::test]
    async fn dispatchable_pending_message_requires_idle_runtime() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("resume later", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");
        db.set_waiting_permission(
            session_id,
            Some(PersistedPermissionRequest {
                id: "perm-1".to_string(),
                request: PersistedPermissionRequestPayload {
                    mode: PermissionRequestMode::Default,
                    script: "echo hi".to_string(),
                    session_id: Some(session_id.to_string()),
                    agent_id: None,
                },
            }),
        )
        .await
        .expect("waiting permission should persist");

        let blocked = db
            .take_next_dispatchable_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("dispatchable lookup should succeed");
        assert!(blocked.is_none());

        db.set_waiting_permission(session_id, None)
            .await
            .expect("waiting permission should clear");
        db.set_session_runtime_state(session_id, SessionRuntimeState::Idle)
            .await
            .expect("runtime should become idle before resumed dispatch");

        let resumed = db
            .take_next_dispatchable_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("dispatchable lookup should succeed")
            .expect("pending steer should dispatch once idle");
        assert_eq!(resumed.content, "resume later");
    }

    #[tokio::test]
    async fn notification_callback_consumes_only_once() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let payload = crate::notification::NotificationPayload {
            notification_id: Some("notification-1".to_string()),
            title: "Task".to_string(),
            body: "Completed".to_string(),
            session_id: Some("session-1".to_string()),
            callback_context_xml: Some(
                crate::XmlBlock::parse("<system-reminder><status>ok</status></system-reminder>")
                    .expect("callback XML should parse"),
            ),
            callback_message: Some("Follow up".to_string()),
        };
        db.create_notification_mailbox_entry("notification-1", &payload, "delivered")
            .await
            .expect("mailbox entry should persist");

        let first = db
            .consume_notification_callback("notification-1")
            .await
            .expect("first callback consume should succeed");
        assert!(first.has_callback);
        assert!(first.callback_consumed);
        assert_eq!(first.callback_message.as_deref(), Some("Follow up"));

        let second = db
            .consume_notification_callback("notification-1")
            .await
            .expect("second callback consume should succeed");
        assert!(second.has_callback);
        assert!(!second.callback_consumed);
        assert_eq!(second.callback_message.as_deref(), Some("Follow up"));
    }

    #[tokio::test]
    async fn task_run_recovery_marks_running_runs_interrupted() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_scheduled_task(CreateScheduledTaskParams {
            id: "task-1",
            profile_id: "profile-1",
            session_id: Some("session-1"),
            name: "Demo task",
            trigger: &PersistedTaskTrigger::Event {
                event_name: "chat.turn.completed".to_string(),
            },
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi"),
            needs_network: false,
            timeout_secs: 60,
        })
        .await
        .expect("task should persist");
        let task = db
            .get_scheduled_task("task-1")
            .await
            .expect("task lookup should succeed")
            .expect("task should exist");
        db.start_task_run("run-1", &task, "event")
            .await
            .expect("task run should persist");

        db.recover_task_runs_after_restart()
            .await
            .expect("task run recovery should succeed");

        let recovered = query!(
            db.db,
            "SELECT * FROM scheduled_task_runs WHERE id = ?",
            "run-1"
        )
        .fetch_optional::<scheduled_task_runs::Model>()
        .await
        .expect("task run lookup should succeed")
        .expect("task run should exist");
        assert_eq!(recovered.status, "interrupted");
        assert!(recovered.finished_at.is_some());
        assert_eq!(
            recovered.output.as_deref(),
            Some("May server restarted before the task run completed")
        );
    }

    #[tokio::test]
    async fn open_puts_the_database_in_wal_mode() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        let mode = query!(db.db, "PRAGMA journal_mode")
            .fetch_one::<JournalMode>()
            .await
            .expect("journal mode should be readable");
        assert_eq!(mode.journal_mode, "wal");
    }

    #[derive(Debug, serde::Deserialize, skyzen::FromRow)]
    struct JournalMode {
        journal_mode: String,
    }

    /// Concurrent dequeues of distinct sessions' pending messages on one
    /// database file must all succeed and hand back every queued message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_pending_message_takes_never_hit_lock_errors() {
        const SESSIONS: usize = 16;
        const MESSAGES: usize = 10;

        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        let session_ids = (0..SESSIONS)
            .map(|index| format!("session-{index}"))
            .collect::<Vec<_>>();
        for session_id in &session_ids {
            db.create_session(session_id, Some("Session"), Some("test-model"))
                .await
                .expect("session should persist");
            for index in 0..MESSAGES {
                db.push_queued_message(
                    session_id,
                    test_pending_message(
                        &format!("message {index}"),
                        PendingUserMessageMode::Queue,
                    ),
                )
                .await
                .expect("queued message should persist");
            }
        }

        let tasks = session_ids
            .into_iter()
            .map(|session_id| {
                let db = db.clone();
                tokio::spawn(async move {
                    for index in 0..MESSAGES {
                        let message = db
                            .claim_next_pending_message(
                                &RunFence {
                                    session_id: session_id.clone(),
                                    run_id: "run".to_string(),
                                },
                                &session_id,
                            )
                            .await
                            .unwrap_or_else(|error| {
                                panic!("take {index} of {session_id} failed: {error}")
                            })
                            .unwrap_or_else(|| {
                                panic!("take {index} of {session_id} found no message")
                            });
                        assert_eq!(message.content, format!("message {index}"));
                    }
                    assert!(
                        db.claim_next_pending_message(
                            &RunFence {
                                session_id: session_id.clone(),
                                run_id: "run".to_string()
                            },
                            &session_id
                        )
                        .await
                        .expect("final take should succeed")
                        .is_none(),
                        "{session_id} should have no pending message left"
                    );
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.expect("take task should not panic");
        }
    }

    /// A session whose plan is still a draft accepts a new request but is not
    /// archived: the draft only blocks auto-archive, not admission.
    #[tokio::test]
    async fn a_draft_session_admits_requests_but_is_not_archived() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("session should be created");

        let runtime = db
            .set_session_plan_state("session", SessionPlanState::Draft)
            .await
            .expect("plan state should be set");
        assert!(runtime.admits_new_request());
        assert!(runtime.blocks_auto_archive());

        let error = db
            .archive_session("session")
            .await
            .expect_err("a draft plan must block archiving");
        assert!(
            format!("{error}").contains("draft"),
            "the refusal should name the draft plan: {error}"
        );
        assert!(
            !db.get_session("session")
                .await
                .expect("session lookup")
                .expect("session exists")
                .archived,
            "the auto-archive pass must leave the draft session open"
        );
    }

    fn queue_entry_fence(session_id: &str) -> RunFence {
        RunFence {
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
        }
    }

    #[tokio::test]
    async fn a_prep_failure_after_claim_returns_the_message_to_pending() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "claim-release";
        db.create_session(session_id, Some("t"), Some("model"))
            .await
            .expect("session");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("queued input", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue");
        let fence = queue_entry_fence(session_id);
        assert!(db.claim_session_run(&fence).await.expect("run claim"));

        let claimed = db
            .claim_next_pending_message(&fence, session_id)
            .await
            .expect("claim")
            .expect("message waits in the queue");
        assert_eq!(claimed.state, PendingMessageState::Claimed);
        let entry_id = claimed.id.expect("claimed rows carry their entry id");
        assert_eq!(
            db.load_session_runtime_summary(session_id)
                .await
                .expect("summary")
                .queued_messages
                .len(),
            1,
            "a claimed entry stays visible to the queue's readers"
        );

        db.release_pending_claim(entry_id, &fence)
            .await
            .expect("release");
        let reclaimed = db
            .claim_next_pending_message(&fence, session_id)
            .await
            .expect("second claim")
            .expect("the released entry is pending again and still runs");
        assert_eq!(reclaimed.content, "queued input");
    }

    #[tokio::test]
    async fn a_lost_claim_returns_to_pending_on_recovery() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "claim-crash";
        db.create_session(session_id, Some("t"), Some("model"))
            .await
            .expect("session");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("queued input", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue");
        let fence = queue_entry_fence(session_id);
        assert!(db.claim_session_run(&fence).await.expect("run claim"));
        db.claim_next_pending_message(&fence, session_id)
            .await
            .expect("claim")
            .expect("message waits in the queue");

        db.recover_session_runtimes_after_restart()
            .await
            .expect("recovery");
        let fence2 = RunFence {
            session_id: session_id.to_string(),
            run_id: "run-2".to_string(),
        };
        assert!(
            db.claim_session_run(&fence2).await.expect("new run claim"),
            "the expired lease frees the session"
        );
        let reclaimed = db
            .claim_next_pending_message(&fence2, session_id)
            .await
            .expect("second claim")
            .expect("the dead run's claim returns to pending");
        assert_eq!(reclaimed.content, "queued input");
    }

    #[tokio::test]
    async fn a_committed_pending_message_never_runs_again() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "claim-commit";
        db.create_session(session_id, Some("t"), Some("model"))
            .await
            .expect("session");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("queued input", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue");
        let fence = queue_entry_fence(session_id);
        assert!(db.claim_session_run(&fence).await.expect("run claim"));
        let claimed = db
            .claim_next_pending_message(&fence, session_id)
            .await
            .expect("claim")
            .expect("message waits in the queue");
        let entry_id = claimed.id.expect("claimed rows carry their entry id");

        db.append_chat_message_as_run_and_commit_pending(
            &fence,
            entry_id,
            &crate::session::ChatMessage::user("queued input"),
        )
        .await
        .expect("commit");
        assert!(
            db.claim_next_pending_message(&fence, session_id)
                .await
                .expect("second claim")
                .is_none(),
            "a committed entry never runs twice"
        );
        assert!(
            db.load_session_runtime_summary(session_id)
                .await
                .expect("summary")
                .queued_messages
                .is_empty(),
            "a committed entry leaves the queue"
        );
        let history = crate::session::load_history_from_db(&db, session_id)
            .await
            .expect("history");
        assert_eq!(
            history
                .messages()
                .iter()
                .filter(|m| m.role == crate::session::MessageRole::User)
                .count(),
            1,
            "the committed input lands in the transcript exactly once"
        );
    }
}
