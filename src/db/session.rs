//! Session CRUD, runtime state, pending-work queues, waiting flags, run
//! leases, and restart recovery queries on [`ChatDb`].
//!
//! Every `impl ChatDb` method in this file either reads or mutates
//! `sessions`, `session_runtimes`, `session_pending_permissions`,
//! `session_pending_workspace`, `session_plans`, `session_run_leases`,
//! or the profile-memory dirty-job table. Split out of db/mod.rs so
//! the session subsystem reads as one file instead of being buried in
//! the 5k-line monolith.

use std::collections::HashMap;

use crate::scheduler::TaskRunStatus;
use crate::session::{
    BrowserInteractionState, PendingUserMessage, PersistedAskUserRequest,
    PersistedPermissionRequest, PersistedWorkspaceRequest, SessionInteractionState,
    SessionPlanState, SessionRuntimeState, SessionRuntimeSummary, TerminalRuntimeState,
};
use crate::xml::XmlBlock;

use super::common::{
    I64ValueRow, PendingAskUserQuestionOptionRow, PendingAskUserQuestionRow,
    PendingAskUserRequestRow, PendingMessageAttachmentRow, PendingMessageEntryRow, SessionIdRow,
    db_custom_error, overflow_error, parse_attachment_kind, parse_pending_user_message_mode,
    parse_session_plan_state, parse_session_runtime_state, row_not_found,
    session_not_found_db_error, storage_error,
};
use super::entity::{
    session_pending_permissions, session_pending_workspace, session_plans, session_runtimes,
    sessions,
};
use super::{ChatDb, ChatDbError, query};

/// Grouped ask-user rows used when assembling a runtime summary: the request
/// a run waits on, and the questions left open without a waiter.
struct PendingAskUserParts {
    row: Option<PendingAskUserRequestRow>,
    open_questions: Vec<PersistedAskUserRequest>,
    questions: Vec<PendingAskUserQuestionRow>,
    options: Vec<PendingAskUserQuestionOptionRow>,
}

impl ChatDb {
    /// Create a new session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn create_session(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<sessions::Model, ChatDbError> {
        self.create_session_in_profile(id, title, model, None).await
    }

    /// Create a new session within a specific profile.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn create_session_in_profile(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
        profile_id: Option<&str>,
    ) -> Result<sessions::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO sessions (id, title, model, preview, message_count, profile_id, pinned, archived, folder_id, pin_sort_order, created_at, updated_at, reasoning_effort) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            title,
            model,
            Option::<String>::None,
            0_i64,
            profile_id,
            false,
            false,
            Option::<String>::None,
            Option::<i32>::None,
            now.clone(),
            now,
            Option::<String>::None
        )
        .execute()
        .await?;
        self.get_session(id)
            .await?
            .ok_or_else(|| session_not_found_db_error(id))
    }

    /// Get a session by ID.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn get_session(&self, id: &str) -> Result<Option<sessions::Model>, ChatDbError> {
        Ok(query!(self.db, "SELECT * FROM sessions WHERE id = ?", id)
            .fetch_optional::<sessions::Model>()
            .await?)
    }

    fn pending_user_message_from_parts(
        row: PendingMessageEntryRow,
        attachment_rows: Vec<PendingMessageAttachmentRow>,
    ) -> Result<PendingUserMessage, ChatDbError> {
        Ok(PendingUserMessage {
            content: row.content,
            attachments: attachment_rows
                .into_iter()
                .map(|attachment| {
                    Ok(crate::session::PersistedAttachment {
                        id: attachment.attachment_id,
                        name: attachment.name,
                        attachment_type: parse_attachment_kind(&attachment.attachment_type)?,
                        url: attachment.url,
                        upload_data_url: attachment.upload_data_url,
                        text_content: attachment.text_content,
                    })
                })
                .collect::<Result<Vec<_>, ChatDbError>>()?,
            hidden_context_xml: row.hidden_context_xml.map(XmlBlock::parse).transpose()?,
            created_at: row.created_at,
            delivery_mode: parse_pending_user_message_mode(&row.delivery_mode)?,
        })
    }

    fn persisted_permission_request_from_row(
        row: session_pending_permissions::Model,
    ) -> Result<PersistedPermissionRequest, ChatDbError> {
        Ok(PersistedPermissionRequest {
            id: row.request_id,
            request: crate::session::PersistedPermissionRequestPayload {
                mode: row.mode.parse().map_err(|error| {
                    storage_error(db_custom_error(
                        "invalid persisted permission request mode: ",
                        &error,
                    ))
                })?,
                script: row.script,
                session_id: Some(row.session_id),
            },
        })
    }

    fn persisted_workspace_request_from_row(
        row: session_pending_workspace::Model,
    ) -> PersistedWorkspaceRequest {
        PersistedWorkspaceRequest {
            id: row.request_id,
            request: crate::session::PersistedWorkspaceRequestPayload {
                path: row.path,
                reason: row.reason,
                session_id: Some(row.session_id),
            },
        }
    }

    pub(super) fn persisted_ask_user_request_from_parts(
        row: PendingAskUserRequestRow,
        question_rows: Vec<PendingAskUserQuestionRow>,
        option_rows: Vec<PendingAskUserQuestionOptionRow>,
    ) -> PersistedAskUserRequest {
        let mut option_rows_by_question = option_rows.into_iter().fold(
            std::collections::BTreeMap::<i64, Vec<PendingAskUserQuestionOptionRow>>::new(),
            |mut grouped, row| {
                grouped.entry(row.question_ordinal).or_default().push(row);
                grouped
            },
        );
        PersistedAskUserRequest {
            id: row.request_id,
            questions: question_rows
                .into_iter()
                .map(|question| crate::session::PersistedAskUserQuestion {
                    section: question.section,
                    question: question.question,
                    option: option_rows_by_question
                        .remove(&question.ordinal)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|option| option.value)
                        .collect(),
                    multi_select: question.multi_select,
                })
                .collect(),
        }
    }

    fn build_session_runtime_summary(
        runtime_row: Option<session_runtimes::Model>,
        message_rows: Vec<PendingMessageEntryRow>,
        attachment_rows: Vec<PendingMessageAttachmentRow>,
        permission_row: Option<session_pending_permissions::Model>,
        workspace_row: Option<session_pending_workspace::Model>,
        ask_user: PendingAskUserParts,
        plan_row: Option<&session_plans::Model>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let mut attachments_by_message = attachment_rows.into_iter().fold(
            std::collections::BTreeMap::<i64, Vec<PendingMessageAttachmentRow>>::new(),
            |mut grouped, row| {
                grouped.entry(row.message_id).or_default().push(row);
                grouped
            },
        );
        let mut steer_messages = Vec::new();
        let mut queued_messages = Vec::new();
        for row in message_rows {
            let message = Self::pending_user_message_from_parts(
                row.clone(),
                attachments_by_message.remove(&row.id).unwrap_or_default(),
            )?;
            if message.delivery_mode == crate::session::PendingUserMessageMode::Steer {
                steer_messages.push(message);
            } else {
                queued_messages.push(message);
            }
        }
        // The newest steer message (last by id) becomes pending_immediate.
        // Older steers overflow into the front of the queue.
        let pending_immediate = if steer_messages.is_empty() {
            None
        } else {
            Some(steer_messages.pop().expect("non-empty"))
        };
        // Insert remaining steers at the front of the queue, preserving their order
        queued_messages.splice(0..0, steer_messages);

        let mut summary = SessionRuntimeSummary {
            state: parse_session_runtime_state(
                runtime_row
                    .as_ref()
                    .map_or(SessionRuntimeState::Idle.as_str(), |row| row.state.as_str()),
            )?,
            plan_state: parse_session_plan_state(
                plan_row.map_or(SessionPlanState::Disabled.as_str(), |row| {
                    row.plan_state.as_str()
                }),
            )?,
            interactions: SessionInteractionState {
                browser: BrowserInteractionState {
                    takeover_active: runtime_row
                        .as_ref()
                        .is_some_and(|row| row.browser_takeover_active),
                    takeover_alert_pending: runtime_row
                        .as_ref()
                        .is_some_and(|row| row.browser_takeover_alert_pending),
                },
                terminal: TerminalRuntimeState {
                    task_id: runtime_row
                        .as_ref()
                        .and_then(|row| row.terminal_task_id.clone()),
                    output_file: runtime_row
                        .as_ref()
                        .and_then(|row| row.terminal_output_file.clone()),
                    output_preview: runtime_row
                        .as_ref()
                        .and_then(|row| row.terminal_output_preview.clone()),
                    input_notice: runtime_row
                        .as_ref()
                        .and_then(|row| row.terminal_input_notice.clone()),
                },
            },
            pending_immediate,
            queued_messages,
            waiting_permission: permission_row
                .map(Self::persisted_permission_request_from_row)
                .transpose()?,
            waiting_workspace: workspace_row.map(Self::persisted_workspace_request_from_row),
            waiting_ask_user: ask_user.row.map(|row| {
                Self::persisted_ask_user_request_from_parts(
                    row,
                    ask_user.questions.clone(),
                    ask_user.options.clone(),
                )
            }),
            open_questions: ask_user.open_questions,
            updated_at: runtime_row.map(|row| row.updated_at),
            ..Default::default()
        };
        summary.normalize_state();
        Ok(summary)
    }

    /// Get the persisted runtime summary for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn get_session_runtime(
        &self,
        id: &str,
    ) -> Result<Option<SessionRuntimeSummary>, ChatDbError> {
        let session = self.get_session(id).await?;
        if session.is_none() {
            return Ok(None);
        }
        Ok(Some(self.load_session_runtime_summary(id).await?))
    }

    pub(super) async fn load_session_runtime_summary(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let runtime_row = query!(
            self.db,
            "SELECT * FROM session_runtimes WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<session_runtimes::Model>()
        .await?;
        let message_rows = query!(
            self.db,
            "SELECT * FROM session_pending_message_entries WHERE session_id = ? ORDER BY id ASC",
            session_id
        )
        .fetch_all::<PendingMessageEntryRow>()
        .await?;
        let attachment_rows = query!(
            self.db,
            "SELECT attachments.* FROM session_pending_message_attachments AS attachments INNER JOIN session_pending_message_entries AS entries ON entries.id = attachments.message_id WHERE entries.session_id = ? ORDER BY attachments.message_id ASC, attachments.ordinal ASC",
            session_id
        )
        .fetch_all::<PendingMessageAttachmentRow>()
        .await?;
        let permission_row = query!(
            self.db,
            "SELECT * FROM session_pending_permissions WHERE session_id = ? LIMIT 1",
            session_id
        )
        .fetch_optional::<session_pending_permissions::Model>()
        .await?;
        let workspace_row = query!(
            self.db,
            "SELECT * FROM session_pending_workspace WHERE session_id = ? LIMIT 1",
            session_id
        )
        .fetch_optional::<session_pending_workspace::Model>()
        .await?;
        let ask_user_row = query!(
            self.db,
            "SELECT * FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1 LIMIT 1",
            session_id
        )
        .fetch_optional::<PendingAskUserRequestRow>()
        .await?;
        let ask_user_question_rows = if let Some(request) = ask_user_row.as_ref() {
            query!(
                self.db,
                "SELECT * FROM session_pending_ask_user_questions_v2 WHERE request_id = ? ORDER BY ordinal ASC",
                request.request_id.as_str()
            )
            .fetch_all::<PendingAskUserQuestionRow>()
            .await?
        } else {
            Vec::new()
        };
        let ask_user_option_rows = if let Some(request) = ask_user_row.as_ref() {
            query!(
                self.db,
                "SELECT * FROM session_pending_ask_user_question_options_v2 WHERE request_id = ? ORDER BY question_ordinal ASC, option_ordinal ASC",
                request.request_id.as_str()
            )
            .fetch_all::<PendingAskUserQuestionOptionRow>()
            .await?
        } else {
            Vec::new()
        };
        let plan_row = query!(
            self.db,
            "SELECT * FROM session_plans WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<session_plans::Model>()
        .await?;
        Self::build_session_runtime_summary(
            runtime_row,
            message_rows,
            attachment_rows,
            permission_row,
            workspace_row,
            PendingAskUserParts {
                row: ask_user_row,
                open_questions: self.open_ask_user_questions(session_id).await?,
                questions: ask_user_question_rows,
                options: ask_user_option_rows,
            },
            plan_row.as_ref(),
        )
    }

    /// List session runtime summaries.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn list_session_runtime_summaries(
        &self,
        session_ids: &[String],
    ) -> Result<HashMap<String, SessionRuntimeSummary>, ChatDbError> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut summaries = HashMap::with_capacity(session_ids.len());
        for session_id in session_ids {
            summaries.insert(
                session_id.clone(),
                self.load_session_runtime_summary(session_id).await?,
            );
        }

        Ok(summaries)
    }

    /// Set the base runtime state for a session with a direct SQL update.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_session_runtime_state(
        &self,
        session_id: &str,
        state: SessionRuntimeState,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO session_runtimes (session_id, state, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, ?, 0, 0, ?) ON CONFLICT(session_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            session_id,
            state.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Set only the terminal interaction fields via direct SQL update.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    ///
    /// # Panics
    ///
    /// Panics if `BackgroundReason` fails to serialize — the type is
    /// serde-derived, so this cannot happen.
    pub async fn set_terminal_interaction(
        &self,
        session_id: &str,
        terminal: &TerminalRuntimeState,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO session_runtimes (session_id, state, browser_takeover_active, browser_takeover_alert_pending, terminal_task_id, terminal_output_file, terminal_output_preview, terminal_input_notice, updated_at) VALUES (?, 'idle', 0, 0, ?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET terminal_task_id = excluded.terminal_task_id, terminal_output_file = excluded.terminal_output_file, terminal_output_preview = excluded.terminal_output_preview, terminal_input_notice = excluded.terminal_input_notice, updated_at = excluded.updated_at",
            session_id,
            terminal.task_id.as_deref(),
            terminal.output_file.as_deref(),
            terminal.output_preview.as_deref(),
            terminal.input_notice.as_deref(),
            now.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Set session plan state.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_session_plan_state(
        &self,
        session_id: &str,
        plan_state: SessionPlanState,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO session_plans (session_id, plan_state, updated_at) VALUES (?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET plan_state = excluded.plan_state, updated_at = excluded.updated_at",
            session_id,
            plan_state.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Enqueue a pending-immediate (steer) message. Existing steer messages
    /// remain in the DB with their original `delivery_mode` — the
    /// `build_session_runtime_summary` method sorts steer messages by recency
    /// and only promotes the newest one to `pending_immediate`, keeping older
    /// steers as overflow in the queue.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_pending_immediate_message(
        &self,
        session_id: &str,
        message: PendingUserMessage,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let mut tx = self.db.begin().await?;
        self.insert_pending_message_in_tx(&mut tx, session_id, &message)
            .await?;
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Append a queued message via direct SQL insert.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn push_queued_message(
        &self,
        session_id: &str,
        message: PendingUserMessage,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let mut tx = self.db.begin().await?;
        self.insert_pending_message_in_tx(&mut tx, session_id, &message)
            .await?;
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Insert a pending message and its attachments within an existing transaction.
    pub(super) async fn insert_pending_message_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        message: &PendingUserMessage,
    ) -> Result<(), ChatDbError> {
        query!(
            tx,
            "INSERT INTO session_pending_message_entries (session_id, delivery_mode, content, hidden_context_xml, created_at) VALUES (?, ?, ?, ?, ?)",
            session_id,
            message.delivery_mode.as_str(),
            message.content.as_str(),
            message.hidden_context_xml.clone().map(XmlBlock::into_inner),
            message.created_at.as_str()
        )
        .execute()
        .await?;
        let message_id = query!(tx, "SELECT last_insert_rowid() AS value")
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        for (ordinal, attachment) in message.attachments.iter().enumerate() {
            query!(
                tx,
                "INSERT INTO session_pending_message_attachments (message_id, ordinal, attachment_id, name, attachment_type, url, upload_data_url, text_content) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                message_id,
                i64::try_from(ordinal).map_err(|error| overflow_error("pending attachment ordinal", &error))?,
                attachment.id.as_str(),
                attachment.name.as_str(),
                attachment.attachment_type.as_str(),
                attachment.url.as_str(),
                attachment.upload_data_url.as_deref(),
                attachment.text_content.as_deref()
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    /// Atomically dequeue the next pending message.
    /// Priority: newest steer first, then oldest queued.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn take_next_pending_message(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        // The transaction opens with the delete so it takes SQLite's write
        // lock up front: a read first would make the later write a lock
        // upgrade, which SQLite refuses with `SQLITE_BUSY` once another
        // connection has written in the meantime.
        // Steer messages first (newest steer = highest priority), then queue (oldest first).
        // Two-level sort: steer before queue, then newest steer or oldest queue.
        let row = query!(
            tx,
            "DELETE FROM session_pending_message_entries WHERE id = (SELECT id FROM session_pending_message_entries WHERE session_id = ? ORDER BY CASE delivery_mode WHEN 'steer' THEN 0 ELSE 1 END, CASE delivery_mode WHEN 'steer' THEN -id ELSE id END LIMIT 1) RETURNING *",
            session_id
        )
        .fetch_optional::<PendingMessageEntryRow>()
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let row_id = row.id;
        let attachment_rows = query!(
            tx,
            "SELECT * FROM session_pending_message_attachments WHERE message_id = ? ORDER BY ordinal ASC",
            row_id
        )
        .fetch_all::<PendingMessageAttachmentRow>()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_attachments WHERE message_id = ?",
            row_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(Some(Self::pending_user_message_from_parts(
            row,
            attachment_rows,
        )?))
    }

    /// Remove and return the next pending message only when follow-up execution is allowed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn take_next_dispatchable_pending_message(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, ChatDbError> {
        // Check dispatchability atomically by reading the runtime state
        let summary = self.load_session_runtime_summary(session_id).await?;
        if !summary.can_dispatch_follow_up() {
            return Ok(None);
        }
        self.take_next_pending_message(session_id).await
    }

    /// Remove all steer (pending-immediate) messages for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn clear_pending_immediate_message(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_attachments WHERE message_id IN (SELECT id FROM session_pending_message_entries WHERE session_id = ? AND delivery_mode = 'steer')",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_entries WHERE session_id = ? AND delivery_mode = 'steer'",
            session_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Remove every pending message (steer and queued) for a session.
    /// Used when a conversation rewind discards the timeline those
    /// messages belonged to.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn clear_pending_messages(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_attachments WHERE message_id IN (SELECT id FROM session_pending_message_entries WHERE session_id = ?)",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_entries WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Remove a queued message by its position in the queue.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn remove_queued_message_at(
        &self,
        session_id: &str,
        index: usize,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        // Find the row ID for the message at this index position
        // Queue messages are non-steer, ordered by creation time
        let rows = query!(
            self.db,
            "SELECT id FROM session_pending_message_entries WHERE session_id = ? AND delivery_mode != 'steer' ORDER BY created_at ASC",
            session_id
        )
        .fetch_all::<I64ValueRow>()
        .await?;
        let row = rows.get(index).ok_or_else(|| {
            row_not_found(format!(
                "Queued message index {index} out of range for session {session_id}"
            ))
        })?;
        let row_id = row.value;
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_attachments WHERE message_id = ?",
            row_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_entries WHERE id = ?",
            row_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Persist or clear a waiting-permission request via direct SQL.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_waiting_permission(
        &self,
        session_id: &str,
        request: Option<PersistedPermissionRequest>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "DELETE FROM session_pending_permissions WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        if let Some(request) = &request {
            query!(
                tx,
                "INSERT INTO session_pending_permissions (request_id, session_id, mode, script, created_at) VALUES (?, ?, ?, ?, ?)",
                request.id.as_str(),
                session_id,
                request.request.mode.as_str(),
                request.request.script.as_str(),
                now.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Clear a single waiting-permission row by its `request_id`.
    ///
    /// Use this when responding to a specific permission prompt so a fresh row
    /// inserted by the resumed agent for the *next* permission isn't wiped by a
    /// session-wide delete.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn clear_waiting_permission_request(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        query!(
            self.db,
            "DELETE FROM session_pending_permissions WHERE request_id = ?",
            request_id
        )
        .execute()
        .await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Persist or clear a waiting workspace request via direct SQL.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_waiting_workspace(
        &self,
        session_id: &str,
        request: Option<PersistedWorkspaceRequest>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "DELETE FROM session_pending_workspace WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        if let Some(request) = &request {
            query!(
                tx,
                "INSERT INTO session_pending_workspace (request_id, session_id, path, reason, created_at) VALUES (?, ?, ?, ?, ?)",
                request.id.as_str(),
                session_id,
                request.request.path.as_str(),
                request.request.reason.as_str(),
                now.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Persist or clear a waiting ask-user request via direct SQL.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_waiting_ask_user(
        &self,
        session_id: &str,
        request: Option<PersistedAskUserRequest>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        // Clear existing
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1)",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1)",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1",
            session_id
        )
        .execute()
        .await?;
        // Insert new if present
        if let Some(request) = &request {
            query!(
                tx,
                "INSERT INTO session_pending_ask_user_requests_v2 (request_id, session_id, created_at, blocking) VALUES (?, ?, ?, 1)",
                request.id.as_str(),
                session_id,
                now.as_str()
            )
            .execute()
            .await?;
            for (question_ordinal, question) in request.questions.iter().enumerate() {
                let question_ordinal = i64::try_from(question_ordinal)
                    .map_err(|error| overflow_error("ask-user question ordinal", &error))?;
                query!(
                    tx,
                    "INSERT INTO session_pending_ask_user_questions_v2 (request_id, ordinal, section, question, multi_select) VALUES (?, ?, ?, ?, ?)",
                    request.id.as_str(),
                    question_ordinal,
                    question.section.as_str(),
                    question.question.as_str(),
                    question.multi_select
                )
                .execute()
                .await?;
                for (option_ordinal, option) in question.option.iter().enumerate() {
                    query!(
                        tx,
                        "INSERT INTO session_pending_ask_user_question_options_v2 (request_id, question_ordinal, option_ordinal, value) VALUES (?, ?, ?, ?)",
                        request.id.as_str(),
                        question_ordinal,
                        i64::try_from(option_ordinal).map_err(|error| overflow_error("ask-user option ordinal", &error))?,
                        option.as_str()
                    )
                    .execute()
                    .await?;
                }
            }
        }
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Set browser takeover to this session. Clears takeover from OTHER sessions only.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn set_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        // Clear takeover from OTHER sessions (scoped — not global!)
        query!(
            tx,
            "UPDATE session_runtimes SET browser_takeover_active = 0, updated_at = ? WHERE session_id != ? AND browser_takeover_active = 1",
            now.as_str(),
            session_id
        )
        .execute()
        .await?;
        // Set takeover on this session
        query!(
            tx,
            "INSERT INTO session_runtimes (session_id, state, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, 'running', 1, 0, ?) ON CONFLICT(session_id) DO UPDATE SET browser_takeover_active = 1, browser_takeover_alert_pending = 0, updated_at = excluded.updated_at",
            session_id,
            now.as_str()
        )
        .execute()
        .await?;
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Clear browser takeover and set alert pending via direct SQL.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn clear_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        // Atomic conditional update: only set alert_pending if takeover was active
        query!(
            self.db,
            "UPDATE session_runtimes SET browser_takeover_active = 0, browser_takeover_alert_pending = 1, updated_at = ? WHERE session_id = ? AND browser_takeover_active = 1",
            now.as_str(),
            session_id
        )
        .execute()
        .await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Atomically consume the browser takeover alert pending flag.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn take_browser_takeover_alert_pending(
        &self,
        session_id: &str,
    ) -> Result<bool, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let result = query!(
            self.db,
            "UPDATE session_runtimes SET browser_takeover_alert_pending = 0, updated_at = ? WHERE session_id = ? AND browser_takeover_alert_pending = 1",
            now.as_str(),
            session_id
        )
        .execute()
        .await?;
        Ok(result.rows_written > 0)
    }

    /// Session has browser takeover.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn session_has_browser_takeover(
        &self,
        session_id: &str,
    ) -> Result<bool, ChatDbError> {
        let Some(summary) = self.get_session_runtime(session_id).await? else {
            return Ok(false);
        };
        Ok(summary.interactions.browser.takeover_active)
    }

    /// Find browser takeover session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn find_browser_takeover_session(&self) -> Result<Option<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT session_id FROM session_runtimes WHERE browser_takeover_active = 1 LIMIT 1"
        )
        .fetch_optional::<SessionIdRow>()
        .await?
        .map(|row| row.session_id))
    }

    /// Check if a session exists in the database.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn session_exists(&self, id: &str) -> Result<bool, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT id AS session_id FROM sessions WHERE id = ? LIMIT 1",
            id
        )
        .fetch_optional::<SessionIdRow>()
        .await?
        .is_some())
    }

    /// Update session metadata (title, model, and/or reasoning effort).
    ///
    /// `reasoning_effort` uses a tri-state patch: `None` leaves the column
    /// untouched, `Some(None)` clears it to NULL, `Some(Some(value))` sets it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn update_session(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<Option<&str>>,
    ) -> Result<(), ChatDbError> {
        let mut existing = self
            .get_session(id)
            .await?
            .ok_or_else(|| row_not_found(format!("Session not found: {id}")))?;
        if let Some(title) = title {
            existing.title = Some(title.to_string());
        }
        if let Some(model) = model {
            existing.model = Some(model.to_string());
        }
        if let Some(effort_patch) = reasoning_effort {
            existing.reasoning_effort = effort_patch.map(ToString::to_string);
        }
        existing.updated_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE sessions SET title = ?, model = ?, preview = ?, message_count = ?, profile_id = ?, pinned = ?, archived = ?, folder_id = ?, pin_sort_order = ?, created_at = ?, updated_at = ?, reasoning_effort = ? WHERE id = ?",
            existing.title.as_deref(),
            existing.model.as_deref(),
            existing.preview.as_deref(),
            existing.message_count,
            existing.profile_id.as_deref(),
            existing.pinned,
            existing.archived,
            existing.folder_id.as_deref(),
            existing.pin_sort_order,
            existing.created_at.as_str(),
            existing.updated_at.as_str(),
            existing.reasoning_effort.as_deref(),
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Delete a session and all its messages (CASCADE).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn delete_session(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "DELETE FROM messages WHERE session_id = ?", id)
            .execute()
            .await?;
        self.delete_session_ask_user_requests(Some(id)).await?;
        self.delete_session_context_snapshot(id).await?;
        query!(self.db, "DELETE FROM sessions WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Delete all sessions.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn delete_all_sessions(&self) -> Result<u64, ChatDbError> {
        let result = query!(self.db, "DELETE FROM messages").execute().await?;
        self.delete_session_ask_user_requests(None).await?;
        let snapshot_result = query!(self.db, "DELETE FROM session_context_snapshots")
            .execute()
            .await?;
        let session_result = query!(self.db, "DELETE FROM sessions").execute().await?;
        Ok(result.rows_written + snapshot_result.rows_written + session_result.rows_written)
    }

    /// List all sessions ordered by `updated_at` descending.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn list_sessions(&self) -> Result<Vec<sessions::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM sessions ORDER BY updated_at DESC")
                .fetch_all::<sessions::Model>()
                .await?,
        )
    }

    /// Repair persisted runtime state after a server restart.
    ///
    /// Only durable state is repaired; live resources such as streams,
    /// process handles, and SSH connections are rebuilt lazily. Leases whose
    /// holder stopped renewing before `expired_before_ms` are freed, and
    /// sessions no live lease covers get field-level fixes: a `running`
    /// state becomes `idle` (waiting states are derived from the pending
    /// request rows) with its terminal interaction cleared, and an active
    /// browser takeover ends with its alert pending. A session whose lease a
    /// live run still holds, a TUI process sharing this database for
    /// instance, is left to that run.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn recover_session_runtimes_after_restart(
        &self,
        expired_before_ms: i64,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "UPDATE session_run_leases SET owner = NULL, cancellation_requested = 0 WHERE owner IS NOT NULL AND heartbeat_at < ?",
            expired_before_ms
        )
        .execute()
        .await?;
        query!(
            tx,
            "UPDATE session_runtimes SET browser_takeover_active = 0, browser_takeover_alert_pending = 1, updated_at = ? WHERE browser_takeover_active = 1 AND NOT EXISTS (SELECT 1 FROM session_run_leases AS lease WHERE lease.session_id = session_runtimes.session_id AND lease.owner IS NOT NULL)",
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        query!(
            tx,
            "UPDATE session_runtimes SET state = 'idle', terminal_task_id = NULL, terminal_output_file = NULL, terminal_output_preview = NULL, terminal_input_notice = NULL, terminal_background_reason = NULL, updated_at = ? WHERE state = 'running' AND NOT EXISTS (SELECT 1 FROM session_run_leases AS lease WHERE lease.session_id = session_runtimes.session_id AND lease.owner IS NOT NULL)",
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Recover task runs after restart.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn recover_task_runs_after_restart(&self) -> Result<(), ChatDbError> {
        let finished_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_task_runs SET status = ?, output = ?, finished_at = ?, updated_at = ? WHERE status = ?",
            TaskRunStatus::Interrupted.as_str(),
            "May server restarted before the task run completed",
            Some(finished_at.as_str()),
            finished_at.as_str(),
            TaskRunStatus::Running.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Return session ids that have persisted follow-up work ready to resume.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn sessions_requiring_pending_dispatch(&self) -> Result<Vec<String>, ChatDbError> {
        let sessions = self.list_sessions().await?;
        let mut session_ids = Vec::new();
        for session in sessions {
            let summary = self.load_session_runtime_summary(&session.id).await?;
            if summary.can_dispatch_follow_up() {
                session_ids.push(session.id);
            }
        }
        Ok(session_ids)
    }

    /// Update session stats after inserting a message row, inside `tx`.
    pub(super) async fn update_session_stats_in(
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        updated_at: &str,
        preview: Option<&str>,
        increment_count: bool,
    ) -> Result<(), ChatDbError> {
        let sql = match (preview.is_some(), increment_count) {
            (true, true) => {
                "UPDATE sessions SET updated_at = ?, preview = ?, message_count = message_count + 1 WHERE id = ?"
            }
            (true, false) => "UPDATE sessions SET updated_at = ?, preview = ? WHERE id = ?",
            (false, true) => {
                "UPDATE sessions SET updated_at = ?, message_count = message_count + 1 WHERE id = ?"
            }
            (false, false) => "UPDATE sessions SET updated_at = ? WHERE id = ?",
        };
        match preview {
            Some(preview) => {
                query!(tx, sql, updated_at, preview, session_id)
                    .execute()
                    .await?;
            }
            None => {
                query!(tx, sql, updated_at, session_id).execute().await?;
            }
        }
        Ok(())
    }
}
