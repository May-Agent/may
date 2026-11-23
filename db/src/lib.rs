#![cfg_attr(test, allow(clippy::disallowed_methods))]
//! Chat history database backed by SQLite.
//!
//! Single global database at `~/.may/history.db` containing all sessions and messages.

mod cache_misses;
mod checkout_state;
mod ci;
mod cloud_session;
mod common;
mod context_snapshot;
mod daemon_exit;
pub mod entity;
mod find;
pub mod ids;
mod issue;
mod issue_provenance;
mod issue_search;
mod logs;
mod memory;
mod message;
mod notification;
mod open_questions;
mod pr;
mod pr_checks;
pub mod profile;
mod repo_clone;
mod repo_usage;
mod rewind;
mod schema_check;
mod search_outbox;
mod session;
mod session_ids;

pub use ids::DaemonIds;
mod session_intervals;
mod session_run;
mod sleep_log;
mod sql;
mod task;
mod tool_cards;
mod transcript_rows;

use std::path::Path;

use skyzen_services::Db as SkyDb;
use skyzen_services::sql::sqlite::{SqliteConnectOptions, SqliteJournalMode};

use may_protocol::permission::PermissionHistoryMode;
use sql as db_sql;

pub use cache_misses::PromptCacheRequest;
pub use ci::{CiBaselineRow, CiFindingRow, CiJobRow, CiPrRunRow, CiStepRow, CiTestRow, NewCiJob};
pub use cloud_session::CloudSessionRow;
pub use common::{
    AssistantCacheUsageSummary, ChatDbError, ContextDebugSnapshot, ContextDebugSnapshotMessage,
    CreateProfileParams, CreateScheduledTaskParams, MessageStatsParams, NetworkAccessLogParams,
    NetworkAccessMode, NetworkAccessSource, PendingWorkspaceLookup, ProfileEnvironmentKind,
    RequestLogParams, SessionContextSnapshotUpsert, StoredSessionContextSnapshot, TranscriptEntry,
    UpdateProfileParams, UpdateScheduledTaskParams,
};
pub use context_snapshot::ContextDebugSnapshotWrite;
pub use daemon_exit::PreviousDaemon;
pub use find::{MessageScan, ScannedMessage};
pub use issue_provenance::{IssueProvenanceRecord, NewIssueProvenance, UserMessageAt};
pub use issue_search::IssueSearchDoc;
pub use message::MessageMeta;
pub use open_questions::{CommittedAskUserAnswer, NamedAskUserQuestion, PendingAskUserQuestion};
pub use pr::{PrEventRow, PrFilter, PrRow, PrState, PrSummaryRow, pr_sigil};
pub use pr_checks::PrCheckState;
pub use repo_clone::RepoCloneRow;
pub use repo_usage::RepoUsage;
pub use rewind::RewindOutcome;
pub use search_outbox::{IndexedMessageRow, SearchOutboxEntry};
pub use session_intervals::SessionIntervals;
pub use session_run::RunFence;
pub use transcript_rows::TranscriptRows;

use common::{
    AskUserRequestMessageRow, AskUserRequestQuestionOptionRow, AskUserRequestQuestionRow,
    I64ValueRow, PermissionRequestMessageRow, db_custom_error, overflow_error, storage_error,
    to_i32, to_i64_from_u64,
};

/// Runs `sql` on `db` with `param`s bound in order, as this crate's queries do.
#[macro_export]
macro_rules! query {
    ($db:expr, $sql:expr $(, $param:expr)* $(,)?) => {{
        $db.query($sql)$(.bind($param))*
    }};
}

/// Chat history database.
///
#[derive(Debug, Clone)]
pub struct ChatDb {
    #[doc(hidden)]
    pub db: SkyDb,
    /// Each session's pool of model-facing ids.
    session_pools: session_ids::SessionPools,
    /// Test seam: suspends a run claim or finish at its await and injects the
    /// storage faults disk-exhaustion coverage relies on.
    /// Test gates into session-run stages.
    #[doc(hidden)]
    pub run_gates: session_run::RunGates,
}

impl ChatDb {
    /// Connect to the history database file, creating it when missing. The
    /// file is in WAL mode, so [`SkyDb`] serves reads on read-only connections
    /// while its one writer connection queues writes.
    pub(crate) async fn connect(db_path: &Path) -> Result<SkyDb, ChatDbError> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .pragma("synchronous", "NORMAL")
            .pragma("temp_store", "MEMORY")
            .pragma("cache_size", "-32000");
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
        Self::apply_schema(&db).await?;
        Self::check_schema(&db, db_path).await?;
        Ok(Self {
            db,
            session_pools: session_ids::SessionPools::default(),
            run_gates: session_run::RunGates::default(),
        })
    }

    /// Run `sql` as given, so a test can break the storage a commit needs.
    #[doc(hidden)]
    pub async fn execute_for_test(&self, sql: &str) -> Result<(), ChatDbError> {
        query!(self.db, sql).execute().await?;
        Ok(())
    }

    async fn apply_schema(db: &SkyDb) -> Result<(), ChatDbError> {
        Self::initialize_schema(db).await?;
        Self::migrate_session_runtimes_pid(db).await?;
        Self::migrate_session_repo_clones(db).await?;
        Self::migrate_pr_external_numbers(db).await?;
        Self::ensure_session_context_snapshot_table(db).await?;
        Self::ensure_search_outbox(db).await?;
        Self::ensure_session_interval_triggers(db).await
    }

    async fn migrate_session_runtimes_pid(db: &SkyDb) -> Result<(), ChatDbError> {
        let has_pid = query!(
            db,
            "SELECT 1 AS value FROM pragma_table_info('session_runtimes') WHERE name = 'terminal_pid'"
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .is_some();
        if !has_pid {
            query!(
                db,
                "ALTER TABLE session_runtimes ADD COLUMN terminal_pid INTEGER"
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    async fn initialize_schema(db: &SkyDb) -> Result<(), ChatDbError> {
        for statement in db_sql::statements(db_sql::CHAT_SCHEMA) {
            query!(db, statement).execute().await?;
        }
        Ok(())
    }

    async fn ensure_session_context_snapshot_table(db: &SkyDb) -> Result<(), ChatDbError> {
        for statement in db_sql::statements(db_sql::SESSION_CONTEXT_SNAPSHOT_SCHEMA) {
            query!(db, statement).execute().await?;
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
            "SELECT request.request_id AS request_id, request.mode AS mode, request.script AS script FROM messages AS message INNER JOIN message_permission_requests AS request ON request.message_id = message.id WHERE message.session_id = ? AND NOT EXISTS (SELECT 1 FROM message_permission_responses AS response INNER JOIN messages AS response_message ON response_message.id = response.message_id WHERE response.request_id = request.request_id AND response_message.session_id = message.session_id) ORDER BY message.id ASC",
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
                        .map(|question| may_protocol::ask_user::Question {
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
pub(crate) fn truncate_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.char_indices().nth(PREVIEW_MAX_CHARS) {
        Some((pos, _)) => format!("{}...", &trimmed[..pos]),
        None => trimmed.to_string(),
    }
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
    pub questions: Vec<may_protocol::ask_user::Question>,
}

/// Persisted ask-user response from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserResponsePayload {
    /// ID of the ask-user request this answers.
    pub id: String,
    /// Answers supplied by the user, one per question.
    pub answers: Vec<may_protocol::ask_user::QuestionAnswer>,
}
