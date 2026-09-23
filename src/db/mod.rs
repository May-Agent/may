//! Chat history database backed by SQLite.
//!
//! Single global database at `~/.may/history.db` containing all sessions and messages.

mod common;
mod context_snapshot;
pub mod entity;
mod logs;
mod memory;
mod message;
mod notification;
mod profile;
mod session;
mod sql;
mod task;

use std::collections::HashMap;
use std::path::Path;

use skyzen_services::Db as SkyDb;

use crate::PermissionHistoryMode;
use crate::ask_user::AnswerValue;
use crate::profile::{ContainerMountPreference, ContainerProfilePreferences, ProfilePreferences};
use crate::scheduler::TaskRunStatus;
use crate::session::{
    BrowserInteractionState, ChatMessage, ChatMessageStats, MessageRole, PendingUserMessage,
    PersistedAskUserRequest, PersistedPermissionRequest, PersistedWorkspaceRequest,
    SessionInteractionState, SessionPlanState, SessionRuntimeState, SessionRuntimeSummary,
    TerminalRuntimeState,
};
use crate::task_trigger::{PersistedTaskTrigger, TRIGGER_KIND_WEBHOOK};
use crate::xml::XmlBlock;
use entity::{
    folders, messages, network_access_logs, notification_mailbox, profile_memories,
    profile_memory_jobs, profiles, push_subscriptions, request_logs, scheduled_tasks,
    session_pending_permissions, session_pending_workspace, session_plans, session_run_leases,
    session_runtimes, sessions,
};
use sql as db_sql;

pub use common::{
    AssistantCacheUsageSummary, ChatDbError, ContextDebugSnapshot, ContextDebugSnapshotMessage,
    CreateProfileParams, CreateScheduledTaskParams, MessageStatsParams, NetworkAccessLogParams,
    NetworkAccessMode, NetworkAccessSource, NotificationCallbackConsumeResult,
    ProfileEnvironmentKind, RequestLogParams, SessionContextSnapshotUpsert,
    StoredSessionContextSnapshot, UpdateProfileParams, UpdateScheduledTaskParams,
};

use common::{
    AskUserRequestMessageRow, AskUserRequestQuestionOptionRow, AskUserRequestQuestionRow,
    AssistantCacheUsageSummaryRow, ContextDebugMessageAttachmentRow, ContextDebugMessageRow,
    ContextDebugMessageToolCallRow, ContextDebugPromptAttachmentRow, ContextDebugSnapshotHeaderRow,
    DefaultProfile, I32ValueRow, I64ValueRow, PendingAskUserLookup, PendingPermissionLookup,
    PendingWorkspaceLookup, PermissionRequestMessageRow, ProfileMountRow, SessionIdRow,
    SqliteTableInfoRow, StoredSessionContextSnapshotRow, build_profile_preferences,
    db_custom_error, message_stats_params_from_chat_stats, normalize_profile_preferences,
    overflow_error, parse_attachment_kind, parse_profile_environment, row_not_found, storage_error,
    to_i32, to_i64_from_u64, to_u64, to_usize,
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
}

impl ChatDb {
    /// Open (or create) the chat history database at the given path.
    ///
    /// Creates the current schema if the tables don't exist.
    pub async fn open(db_path: &Path) -> Result<Self, ChatDbError> {
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let db = SkyDb::connect_sqlite(&url).await?;
        Self::initialize_schema(&db).await?;
        Self::ensure_profile_preference_columns(&db).await?;
        Self::ensure_session_runtime_columns(&db).await?;
        Self::ensure_session_columns(&db).await?;
        Self::ensure_session_context_snapshot_table(&db).await?;
        Self::migrate_legacy_remote_profiles(&db).await?;
        Ok(Self { db })
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

    async fn ensure_session_runtime_columns(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(db, "PRAGMA table_info(session_runtimes)")
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        let existing = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        if !existing.contains("terminal_background_reason") {
            query!(
                db,
                "ALTER TABLE session_runtimes ADD COLUMN terminal_background_reason TEXT"
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
        if !existing.contains("container_dockerfile") {
            query!(
                db,
                "ALTER TABLE profiles ADD COLUMN container_dockerfile TEXT"
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    async fn ensure_session_context_snapshot_table(db: &SkyDb) -> Result<(), ChatDbError> {
        let columns = query!(db, db_sql::SESSION_CONTEXT_SNAPSHOT_COLUMNS)
            .fetch_all::<SqliteTableInfoRow>()
            .await?;
        let expected_columns = [
            "session_id",
            "checkpoint_blob",
            "last_message_db_id",
            "updated_at",
        ];
        let actual_columns = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>();

        if !actual_columns.is_empty() && actual_columns != expected_columns {
            query!(db, db_sql::SESSION_CONTEXT_SNAPSHOT_DROP)
                .execute()
                .await?;
        }

        for statement in db_sql::statements(db_sql::SESSION_CONTEXT_SNAPSHOT_SCHEMA) {
            query!(db, statement).execute().await?;
        }

        Ok(())
    }

    // ── Permission / AskUser Queries ─────────────────────────────

    /// Find unanswered permission requests in a session.
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

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyRemoteProfileRow {
    id: String,
    remote_ssh_server_id: Option<String>,
}

/// Persisted permission request from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionRequestPayload {
    pub id: String,
    pub mode: PermissionHistoryMode,
    pub script: String,
}

/// Persisted permission response from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionResponsePayload {
    pub id: String,
    pub approved: bool,
}

/// Persisted ask-user request from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserRequestPayload {
    pub id: String,
    pub questions: Vec<crate::ask_user::Question>,
}

/// Persisted ask-user response from structured history rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserResponsePayload {
    pub id: String,
    pub answers: Vec<crate::ask_user::QuestionAnswer>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::PermissionRequestMode;
    use crate::db::ProfileEnvironmentKind;
    use crate::db::entity::{scheduled_task_runs, session_run_leases};
    use crate::profile::ProfileEnvironment;
    use crate::session::{
        PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
        PersistedAskUserRequest, PersistedPermissionRequest, PersistedPermissionRequestPayload,
        PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, SessionRuntimeState,
        TerminalRuntimeState,
    };
    use crate::task_trigger::PersistedTaskTrigger;

    use super::{ChatDb, CreateProfileParams, CreateScheduledTaskParams};

    fn test_pending_message(content: &str, mode: PendingUserMessageMode) -> PendingUserMessage {
        PendingUserMessage {
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
            container_image: None,
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
            container_image: None,
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
            container_image: Some("custom-image"),
            slug: Some("daily"),
            icon: Some("D"),
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("daily profile should be created");
        db.create_profile(CreateProfileParams {
            id: "work-id",
            name: "Custom Work",
            environment: ProfileEnvironmentKind::Container,
            container_image: Some("work-image"),
            slug: Some("work"),
            icon: Some("W"),
            persona: None,
            preferences: None,
            location_aware: true,
        })
        .await
        .expect("work profile should be created");
        db.create_profile(CreateProfileParams {
            id: "local-id",
            name: "Custom Local",
            environment: ProfileEnvironmentKind::Local,
            container_image: Some("still-here"),
            slug: Some("local"),
            icon: Some("L"),
            persona: None,
            preferences: None,
            location_aware: true,
        })
        .await
        .expect("local profile should be created");

        let first_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        assert_eq!(first_id, "daily-id");

        let all = db.list_profiles().await.expect("profiles should be listed");
        assert_eq!(all.len(), 3, "existing slugged profiles should be reused");

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

        let work = db
            .get_profile_by_slug("work")
            .await
            .expect("work query should succeed")
            .expect("work profile should exist");
        assert_eq!(work.id, "work-id");
        assert_eq!(work.name, "Custom Work");
        assert_eq!(work.environment, "container");
        assert_eq!(work.icon.as_deref(), Some("W"));
        assert!(work.location_aware);

        let local = db
            .get_profile_by_slug("local")
            .await
            .expect("local query should succeed")
            .expect("local profile should exist");
        assert_eq!(local.id, "local-id");
        assert_eq!(local.name, "Custom Local");
        assert_eq!(local.environment, "local");
        assert_eq!(local.icon.as_deref(), Some("L"));
        assert!(local.location_aware);
        assert!(local.container_image.is_none());
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
            .take_next_pending_message(session_id)
            .await
            .expect("first pending should load")
            .expect("first pending should exist");
        assert_eq!(first.content, "second steer");

        let second = db
            .take_next_pending_message(session_id)
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
        db.set_session_runtime_state(session_id, SessionRuntimeState::Running)
            .await
            .expect("run should be marked active");
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
            db.clear_running_if_idle(session_id)
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
                container_image: None,
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
                container_image: None,
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
                "INSERT INTO profiles (id, slug, name, icon, environment, container_image, container_id, persona, remote_ssh_server_id, container_dockerfile, location_aware, pinned_limit, sort_order, created_at, updated_at) VALUES (?, ?, ?, ?, 'remote', NULL, NULL, NULL, 'prod', NULL, 0, 10, 99, ?, ?)",
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
        assert!(resumable.is_empty());
    }

    #[tokio::test]
    async fn restart_recovery_expires_terminal_wait_state() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_session_runtime_state(session_id, SessionRuntimeState::WaitingTerminalInput)
            .await
            .expect("waiting terminal input should persist");
        let mut runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        runtime.interactions.terminal = TerminalRuntimeState {
            task_id: Some("amber-forest-thunder-pearl".to_string()),
            output_file: Some("outputs/amber-forest-thunder-pearl.txt".to_string()),
            output_preview: Some("preview".to_string()),
            input_notice: Some("input needed".to_string()),
            background_reason: None,
        };
        db.write_session_runtime(session_id, &runtime)
            .await
            .expect("terminal runtime metadata should persist");

        db.recover_session_runtimes_after_restart()
            .await
            .expect("runtime recovery should succeed");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(runtime.state, SessionRuntimeState::Idle);
        assert!(!runtime.has_pending_messages());
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
                },
            }),
        )
        .await
        .expect("waiting permission should persist");

        let blocked = db
            .take_next_dispatchable_pending_message(session_id)
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
            .take_next_dispatchable_pending_message(session_id)
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
    async fn session_run_lease_rejects_second_claim_until_released() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_session("session-1", Some("Session"), Some("test-model"))
            .await
            .expect("session should persist");

        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("first lease claim should succeed")
        );
        assert!(
            !db.claim_session_run_lease("session-1")
                .await
                .expect("second lease claim should be rejected")
        );
        assert!(
            db.request_session_run_cancel("session-1")
                .await
                .expect("cancellation request should persist against active lease")
        );

        db.release_session_run_lease("session-1")
            .await
            .expect("lease release should persist");

        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("released lease should become claimable again")
        );
    }

    #[tokio::test]
    async fn session_run_lease_recovery_clears_stale_active_run() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_session("session-1", Some("Session"), Some("test-model"))
            .await
            .expect("session should persist");
        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("lease claim should succeed")
        );
        assert!(
            db.request_session_run_cancel("session-1")
                .await
                .expect("cancellation request should persist")
        );

        db.recover_session_run_leases_after_restart()
            .await
            .expect("lease recovery should clear stale active state");

        let recovered = query!(
            db.db,
            "SELECT * FROM session_run_leases WHERE session_id = ?",
            "session-1"
        )
        .fetch_optional::<session_run_leases::Model>()
        .await
        .expect("lease lookup should succeed")
        .expect("lease row should exist");
        assert!(!recovered.active);
        assert!(!recovered.cancellation_requested);
        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("recovered lease should become claimable")
        );
    }
}
