//! Chat history database backed by SQLite.
//!
//! Single global database at `~/.may/history.db` containing all sessions and messages.

mod common;
pub mod entity;
mod logs;
mod message;
mod notification;
mod session;
mod sql;

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
    CreateProfileParams, MessageStatsParams, NetworkAccessLogParams, NetworkAccessMode,
    NetworkAccessSource, NotificationCallbackConsumeResult, ProfileEnvironmentKind,
    RequestLogParams, UpdateProfileParams,
};

use common::{
    AskUserRequestMessageRow, AskUserRequestQuestionOptionRow, AskUserRequestQuestionRow,
    AssistantCacheUsageSummaryRow, ContextDebugMessageAttachmentRow, ContextDebugMessageRow,
    ContextDebugMessageToolCallRow, ContextDebugPromptAttachmentRow,
    ContextDebugSnapshotHeaderRow, DefaultProfile, I32ValueRow, I64ValueRow, PendingAskUserLookup,
    PendingPermissionLookup, PendingWorkspaceLookup, PermissionRequestMessageRow, ProfileMountRow,
    SessionIdRow, SqliteTableInfoRow, StoredSessionContextSnapshotRow, build_profile_preferences,
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
        Self::ensure_session_context_snapshot_table(&db).await?;
        Ok(Self { db })
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
            "history_message_count",
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



    // ── Context Debug Snapshot ────────────────────────────────

    /// Upsert the latest context-debug snapshot for a session.
    pub async fn upsert_context_debug_snapshot(
        &self,
        session_id: &str,
        timestamp: &str,
        model: &str,
        provider_id: &str,
        prompt: &str,
        prompt_attachments: &[String],
        assembled_messages: &[ContextDebugSnapshotMessage],
    ) -> Result<(), ChatDbError> {
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO context_debug_snapshot_headers (session_id, timestamp, model, provider_id, prompt) VALUES (?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET timestamp = excluded.timestamp, model = excluded.model, provider_id = excluded.provider_id, prompt = excluded.prompt",
            session_id,
            timestamp,
            model,
            provider_id,
            prompt
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM context_debug_snapshot_prompt_attachments WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM context_debug_snapshot_message_attachments WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM context_debug_snapshot_message_tool_calls WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM context_debug_snapshot_messages WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        for (ordinal, attachment_url) in prompt_attachments.iter().enumerate() {
            query!(
                tx,
                "INSERT INTO context_debug_snapshot_prompt_attachments (session_id, ordinal, url) VALUES (?, ?, ?)",
                session_id,
                i64::try_from(ordinal).map_err(|error| overflow_error("context debug prompt attachment ordinal", &error))?,
                attachment_url.as_str()
            )
            .execute()
            .await?;
        }
        for (message_ordinal, message) in assembled_messages.iter().enumerate() {
            let message_ordinal = i64::try_from(message_ordinal)
                .map_err(|error| overflow_error("context debug message ordinal", &error))?;
            query!(
                tx,
                "INSERT INTO context_debug_snapshot_messages (session_id, ordinal, role, content, tool_call_id) VALUES (?, ?, ?, ?, ?)",
                session_id,
                message_ordinal,
                message.role.as_str(),
                message.content.as_str(),
                message.tool_call_id.as_deref()
            )
            .execute()
            .await?;
            for (attachment_ordinal, attachment_url) in message.attachments.iter().enumerate() {
                query!(
                    tx,
                    "INSERT INTO context_debug_snapshot_message_attachments (session_id, message_ordinal, attachment_ordinal, url) VALUES (?, ?, ?, ?)",
                    session_id,
                    message_ordinal,
                    i64::try_from(attachment_ordinal).map_err(|error| overflow_error("context debug message attachment ordinal", &error))?,
                    attachment_url.as_str()
                )
                .execute()
                .await?;
            }
            for (tool_call_ordinal, tool_call_name) in message.tool_call_names.iter().enumerate() {
                query!(
                    tx,
                    "INSERT INTO context_debug_snapshot_message_tool_calls (session_id, message_ordinal, tool_call_ordinal, name) VALUES (?, ?, ?, ?)",
                    session_id,
                    message_ordinal,
                    i64::try_from(tool_call_ordinal).map_err(|error| overflow_error("context debug tool call ordinal", &error))?,
                    tool_call_name.as_str()
                )
                .execute()
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Load latest context-debug snapshot for a session.
    pub async fn get_context_debug_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<ContextDebugSnapshot>, ChatDbError> {
        let Some(header) = query!(
            self.db,
            "SELECT * FROM context_debug_snapshot_headers WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<ContextDebugSnapshotHeaderRow>()
        .await?
        else {
            return Ok(None);
        };
        let prompt_attachment_rows = query!(
            self.db,
            "SELECT * FROM context_debug_snapshot_prompt_attachments WHERE session_id = ? ORDER BY ordinal ASC",
            session_id
        )
        .fetch_all::<ContextDebugPromptAttachmentRow>()
        .await?;
        let message_rows = query!(
            self.db,
            "SELECT * FROM context_debug_snapshot_messages WHERE session_id = ? ORDER BY ordinal ASC",
            session_id
        )
        .fetch_all::<ContextDebugMessageRow>()
        .await?;
        let message_attachment_rows = query!(
            self.db,
            "SELECT * FROM context_debug_snapshot_message_attachments WHERE session_id = ? ORDER BY message_ordinal ASC, attachment_ordinal ASC",
            session_id
        )
        .fetch_all::<ContextDebugMessageAttachmentRow>()
        .await?;
        let message_tool_call_rows = query!(
            self.db,
            "SELECT * FROM context_debug_snapshot_message_tool_calls WHERE session_id = ? ORDER BY message_ordinal ASC, tool_call_ordinal ASC",
            session_id
        )
        .fetch_all::<ContextDebugMessageToolCallRow>()
        .await?;
        let mut attachments_by_message = message_attachment_rows.into_iter().fold(
            std::collections::BTreeMap::<i64, Vec<ContextDebugMessageAttachmentRow>>::new(),
            |mut grouped, row| {
                grouped.entry(row.message_ordinal).or_default().push(row);
                grouped
            },
        );
        let mut tool_calls_by_message = message_tool_call_rows.into_iter().fold(
            std::collections::BTreeMap::<i64, Vec<ContextDebugMessageToolCallRow>>::new(),
            |mut grouped, row| {
                grouped.entry(row.message_ordinal).or_default().push(row);
                grouped
            },
        );
        Ok(Some(ContextDebugSnapshot {
            session_id: header.session_id,
            timestamp: header.timestamp,
            model: header.model,
            provider_id: header.provider_id,
            prompt: header.prompt,
            prompt_attachments: prompt_attachment_rows
                .into_iter()
                .map(|row| row.url)
                .collect(),
            assembled_messages: message_rows
                .into_iter()
                .map(|row| ContextDebugSnapshotMessage {
                    role: row.role,
                    content: row.content,
                    attachments: attachments_by_message
                        .remove(&row.ordinal)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|attachment| attachment.url)
                        .collect(),
                    tool_call_id: row.tool_call_id,
                    tool_call_names: tool_calls_by_message
                        .remove(&row.ordinal)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|tool_call| tool_call.name)
                        .collect(),
                })
                .collect(),
        }))
    }

    /// Upsert the latest serialized agent context snapshot for a session.
    pub async fn upsert_session_context_snapshot(
        &self,
        session_id: &str,
        snapshot: SessionContextSnapshotUpsert,
    ) -> Result<(), ChatDbError> {
        let updated_at = jiff::Timestamp::now().to_string();
        let history_message_count = to_i64_from_u64(
            u64::try_from(snapshot.history_message_count)
                .map_err(|error| overflow_error("history_message_count", &error))?,
            "history_message_count",
        )?;
        let checkpoint_blob = rmp_serde::to_vec_named(&snapshot.checkpoint)
            .map_err(|error| storage_error(format!("failed to encode checkpoint blob: {error}")))?;
        query!(
            self.db,
            "INSERT INTO session_context_snapshots (session_id, checkpoint_blob, history_message_count, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET checkpoint_blob = excluded.checkpoint_blob, history_message_count = excluded.history_message_count, updated_at = excluded.updated_at",
            session_id,
            checkpoint_blob,
            history_message_count,
            updated_at
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Load the serialized agent context snapshot for a session.
    pub async fn get_session_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<StoredSessionContextSnapshot>, ChatDbError> {
        let Some(row) = query!(
            self.db,
            "SELECT checkpoint_blob, history_message_count, updated_at FROM session_context_snapshots WHERE session_id = ?",
            session_id
        )
            .fetch_optional::<StoredSessionContextSnapshotRow>()
            .await?
        else {
            return Ok(None);
        };

        let checkpoint = rmp_serde::from_slice::<aither_agent::AgentCheckpoint>(
            &row.checkpoint_blob,
        )
        .map_err(|error| {
            storage_error(format!(
                "failed to decode checkpoint blob for session {session_id}: {error}"
            ))
        })?;
        let history_message_count = to_usize(row.history_message_count, "history_message_count")?;
        Ok(Some(StoredSessionContextSnapshot {
            checkpoint,
            history_message_count,
            updated_at: row.updated_at,
        }))
    }

    /// Delete a persisted context snapshot for a session.
    pub async fn delete_session_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM session_context_snapshots WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }

    // ── Permission / AskUser Queries ─────────────────────────────

    /// Check if network permission was ever approved in a session.
    pub async fn has_network_approval(&self, session_id: &str) -> Result<bool, ChatDbError> {
        self.session_has_approved_permission_mode(session_id, PermissionHistoryMode::Network)
            .await
    }

    /// Find unanswered permission requests in a session.
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

    pub async fn load_profile_preferences(
        &self,
        profile: &profiles::Model,
    ) -> Result<Option<ProfilePreferences>, ChatDbError> {
        let mounts = query!(
            self.db,
            "SELECT * FROM profile_container_mounts WHERE profile_id = ? ORDER BY ordinal ASC",
            profile.id.as_str()
        )
        .fetch_all::<ProfileMountRow>()
        .await?
        .into_iter()
        .map(|row| {
            let access = row.access.parse().map_err(|error| {
                storage_error(db_custom_error("invalid profile mount access: ", &error))
            })?;
            Ok(ContainerMountPreference {
                host_path: row.host_path,
                container_path: row.container_path,
                access,
            })
        })
        .collect::<Result<Vec<_>, ChatDbError>>()?;
        Ok(build_profile_preferences(profile, mounts))
    }

    pub async fn profile_info_from_model(
        &self,
        profile: profiles::Model,
    ) -> Result<crate::profile::ProfileInfo, ChatDbError> {
        let preferences = self.load_profile_preferences(&profile).await?;
        crate::profile::ProfileInfo::from_db_parts(profile, preferences)
    }

    pub async fn load_container_profile_preferences(
        &self,
        profile: &profiles::Model,
    ) -> Result<Option<ContainerProfilePreferences>, ChatDbError> {
        Ok(self
            .load_profile_preferences(profile)
            .await?
            .and_then(|preferences| preferences.container))
    }

    // ── Profile CRUD ────────────────────────────────────────────

    /// Create a new profile.
    pub async fn create_profile(
        &self,
        params: CreateProfileParams<'_>,
    ) -> Result<profiles::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let sort_order = self.max_profile_sort_order().await? + 1;
        let preferences = normalize_profile_preferences(params.environment, params.preferences)?;
        let container_image = match params.environment {
            ProfileEnvironmentKind::Local | ProfileEnvironmentKind::Remote => None,
            ProfileEnvironmentKind::Container => params.container_image,
        };
        let remote_ssh_server_id = preferences
            .as_ref()
            .and_then(|value| value.remote.as_ref())
            .map(|value| value.ssh_server_id.as_str());
        let container_dockerfile = preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .and_then(|value| value.dockerfile.as_deref());
        let container_mounts = preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .map(|value| value.mounts.as_slice())
            .unwrap_or(&[]);
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO profiles (id, slug, name, icon, environment, container_image, container_id, persona, remote_ssh_server_id, container_dockerfile, location_aware, pinned_limit, sort_order, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params.id,
            params.slug,
            params.name,
            params.icon,
            params.environment.as_str(),
            container_image,
            Option::<String>::None,
            params.persona,
            remote_ssh_server_id,
            container_dockerfile,
            params.location_aware,
            10_i32,
            sort_order,
            now.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        for (ordinal, mount) in container_mounts.iter().enumerate() {
            query!(
                tx,
                "INSERT INTO profile_container_mounts (profile_id, ordinal, host_path, container_path, access) VALUES (?, ?, ?, ?, ?)",
                params.id,
                i64::try_from(ordinal).map_err(|error| overflow_error("profile mount ordinal", &error))?,
                mount.host_path.as_str(),
                mount.container_path.as_str(),
                mount.access.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        self.get_profile(params.id)
            .await?
            .ok_or_else(|| row_not_found(format!("Profile not found after insert: {}", params.id)))
    }

    /// List all profiles ordered by `sort_order`.
    pub async fn list_profiles(&self) -> Result<Vec<profiles::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM profiles ORDER BY sort_order ASC")
                .fetch_all::<profiles::Model>()
                .await?,
        )
    }

    /// Get a profile by ID.
    pub async fn get_profile(&self, id: &str) -> Result<Option<profiles::Model>, ChatDbError> {
        Ok(query!(self.db, "SELECT * FROM profiles WHERE id = ?", id)
            .fetch_optional::<profiles::Model>()
            .await?)
    }

    /// Update profile fields.
    pub async fn update_profile(
        &self,
        id: &str,
        params: UpdateProfileParams<'_>,
    ) -> Result<(), ChatDbError> {
        let mut existing = self
            .get_profile(id)
            .await?
            .ok_or_else(|| row_not_found(format!("Profile not found: {id}")))?;
        if let Some(name) = params.name {
            existing.name = name.to_string();
        }
        if let Some(icon) = params.icon {
            existing.icon = icon.map(ToOwned::to_owned);
        }
        if let Some(environment) = params.environment {
            existing.environment = environment.as_str().to_string();
            if matches!(
                environment,
                ProfileEnvironmentKind::Local | ProfileEnvironmentKind::Remote
            ) {
                existing.container_image = None;
                existing.container_id = None;
            }
        }
        if let Some(container_image) = params.container_image {
            existing.container_image = container_image.map(ToOwned::to_owned);
        }
        if let Some(container_id) = params.container_id {
            existing.container_id = container_id.map(ToOwned::to_owned);
        }
        if let Some(sort_order) = params.sort_order {
            existing.sort_order = sort_order;
        }
        if let Some(persona) = params.persona {
            existing.persona = persona.map(ToOwned::to_owned);
        }
        if let Some(location_aware) = params.location_aware {
            existing.location_aware = location_aware;
        }
        let environment = parse_profile_environment(existing.environment.as_str())?;
        let current_preferences = self.load_profile_preferences(&existing).await?;
        let merged_preferences = match params.preferences {
            Some(preferences) => normalize_profile_preferences(environment, preferences)?,
            None => current_preferences,
        };
        existing.remote_ssh_server_id = merged_preferences
            .as_ref()
            .and_then(|value| value.remote.as_ref())
            .map(|value| value.ssh_server_id.clone());
        existing.container_dockerfile = merged_preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .and_then(|value| value.dockerfile.clone());
        let container_mounts = merged_preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .map(|value| value.mounts.as_slice())
            .unwrap_or(&[]);
        existing.updated_at = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "UPDATE profiles SET name = ?, icon = ?, environment = ?, container_image = ?, container_id = ?, persona = ?, remote_ssh_server_id = ?, container_dockerfile = ?, location_aware = ?, pinned_limit = ?, sort_order = ?, created_at = ?, updated_at = ? WHERE id = ?",
            existing.name.as_str(),
            existing.icon.as_deref(),
            existing.environment.as_str(),
            existing.container_image.as_deref(),
            existing.container_id.as_deref(),
            existing.persona.as_deref(),
            existing.remote_ssh_server_id.as_deref(),
            existing.container_dockerfile.as_deref(),
            existing.location_aware,
            existing.pinned_limit,
            existing.sort_order,
            existing.created_at.as_str(),
            existing.updated_at.as_str(),
            id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM profile_container_mounts WHERE profile_id = ?",
            id
        )
        .execute()
        .await?;
        for (ordinal, mount) in container_mounts.iter().enumerate() {
            query!(
                tx,
                "INSERT INTO profile_container_mounts (profile_id, ordinal, host_path, container_path, access) VALUES (?, ?, ?, ?, ?)",
                id,
                i64::try_from(ordinal).map_err(|error| overflow_error("profile mount ordinal", &error))?,
                mount.host_path.as_str(),
                mount.container_path.as_str(),
                mount.access.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete a profile and reassign its sessions to the default profile.
    pub async fn delete_profile(&self, id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET profile_id = NULL WHERE profile_id = ?",
            id
        )
        .execute()
        .await?;
        query!(self.db, "DELETE FROM folders WHERE profile_id = ?", id)
            .execute()
            .await?;
        query!(self.db, "DELETE FROM profiles WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Get a profile by its well-known slug ("daily", "work", "local").
    pub async fn get_profile_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<profiles::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM profiles WHERE slug = ?", slug)
                .fetch_optional::<profiles::Model>()
                .await?,
        )
    }

    /// Ensure the three default profiles (daily, work, local) exist.
    ///
    /// Returns the ID of the first profile (daily).
    pub async fn ensure_default_profiles(&self) -> Result<String, ChatDbError> {
        let local_hostname = hostname::get().map_or_else(
            |_| "local".to_string(),
            |h| h.to_string_lossy().into_owned(),
        );

        let defaults = [
            DefaultProfile {
                slug: "daily",
                name: "Daily",
                icon: "\u{1F305}", // 🌅
                environment: "container",
                location_aware: true,
            },
            DefaultProfile {
                slug: "work",
                name: "Work",
                icon: "\u{1F4BC}", // 💼
                environment: "container",
                location_aware: false,
            },
            DefaultProfile {
                slug: "local",
                name: &local_hostname,
                icon: "\u{1F5A5}\u{FE0F}", // 🖥️
                environment: "local",
                location_aware: false,
            },
        ];

        let mut first_id = None;
        for def in &defaults {
            let ensured = if let Some(existing) = self.get_profile_by_slug(def.slug).await? {
                existing
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                self.create_profile(CreateProfileParams {
                    id: &id,
                    name: def.name,
                    environment: parse_profile_environment(def.environment)?,
                    container_image: None,
                    slug: Some(def.slug),
                    icon: Some(def.icon),
                    persona: None,
                    preferences: None,
                    location_aware: def.location_aware,
                })
                .await?
            };

            if first_id.is_none() {
                first_id = Some(ensured.id.clone());
            }
        }

        // first_id is always set because we iterate at least one default profile
        Ok(first_id.expect("at least one default profile must be created"))
    }

    async fn max_profile_sort_order(&self) -> Result<i32, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT COALESCE(MAX(sort_order), -1) AS value FROM profiles"
        )
        .fetch_one::<I32ValueRow>()
        .await?
        .value)
    }

    // ── Folder CRUD ─────────────────────────────────────────────

    /// Create a folder in a profile.
    pub async fn create_folder(
        &self,
        id: &str,
        profile_id: &str,
        name: &str,
    ) -> Result<folders::Model, ChatDbError> {
        self.ensure_pinned_capacity(profile_id).await?;
        let now = jiff::Timestamp::now().to_string();
        let max_order = self.max_folder_sort_order(profile_id).await?;
        let sort_order = max_order + 1;
        query!(
            self.db,
            "INSERT INTO folders (id, profile_id, name, sort_order, created_at) VALUES (?, ?, ?, ?, ?)",
            id,
            profile_id,
            name,
            sort_order,
            now.clone()
        )
        .execute()
        .await?;
        Ok(folders::Model {
            id: id.to_string(),
            profile_id: profile_id.to_string(),
            name: name.to_string(),
            sort_order,
            created_at: now,
        })
    }

    /// List folders for a profile.
    pub async fn list_folders(&self, profile_id: &str) -> Result<Vec<folders::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM folders WHERE profile_id = ? ORDER BY sort_order ASC",
            profile_id
        )
        .fetch_all::<folders::Model>()
        .await?)
    }

    /// Update a folder's name.
    pub async fn update_folder(&self, id: &str, name: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE folders SET name = ? WHERE id = ?",
            name,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Update a folder's sort order.
    pub async fn update_folder_sort_order(
        &self,
        id: &str,
        sort_order: i32,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE folders SET sort_order = ? WHERE id = ?",
            sort_order,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Delete a folder and unpin its sessions.
    pub async fn delete_folder(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "UPDATE sessions SET folder_id = NULL, pinned = FALSE, pin_sort_order = NULL WHERE folder_id = ?", id)
            .execute()
            .await?;
        query!(self.db, "DELETE FROM folders WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    async fn max_folder_sort_order(&self, profile_id: &str) -> Result<i32, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT COALESCE(MAX(sort_order), -1) AS value FROM folders WHERE profile_id = ?",
            profile_id
        )
        .fetch_one::<I32ValueRow>()
        .await?
        .value)
    }

    // ── Session Profile Operations ──────────────────────────────

    /// Pin a session in its profile.
    pub async fn pin_session(&self, id: &str) -> Result<(), ChatDbError> {
        let session = self
            .get_session(id)
            .await?
            .ok_or_else(|| row_not_found(format!("Session not found: {id}")))?;

        let profile_id = session
            .profile_id
            .as_deref()
            .ok_or_else(|| row_not_found(format!("Session {id} is not attached to a profile")))?;

        // Adding a new standalone pinned session consumes one pinned slot.
        // Moving a folder-contained pinned session to top-level also consumes one slot.
        if !session.pinned || session.folder_id.is_some() {
            self.ensure_pinned_capacity(profile_id).await?;
        }

        let max_order = self.max_pin_sort_order(profile_id).await?;
        query!(
            self.db,
            "UPDATE sessions SET pinned = TRUE, archived = FALSE, pin_sort_order = ? WHERE id = ?",
            max_order + 1,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Unpin a session.
    pub async fn unpin_session(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "UPDATE sessions SET pinned = FALSE, folder_id = NULL, pin_sort_order = NULL WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Archive a session.
    pub async fn archive_session(&self, id: &str) -> Result<(), ChatDbError> {
        let runtime = self.load_session_runtime_summary(id).await?;
        if runtime.is_active() {
            let reason = if runtime.state.is_active() {
                format!("runtime state is {}", runtime.state.as_str())
            } else {
                "pending follow-up work exists".to_string()
            };
            return Err(storage_error(format!(
                "cannot archive active session {id} while {reason}"
            )));
        }
        query!(self.db, "UPDATE sessions SET archived = TRUE, pinned = FALSE, folder_id = NULL, pin_sort_order = NULL WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Unarchive (reopen) a session.
    pub async fn unarchive_session(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(self.db, "UPDATE sessions SET archived = FALSE, pinned = FALSE, folder_id = NULL, pin_sort_order = NULL, updated_at = ? WHERE id = ?", now, id)
            .execute()
            .await?;
        Ok(())
    }

    /// Move a session to a folder (or out of a folder if `folder_id` is None).
    pub async fn move_session_to_folder(
        &self,
        session_id: &str,
        folder_id: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| row_not_found(format!("Session not found: {session_id}")))?;

        if session.archived {
            return Err(storage_error(format!(
                "cannot move archived session {session_id} into pinned folders"
            )));
        }

        if folder_id.is_some() && !session.pinned {
            return Err(storage_error(format!(
                "cannot move unpinned session {session_id} into a pinned folder"
            )));
        }

        // Moving a session out of a folder (while pinned) creates a new standalone
        // pinned item and therefore consumes profile pin capacity.
        if folder_id.is_none() && session.pinned && session.folder_id.is_some() {
            let profile_id = session.profile_id.as_deref().ok_or_else(|| {
                row_not_found(format!("Session {session_id} is not attached to a profile"))
            })?;
            self.ensure_pinned_capacity(profile_id).await?;
        }

        if let Some(target_folder_id) = folder_id {
            let folder = query!(
                self.db,
                "SELECT * FROM folders WHERE id = ?",
                target_folder_id
            )
            .fetch_optional::<folders::Model>()
            .await?
            .ok_or_else(|| row_not_found(format!("Folder not found: {target_folder_id}")))?;

            let session_profile = session.profile_id.as_deref().ok_or_else(|| {
                row_not_found(format!("Session {session_id} is not attached to a profile"))
            })?;
            if folder.profile_id != session_profile {
                return Err(row_not_found(format!(
                    "Folder {target_folder_id} does not belong to session profile {session_profile}"
                )));
            }
        }

        query!(
            self.db,
            "UPDATE sessions SET folder_id = ? WHERE id = ?",
            folder_id,
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Set a session's `profile_id`.
    pub async fn set_session_profile(
        &self,
        session_id: &str,
        profile_id: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET profile_id = ? WHERE id = ?",
            profile_id,
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List sessions for a specific profile, ordered by `updated_at` descending.
    pub async fn list_sessions_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<sessions::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM sessions WHERE profile_id = ? ORDER BY updated_at DESC",
            profile_id
        )
        .fetch_all::<sessions::Model>()
        .await?)
    }

    /// Find non-pinned, non-archived sessions with `updated_at` before the cutoff.
    pub async fn sessions_needing_archive(
        &self,
        cutoff: &str,
    ) -> Result<Vec<sessions::Model>, ChatDbError> {
        let mut active_session_ids = query!(
            self.db,
            "SELECT session_id FROM session_runtimes WHERE state != ?",
            SessionRuntimeState::Idle.as_str()
        )
        .fetch_all::<SessionIdRow>()
        .await?
        .into_iter()
        .map(|row| row.session_id)
        .collect::<std::collections::BTreeSet<_>>();
        active_session_ids.extend(
            query!(
                self.db,
                "SELECT DISTINCT session_id FROM session_pending_message_entries"
            )
            .fetch_all::<SessionIdRow>()
            .await?
            .into_iter()
            .map(|row| row.session_id),
        );
        active_session_ids.extend(
            query!(
                self.db,
                "SELECT session_id FROM session_pending_permissions"
            )
            .fetch_all::<SessionIdRow>()
            .await?
            .into_iter()
            .map(|row| row.session_id),
        );
        active_session_ids.extend(
            query!(
                self.db,
                "SELECT DISTINCT session_id FROM session_pending_ask_user_requests_v2"
            )
            .fetch_all::<SessionIdRow>()
            .await?
            .into_iter()
            .map(|row| row.session_id),
        );
        active_session_ids.extend(
            query!(self.db, "SELECT session_id FROM session_pending_workspace")
                .fetch_all::<SessionIdRow>()
                .await?
                .into_iter()
                .map(|row| row.session_id),
        );

        let sessions = query!(self.db, "SELECT * FROM sessions WHERE pinned = FALSE AND archived = FALSE AND updated_at < ? ORDER BY updated_at ASC", cutoff)
            .fetch_all::<sessions::Model>()
            .await?;
        Ok(sessions
            .into_iter()
            .filter(|session| !active_session_ids.contains(&session.id))
            .collect())
    }

    /// Count pinned items (sessions + folders) in a profile.
    pub async fn pinned_count(&self, profile_id: &str) -> Result<i64, ChatDbError> {
        let session_count = query!(self.db, "SELECT COUNT(*) AS value FROM sessions WHERE profile_id = ? AND pinned = TRUE AND folder_id IS NULL", profile_id)
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        let folder_count = query!(
            self.db,
            "SELECT COUNT(*) AS value FROM folders WHERE profile_id = ?",
            profile_id
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value;

        Ok(session_count + folder_count)
    }

    async fn ensure_pinned_capacity(&self, profile_id: &str) -> Result<(), ChatDbError> {
        let profile = self
            .get_profile(profile_id)
            .await?
            .ok_or_else(|| row_not_found(format!("Profile not found: {profile_id}")))?;
        let current = self.pinned_count(profile_id).await?;
        let limit = i64::from(profile.pinned_limit);
        if current >= limit {
            return Err(ChatDbError::PinnedLimitExceeded {
                profile_id: profile_id.to_string(),
                limit,
                current,
            });
        }
        Ok(())
    }

    async fn max_pin_sort_order(&self, profile_id: &str) -> Result<i32, ChatDbError> {
        Ok(query!(self.db, "SELECT COALESCE(MAX(pin_sort_order), -1) AS value FROM sessions WHERE profile_id = ? AND pinned = TRUE", profile_id)
            .fetch_one::<I32ValueRow>()
            .await?
            .value)
    }

    // ── Scheduled Task CRUD ─────────────────────────────────

    /// Create a scheduled task.
    pub async fn create_scheduled_task(
        &self,
        params: CreateScheduledTaskParams<'_>,
    ) -> Result<scheduled_tasks::Model, ChatDbError> {
        let trigger_kind = params.trigger.kind().as_str();
        let trigger_type = params.trigger.storage_trigger_type();
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO scheduled_tasks (id, profile_id, session_id, name, trigger_kind, trigger_type, cron_expression, run_at, script_path, script_inline, needs_network, approved, enabled, last_run_at, last_run_status, last_run_output, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params.id,
            params.profile_id,
            params.session_id,
            params.name,
            trigger_kind,
            trigger_type,
            params.cron_expression,
            params.run_at,
            params.script_path,
            params.script_inline,
            params.needs_network,
            false,
            true,
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            now.clone(),
            now.clone()
        )
        .execute()
        .await?;
        Ok(scheduled_tasks::Model {
            id: params.id.to_string(),
            profile_id: params.profile_id.to_string(),
            session_id: params.session_id.map(ToOwned::to_owned),
            name: params.name.to_string(),
            trigger_kind: trigger_kind.to_string(),
            trigger_type: trigger_type.to_string(),
            cron_expression: params.cron_expression.map(ToOwned::to_owned),
            run_at: params.run_at.map(ToOwned::to_owned),
            script_path: params.script_path.map(ToOwned::to_owned),
            script_inline: params.script_inline.map(ToOwned::to_owned),
            needs_network: params.needs_network,
            approved: false,
            enabled: true,
            last_run_at: None,
            last_run_status: None,
            last_run_output: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Get a scheduled task by ID.
    pub async fn get_scheduled_task(
        &self,
        id: &str,
    ) -> Result<Option<scheduled_tasks::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM scheduled_tasks WHERE id = ?", id)
                .fetch_optional::<scheduled_tasks::Model>()
                .await?,
        )
    }

    /// List all scheduled tasks, optionally filtered by profile.
    pub async fn list_scheduled_tasks(
        &self,
        profile_id: Option<&str>,
    ) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        let sql = if profile_id.is_some() {
            "SELECT * FROM scheduled_tasks WHERE profile_id = ? ORDER BY created_at DESC"
        } else {
            "SELECT * FROM scheduled_tasks ORDER BY created_at DESC"
        };
        let rows = match profile_id {
            Some(profile_id) => {
                query!(self.db, sql, profile_id)
                    .fetch_all::<scheduled_tasks::Model>()
                    .await?
            }
            None => {
                query!(self.db, sql)
                    .fetch_all::<scheduled_tasks::Model>()
                    .await?
            }
        };
        Ok(rows)
    }

    /// Update a scheduled task's mutable fields.
    pub async fn update_scheduled_task(
        &self,
        id: &str,
        params: UpdateScheduledTaskParams<'_>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let name_enabled = params.name.is_some();
        let enabled_enabled = params.enabled.is_some();
        let approved_enabled = params.approved.is_some();
        let cron_enabled = params.cron_expression.is_some();
        let run_at_enabled = params.run_at.is_some();
        let script_path_enabled = params.script_path.is_some();
        let script_inline_enabled = params.script_inline.is_some();
        query!(
            self.db,
            "UPDATE scheduled_tasks SET updated_at = ?, name = CASE WHEN ? THEN ? ELSE name END, enabled = CASE WHEN ? THEN ? ELSE enabled END, approved = CASE WHEN ? THEN ? ELSE approved END, cron_expression = CASE WHEN ? THEN ? ELSE cron_expression END, run_at = CASE WHEN ? THEN ? ELSE run_at END, script_path = CASE WHEN ? THEN ? ELSE script_path END, script_inline = CASE WHEN ? THEN ? ELSE script_inline END WHERE id = ?",
            now,
            name_enabled,
            params.name,
            enabled_enabled,
            params.enabled.unwrap_or(false),
            approved_enabled,
            params.approved.unwrap_or(false),
            cron_enabled,
            params.cron_expression.flatten(),
            run_at_enabled,
            params.run_at.flatten(),
            script_path_enabled,
            params.script_path.flatten(),
            script_inline_enabled,
            params.script_inline.flatten(),
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Delete a scheduled task.
    pub async fn delete_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "DELETE FROM scheduled_tasks WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Approve a task for execution.
    pub async fn approve_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_tasks SET approved = TRUE, updated_at = ? WHERE id = ?",
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Record a task execution result.
    pub async fn record_task_run(
        &self,
        id: &str,
        status: TaskRunStatus,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_tasks SET last_run_at = ?, last_run_status = ?, last_run_output = ?, updated_at = ? WHERE id = ?",
            now.clone(),
            status.as_str(),
            output,
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    pub async fn start_task_run(
        &self,
        id: &str,
        task: &scheduled_tasks::Model,
        invocation_kind: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO scheduled_task_runs (id, task_id, profile_id, session_id, invocation_kind, status, output, started_at, finished_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            task.id.as_str(),
            task.profile_id.as_str(),
            task.session_id.as_deref(),
            invocation_kind,
            TaskRunStatus::Running.as_str(),
            Option::<String>::None,
            now.clone(),
            Option::<String>::None,
            now
        )
        .execute()
        .await?;
        Ok(())
    }

    pub async fn finish_task_run(
        &self,
        id: &str,
        status: TaskRunStatus,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_task_runs SET status = ?, output = ?, finished_at = ?, updated_at = ? WHERE id = ?",
            status.as_str(),
            output,
            Some(now.clone()),
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List enabled and approved tasks.
    ///
    /// Due-ness is resolved by the scheduler runtime because it depends on
    /// trigger-kind-specific rules.
    pub async fn list_due_tasks(&self) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM scheduled_tasks WHERE enabled = TRUE AND approved = TRUE"
        )
        .fetch_all::<scheduled_tasks::Model>()
        .await?)
    }

    pub async fn list_tasks_for_webhook(
        &self,
        token: &str,
    ) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM scheduled_tasks WHERE enabled = TRUE AND approved = TRUE AND trigger_kind = ? AND trigger_type = ?",
            TRIGGER_KIND_WEBHOOK,
            token
        )
        .fetch_all::<scheduled_tasks::Model>()
        .await?)
    }

    // ── Push subscriptions ────────────────────────────────

    /// Store a push subscription from a browser.
    pub async fn add_push_subscription(
        &self,
        id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
    ) -> Result<push_subscriptions::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO push_subscriptions (id, endpoint, p256dh, auth, created_at) VALUES (?, ?, ?, ?, ?)",
            id,
            endpoint,
            p256dh,
            auth,
            now.clone()
        )
        .execute()
        .await?;
        Ok(push_subscriptions::Model {
            id: id.to_string(),
            endpoint: endpoint.to_string(),
            p256dh: p256dh.to_string(),
            auth: auth.to_string(),
            created_at: now,
        })
    }

    /// Remove a push subscription by endpoint.
    pub async fn remove_push_subscription(&self, endpoint: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM push_subscriptions WHERE endpoint = ?",
            endpoint
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List all push subscriptions.
    pub async fn list_push_subscriptions(
        &self,
    ) -> Result<Vec<push_subscriptions::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM push_subscriptions ORDER BY created_at DESC"
        )
        .fetch_all::<push_subscriptions::Model>()
        .await?)
    }

    // ── Profile memories ──────────────────────────────────

    /// Get a profile memory by profile ID and type.
    pub async fn get_profile_memory(
        &self,
        profile_id: &str,
        memory_type: &str,
    ) -> Result<Option<profile_memories::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM profile_memories WHERE profile_id = ? AND memory_type = ? LIMIT 1",
            profile_id,
            memory_type
        )
        .fetch_optional::<profile_memories::Model>()
        .await?)
    }

    /// Get all memories for a profile.
    pub async fn get_profile_memories(
        &self,
        profile_id: &str,
    ) -> Result<Vec<profile_memories::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM profile_memories WHERE profile_id = ? ORDER BY updated_at DESC",
            profile_id
        )
        .fetch_all::<profile_memories::Model>()
        .await?)
    }

    /// Upsert a profile memory (insert or replace).
    pub async fn upsert_profile_memory(
        &self,
        profile_id: &str,
        memory_type: &str,
        content: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let existing = self.get_profile_memory(profile_id, memory_type).await?;

        if let Some(existing) = existing {
            query!(
                self.db,
                "UPDATE profile_memories SET content = ?, updated_at = ? WHERE id = ?",
                content,
                now,
                existing.id
            )
            .execute()
            .await?;
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            query!(
                self.db,
                "INSERT INTO profile_memories (id, profile_id, memory_type, content, updated_at) VALUES (?, ?, ?, ?, ?)",
                id,
                profile_id,
                memory_type,
                content,
                now
            )
            .execute()
            .await?;
        }

        Ok(())
    }

    /// Mark a profile memory refresh job dirty.
    pub async fn mark_profile_memory_job_dirty(&self, profile_id: &str) -> Result<(), ChatDbError> {
        let updated_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO profile_memory_jobs (profile_id, dirty, last_processed_at, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(profile_id) DO UPDATE SET dirty = excluded.dirty, updated_at = excluded.updated_at",
            profile_id,
            true,
            Option::<String>::None,
            updated_at
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List profile memory jobs still marked dirty.
    pub async fn list_dirty_profile_memory_jobs(
        &self,
    ) -> Result<Vec<profile_memory_jobs::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM profile_memory_jobs WHERE dirty = ? ORDER BY updated_at ASC",
            true
        )
        .fetch_all::<profile_memory_jobs::Model>()
        .await?)
    }

    /// Mark a profile memory refresh job processed.
    pub async fn mark_profile_memory_job_processed(
        &self,
        profile_id: &str,
        expected_updated_at: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE profile_memory_jobs SET dirty = ?, last_processed_at = ?, updated_at = ? WHERE profile_id = ? AND updated_at = ?",
            false,
            Some(now.clone()),
            now,
            profile_id,
            expected_updated_at
        )
        .execute()
        .await?;
        Ok(())
    }
}

/// Parameters for upserting a persisted session context snapshot.
pub struct SessionContextSnapshotUpsert {
    pub checkpoint: aither_agent::AgentCheckpoint,
    pub history_message_count: usize,
}

/// Structured snapshot loaded from SQL-backed session context storage.
#[derive(Debug, Clone)]
pub struct StoredSessionContextSnapshot {
    pub checkpoint: aither_agent::AgentCheckpoint,
    pub history_message_count: usize,
    pub updated_at: String,
}


/// Parameters for creating a scheduled task.
pub struct CreateScheduledTaskParams<'a> {
    pub id: &'a str,
    pub profile_id: &'a str,
    pub session_id: Option<&'a str>,
    pub name: &'a str,
    pub trigger: &'a PersistedTaskTrigger,
    pub cron_expression: Option<&'a str>,
    pub run_at: Option<&'a str>,
    pub script_path: Option<&'a str>,
    pub script_inline: Option<&'a str>,
    pub needs_network: bool,
}

/// Parameters for updating a scheduled task.
pub struct UpdateScheduledTaskParams<'a> {
    pub name: Option<&'a str>,
    pub enabled: Option<bool>,
    pub approved: Option<bool>,
    pub cron_expression: Option<Option<&'a str>>,
    pub run_at: Option<Option<&'a str>>,
    pub script_path: Option<Option<&'a str>>,
    pub script_inline: Option<Option<&'a str>>,
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
    use crate::profile::{ProfileEnvironment, ProfilePreferences, RemoteProfilePreferences};
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
    async fn creates_remote_profile_with_typed_environment() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "remote-profile",
                name: "Remote",
                environment: ProfileEnvironmentKind::Remote,
                container_image: None,
                slug: None,
                icon: None,
                persona: None,
                preferences: Some(&ProfilePreferences {
                    remote: Some(RemoteProfilePreferences {
                        ssh_server_id: "prod".to_string(),
                    }),
                    container: None,
                }),
                location_aware: false,
            })
            .await
            .expect("remote profile should be created");

        let info = db
            .profile_info_from_model(model)
            .await
            .expect("remote profile should deserialize");
        match info.environment.kind() {
            ProfileEnvironment::Remote { ssh_server_id } => assert_eq!(ssh_server_id, "prod"),
            other => panic!("expected remote environment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_remote_profile_without_ssh_server_binding() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        // Whitespace-only ssh_server_id should be rejected at creation
        // time by normalize_profile_preferences — we don't want the
        // invalid row to persist in the first place.
        let error = db
            .create_profile(CreateProfileParams {
                id: "remote-profile",
                name: "Remote",
                environment: ProfileEnvironmentKind::Remote,
                container_image: None,
                slug: None,
                icon: None,
                persona: None,
                preferences: Some(&ProfilePreferences {
                    remote: Some(RemoteProfilePreferences {
                        ssh_server_id: "  ".to_string(),
                    }),
                    container: None,
                }),
                location_aware: false,
            })
            .await
            .expect_err("invalid remote preferences must fail fast");
        assert!(
            error
                .to_string()
                .contains("preferences.remote.ssh_server_id"),
            "unexpected error: {error}"
        );
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
