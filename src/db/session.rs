//! Session CRUD, runtime state, pending-work queues, waiting flags, run
//! leases, and restart recovery queries on [`ChatDb`].
//!
//! Every `impl ChatDb` method in this file either reads or mutates
//! `sessions`, `session_runtimes`, `session_pending_permissions`,
//! `session_pending_workspace`, `session_plans`, `session_runtimes`,
//! or the profile-memory dirty-job table. Split out of db/mod.rs so
//! the session subsystem reads as one file instead of being buried in
//! the 5k-line monolith.

use super::RunFence;
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
    session_yolo, sessions,
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

/// The per-session switch rows a runtime summary reads.
#[derive(Clone, Copy)]
struct SessionModeRows<'a> {
    plan: Option<&'a session_plans::Model>,
    yolo: Option<&'a session_yolo::Model>,
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

    /// Records the host directory `id` works in.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the update fails.
    pub async fn set_session_cwd(&self, id: &str, cwd: &str) -> Result<(), ChatDbError> {
        query!(self.db, "UPDATE sessions SET cwd = ? WHERE id = ?", cwd, id)
            .execute()
            .await?;
        Ok(())
    }

    /// Bind `id` to a repository identity (`github:owner/repo` or a local
    /// repository key), or clear the binding with `None`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the update fails.
    pub async fn set_session_repo(&self, id: &str, repo: Option<&str>) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET repo = ? WHERE id = ?",
            repo,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Marks `id` as the session importing `source`'s global rules (its
    /// [`crate::session::import::ForeignAgent::label`]).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the update fails.
    pub async fn set_session_rules_import(
        &self,
        id: &str,
        source: crate::session::import::ForeignAgent,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET rules_import = ? WHERE id = ?",
            source.label(),
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Marks `id` as a session nobody answers prompts for.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the update fails.
    pub async fn set_session_non_interactive(&self, id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET non_interactive = 1 WHERE id = ?",
            id
        )
        .execute()
        .await?;
        Ok(())
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
            id: Some(row.id),
            state: row.state.parse().map_err(storage_error)?,
            error: row.claim_error,
        })
    }

    fn persisted_permission_request_from_row(
        row: session_pending_permissions::Model,
    ) -> Result<PersistedPermissionRequest, ChatDbError> {
        Ok(PersistedPermissionRequest {
            id: row.request_id,
            request: crate::session::PersistedPermissionRequestPayload::new(
                row.mode.parse().map_err(|error| {
                    storage_error(db_custom_error(
                        "invalid persisted permission request mode: ",
                        &error,
                    ))
                })?,
                row.script,
                Some(row.session_id),
                row.agent_id,
            ),
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
                    secret: question.secret_name.map(|name| {
                        let scope = question
                            .secret_scope
                            .as_deref()
                            .and_then(crate::secrets::SecretScope::parse)
                            .unwrap_or_else(|| {
                                panic!("pending secret question {name} has no readable scope")
                            });
                        crate::session::PersistedSecretRequest { name, scope }
                    }),
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
        mode_rows: SessionModeRows<'_>,
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
                mode_rows
                    .plan
                    .map_or(SessionPlanState::Disabled.as_str(), |row| {
                        row.plan_state.as_str()
                    }),
            )?,
            yolo: mode_rows.yolo.is_some_and(|row| row.enabled),
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
            "SELECT * FROM session_pending_message_entries WHERE session_id = ? AND state != 'committed' ORDER BY id ASC",
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
        let yolo_row = self.session_yolo_row(session_id).await?;
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
            SessionModeRows {
                plan: plan_row.as_ref(),
                yolo: yolo_row.as_ref(),
            },
        )
    }

    async fn session_yolo_row(
        &self,
        session_id: &str,
    ) -> Result<Option<session_yolo::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM session_yolo WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<session_yolo::Model>()
        .await?)
    }

    /// Whether the session's yolo switch is on.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn session_yolo(&self, session_id: &str) -> Result<bool, ChatDbError> {
        Ok(self
            .session_yolo_row(session_id)
            .await?
            .is_some_and(|row| row.enabled))
    }

    /// Set the session's yolo switch.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn set_session_yolo(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO session_yolo (session_id, enabled, updated_at) VALUES (?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
            session_id,
            enabled,
            now.as_str()
        )
        .execute()
        .await?;
        self.load_session_runtime_summary(session_id).await
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

    /// Atomically claim the next pending message for `fence`'s run.
    /// Priority: newest steer first, then oldest queued. The entry stays in
    /// the queue marked `claimed` until its run commits it with the user
    /// message append — a failed preparation or a dead run returns it to
    /// `pending`, never silently drops it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn claim_next_pending_message(
        &self,
        fence: &RunFence,
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
            "UPDATE session_pending_message_entries SET state = 'claimed', claimed_run_id = ? WHERE id = (SELECT id FROM session_pending_message_entries WHERE session_id = ? AND state = 'pending' ORDER BY CASE delivery_mode WHEN 'steer' THEN 0 ELSE 1 END, CASE delivery_mode WHEN 'steer' THEN -id ELSE id END LIMIT 1) AND state = 'pending' RETURNING *",
            fence.run_id.as_str(),
            session_id
        )
        .fetch_optional::<PendingMessageEntryRow>()
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let attachment_rows = query!(
            tx,
            "SELECT * FROM session_pending_message_attachments WHERE message_id = ? ORDER BY ordinal ASC",
            row.id
        )
        .fetch_all::<PendingMessageAttachmentRow>()
        .await?;
        tx.commit().await?;
        Ok(Some(Self::pending_user_message_from_parts(
            row,
            attachment_rows,
        )?))
    }

    /// Return a claimed entry to `pending` when the run that claimed it did
    /// not commit it — a preparation failure hands it back untouched.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn release_pending_claim(
        &self,
        entry_id: i64,
        fence: &RunFence,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE session_pending_message_entries SET state = 'pending', claimed_run_id = NULL WHERE id = ? AND claimed_run_id = ? AND state = 'claimed'",
            entry_id,
            fence.run_id.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Mark a claimed entry `failed`, recording why: only a non-retryable
    /// error ends a claim this way, and the row stays visible to the UI.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn fail_pending_claim(
        &self,
        entry_id: i64,
        fence: &RunFence,
        error: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE session_pending_message_entries SET state = 'failed', claim_error = ? WHERE id = ? AND claimed_run_id = ? AND state = 'claimed'",
            error,
            entry_id,
            fence.run_id.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Remove and return the next pending message only when follow-up execution is allowed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or malformed rows.
    pub async fn take_next_dispatchable_pending_message(
        &self,
        fence: &RunFence,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, ChatDbError> {
        // Check dispatchability atomically by reading the runtime state
        let summary = self.load_session_runtime_summary(session_id).await?;
        if !summary.can_dispatch_follow_up() {
            return Ok(None);
        }
        self.claim_next_pending_message(fence, session_id).await
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
                "INSERT INTO session_pending_permissions (request_id, session_id, mode, script, created_at, agent_id) VALUES (?, ?, ?, ?, ?, ?)",
                request.id.as_str(),
                session_id,
                request.request.mode.as_str(),
                request.request.script.as_str(),
                now.as_str(),
                request.request.agent_id.as_deref()
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

    /// Delete the pending ask-user rows of `session_id` marked `blocking`:
    /// the request rows and their question/option children.
    pub(super) async fn delete_blocking_ask_user_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
    ) -> Result<(), ChatDbError> {
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
        Ok(())
    }

    /// Insert a pending ask-user request row with its questions and options.
    /// `blocking` marks whether a run waits on the prompt (`1`) or the
    /// question stays open without a waiter (`0`).
    pub(super) async fn insert_pending_ask_user_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        request: &PersistedAskUserRequest,
        blocking: bool,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            tx,
            "INSERT INTO session_pending_ask_user_requests_v2 (request_id, session_id, created_at, blocking) VALUES (?, ?, ?, ?)",
            request.id.as_str(),
            session_id,
            now.as_str(),
            blocking
        )
        .execute()
        .await?;
        for (question_ordinal, question) in request.questions.iter().enumerate() {
            let question_ordinal = i64::try_from(question_ordinal)
                .map_err(|error| overflow_error("ask-user question ordinal", &error))?;
            query!(
                tx,
                "INSERT INTO session_pending_ask_user_questions_v2 (request_id, ordinal, section, question, multi_select, secret_name, secret_scope) VALUES (?, ?, ?, ?, ?, ?, ?)",
                request.id.as_str(),
                question_ordinal,
                question.section.as_str(),
                question.question.as_str(),
                question.multi_select,
                question.secret.as_ref().map(|secret| secret.name.as_str()),
                question.secret.as_ref().map(|secret| secret.scope.as_str())
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
        Ok(())
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
        let mut tx = self.db.begin().await?;
        self.delete_blocking_ask_user_in_tx(&mut tx, session_id)
            .await?;
        if let Some(request) = &request {
            self.insert_pending_ask_user_in_tx(&mut tx, session_id, request, true)
                .await?;
        }
        tx.commit().await?;
        self.load_session_runtime_summary(session_id).await
    }

    /// Register a blocking ask-user request atomically: the session's
    /// previous blocking rows are replaced by `request` and the request
    /// joins the message history, all in one transaction.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn register_ask_user_request(
        &self,
        session_id: &str,
        request: &PersistedAskUserRequest,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let mut tx = self.db.begin().await?;
        self.delete_blocking_ask_user_in_tx(&mut tx, session_id)
            .await?;
        self.insert_pending_ask_user_in_tx(&mut tx, session_id, request, true)
            .await?;
        self.append_ask_user_request_message_in_tx(
            &mut tx,
            session_id,
            request.id.as_str(),
            &super::open_questions::history_questions(&request.questions),
            None,
        )
        .await?;
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

    /// Delete a session and every row that belongs to it, in one transaction.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_session(&self, id: &str) -> Result<(), ChatDbError> {
        self.delete_sessions(Some(id)).await?;
        Ok(())
    }

    /// Delete every session and every row that belongs to one, in one
    /// transaction. Returns the number of sessions removed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_all_sessions(&self) -> Result<u64, ChatDbError> {
        self.delete_sessions(None).await
    }

    /// Runs `sql/session_delete.sql` for `id`, or for every session on `None`.
    /// Child rows go before their parents, the `sessions` row last.
    async fn delete_sessions(&self, id: Option<&str>) -> Result<u64, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let mut sessions = 0;
        for statement in super::db_sql::statements(super::db_sql::SESSION_DELETE) {
            sessions = query!(tx, statement, id, id).execute().await?.rows_written;
        }
        tx.commit().await?;
        Ok(sessions)
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
    /// process handles, and SSH connections are rebuilt lazily. Sessions a
    /// run still owned when the daemon last stopped are settled: their
    /// blocking waiting requests are deleted, an active browser takeover
    /// ends with its alert pending, and the state settles to `idle` with its
    /// terminal interaction cleared. Runs are held in the process's memory,
    /// so anything still marked owned or running belongs to a process that
    /// is gone.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn recover_session_runtimes_after_restart(&self) -> Result<(), ChatDbError> {
        const STALE: &str = "(SELECT session_id FROM session_runtimes WHERE run_id IS NOT NULL OR state = 'running')";
        let mut tx = self.db.begin().await?;
        for sql in [
            "DELETE FROM session_pending_permissions WHERE session_id IN {STALE}",
            "DELETE FROM session_pending_workspace WHERE session_id IN {STALE}",
            "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE blocking = 1 AND session_id IN {STALE})",
            "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE blocking = 1 AND session_id IN {STALE})",
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE blocking = 1 AND session_id IN {STALE}",
        ] {
            let sql = sql.replace("{STALE}", STALE);
            query!(tx, sql.as_str()).execute().await?;
        }
        query!(
            tx,
            "UPDATE session_runtimes SET browser_takeover_active = 0, browser_takeover_alert_pending = 1, updated_at = ? WHERE browser_takeover_active = 1",
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        query!(
            tx,
            "UPDATE session_runtimes SET state = 'idle', run_id = NULL, terminal_task_id = NULL, terminal_output_file = NULL, terminal_output_preview = NULL, terminal_input_notice = NULL, terminal_background_reason = NULL, updated_at = ? WHERE run_id IS NOT NULL OR state = 'running'",
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        // A claim dies with its process: claimed entries go back to
        // `pending` so the next run picks them up.
        query!(
            tx,
            "UPDATE session_pending_message_entries SET state = 'pending', claimed_run_id = NULL WHERE state = 'claimed'"
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::super::{ChatDb, query};
    use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest};

    fn ask_user_request(id: &str) -> PersistedAskUserRequest {
        PersistedAskUserRequest {
            id: id.to_string(),
            questions: vec![PersistedAskUserQuestion {
                section: "Section".to_string(),
                question: "Pick one".to_string(),
                option: vec!["a".to_string(), "b".to_string()],
                multi_select: false,
                secret: None,
            }],
        }
    }

    /// Audit and history tables that name a session but outlive it, and the
    /// search outbox, whose triggers queue index removals for deleted messages.
    const RETAINED_ON_DELETE: [&str; 10] = [
        "search_outbox",
        "prs",
        "pr_events",
        "issue_provenance",
        "request_logs",
        "cache_misses",
        "network_access_logs",
        "notification_mailbox",
        "scheduled_tasks",
        "scheduled_task_runs",
    ];

    #[derive(Debug, serde::Deserialize, skyzen::FromRow)]
    struct ColumnRow {
        tbl: String,
        name: String,
        ty: String,
    }

    #[derive(Debug, serde::Deserialize, skyzen::FromRow)]
    struct TableRow {
        tbl: String,
    }

    #[derive(Debug, serde::Deserialize, skyzen::FromRow)]
    struct CountRow {
        value: i64,
    }

    /// Tables the delete script clears, from its `DELETE FROM` statements.
    fn deleted_tables() -> Vec<&'static str> {
        super::super::db_sql::statements(super::super::db_sql::SESSION_DELETE)
            .filter_map(|statement| statement.strip_prefix("DELETE FROM "))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter(|table| *table != "sessions")
            .collect()
    }

    async fn columns(db: &ChatDb) -> Vec<ColumnRow> {
        query!(
            db.db,
            "SELECT m.name AS tbl, p.name AS name, p.type AS ty FROM sqlite_master m, pragma_table_info(m.name) p WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'"
        )
        .fetch_all::<ColumnRow>()
        .await
        .expect("columns should list")
    }

    async fn count(db: &ChatDb, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) AS value FROM {table}");
        query!(db.db, sql.as_str())
            .fetch_all::<CountRow>()
            .await
            .expect("count should run")[0]
            .value
    }

    /// Inserts one row into `table` for the session `session_id`, with every
    /// integer column set to `n` so parent and child rows line up.
    async fn insert_row(db: &ChatDb, columns: &[&ColumnRow], session_id: &str, n: i64) {
        let names = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let values = columns
            .iter()
            .map(|column| match (column.name.as_str(), column.ty.as_str()) {
                ("session_id", _) => format!("'{session_id}'"),
                // Timestamps are always written as ISO 8601.
                ("created_at", _) => "'2026-01-01T00:00:00Z'".to_string(),
                (_, "INTEGER" | "BOOLEAN") => n.to_string(),
                (_, "BLOB") => "x'00'".to_string(),
                _ => format!("'x{n}'"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let table = &columns[0].tbl;
        db.execute_for_test(&format!("INSERT INTO {table} ({names}) VALUES ({values})"))
            .await
            .expect("row should insert");
    }

    /// Every table that can name a session (by `session_id`, or through the
    /// `message_id` / `request_id` of a row that does) is either cleared by
    /// the delete script or listed as retained. A table added without a
    /// decision fails here.
    #[tokio::test]
    async fn every_session_table_is_deleted_or_retained() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should open");
        let deleted = deleted_tables();
        let columns = columns(&db).await;
        let mut keyed = columns
            .iter()
            .filter(|column| {
                matches!(
                    column.name.as_str(),
                    "session_id" | "message_id" | "request_id"
                )
            })
            .map(|column| column.tbl.as_str())
            .collect::<Vec<_>>();
        keyed.sort_unstable();
        keyed.dedup();
        for table in &keyed {
            assert!(
                deleted.contains(table) || RETAINED_ON_DELETE.contains(table),
                "table `{table}` is keyed by a session but session_delete.sql neither clears it nor does RETAINED_ON_DELETE list it"
            );
        }
        for table in &deleted {
            assert!(
                keyed.contains(table),
                "session_delete.sql clears `{table}`, which has no session_id, message_id or request_id column"
            );
        }
        // A foreign key onto `sessions` would block the final delete unless
        // its table is cleared first.
        let references = query!(
            db.db,
            "SELECT m.name AS tbl FROM sqlite_master m, pragma_foreign_key_list(m.name) f WHERE m.type = 'table' AND f.\"table\" = 'sessions'"
        )
        .fetch_all::<TableRow>()
        .await
        .expect("foreign keys should list");
        for row in references {
            assert!(
                deleted.contains(&row.tbl.as_str()),
                "table `{}` references sessions but is not cleared by session_delete.sql",
                row.tbl
            );
        }
    }

    /// Deleting one session clears every row of every cleared table that
    /// belongs to it and leaves another session's rows and the retained
    /// tables alone; deleting all sessions then empties them.
    #[tokio::test]
    async fn delete_session_removes_every_row_it_owns() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should open");
        let deleted = deleted_tables();
        let columns = columns(&db).await;
        let by_table = |table: &str| {
            columns
                .iter()
                .filter(|column| column.tbl == table)
                .collect::<Vec<_>>()
        };
        for (n, id) in [(1, "session-a"), (2, "session-b")] {
            db.create_session(id, Some("Session"), Some("model"))
                .await
                .expect("session should be created");
            // The outbox fills itself through the message triggers.
            for table in deleted
                .iter()
                .chain(&RETAINED_ON_DELETE)
                .filter(|table| **table != "search_outbox")
            {
                insert_row(&db, &by_table(table), id, n).await;
            }
        }

        db.delete_session("session-a")
            .await
            .expect("delete after a turn should succeed");
        assert!(
            db.get_session("session-a")
                .await
                .expect("get should run")
                .is_none()
        );
        for table in &deleted {
            assert_eq!(count(&db, table).await, 1, "`{table}` after one delete");
        }
        for table in &RETAINED_ON_DELETE {
            // The outbox also gains rows from the message triggers.
            assert!(count(&db, table).await >= 2, "`{table}` is retained");
        }

        let removed = db
            .delete_all_sessions()
            .await
            .expect("delete all after turns should succeed");
        assert_eq!(removed, 1);
        for table in &deleted {
            assert_eq!(count(&db, table).await, 0, "`{table}` after delete all");
        }
        assert_eq!(count(&db, "sessions").await, 0);
    }

    /// A long-lived database made by an older build keeps its tables with
    /// foreign keys onto `sessions` (`legacy_session_fk_schema.sql`, read from
    /// a real history database). Opening it drops the two tables current code
    /// no longer reads, and sessions that ran a turn then delete.
    #[tokio::test]
    async fn legacy_foreign_keys_do_not_block_session_delete() {
        const LEGACY_SCHEMA: &str = include_str!("testdata/legacy_session_fk_schema.sql");
        let dir = tempdir().expect("temp dir should be created");
        let path = dir.path().join("history.db");
        let raw = ChatDb::connect(&path).await.expect("raw db should connect");
        // The legacy definitions come first, so the current schema's
        // `CREATE TABLE IF NOT EXISTS` leaves them as they are.
        for statement in super::super::db_sql::statements(LEGACY_SCHEMA) {
            query!(raw, statement)
                .execute()
                .await
                .expect("legacy statement should run");
        }
        ChatDb::initialize_schema(&raw)
            .await
            .expect("schema should initialize");
        for id in ["session-a", "session-b"] {
            for sql in [
                "INSERT INTO sessions (id, message_count, pinned, archived, created_at, updated_at) VALUES (?, 0, 0, 0, 't', 't')",
                "INSERT INTO session_pending_messages (session_id, delivery_mode, content, attachments_json, created_at) VALUES (?, 'queue', 'c', '[]', 't')",
                "INSERT INTO session_pending_ask_user (request_id, session_id, questions_json, created_at) VALUES (?, ?, '[]', 't')",
                "INSERT INTO messages (session_id, role, content) VALUES (?, 'user', 'c')",
                "INSERT INTO session_runtimes (session_id, state, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, 'idle', 0, 0, 't')",
                "INSERT INTO session_plans (session_id, plan_state, updated_at) VALUES (?, 'none', 't')",
                "INSERT INTO session_pending_permissions (request_id, session_id, mode, script, created_at) VALUES (?, ?, 'm', 's', '2026-01-01T00:00:00Z')",
                "INSERT INTO session_pending_workspace (request_id, session_id, path, reason, created_at) VALUES (?, ?, 'p', 'r', '2026-01-01T00:00:00Z')",
            ] {
                if sql.matches('?').count() == 2 {
                    query!(raw, sql, id, id).execute().await
                } else {
                    query!(raw, sql, id).execute().await
                }
                .expect("legacy row should insert");
            }
        }
        // The constraint is enforced: deleting the row alone is the reported 787.
        query!(raw, "DELETE FROM sessions WHERE id = 'session-b'")
            .execute()
            .await
            .expect_err("a bare sessions delete must hit the foreign key");
        drop(raw);

        let db = ChatDb::open(&path).await.expect("legacy db should open");
        for table in ["session_pending_messages", "session_pending_ask_user"] {
            let tables = query!(
                db.db,
                "SELECT name AS tbl FROM sqlite_master WHERE type = 'table' AND name = ?",
                table
            )
            .fetch_all::<TableRow>()
            .await
            .expect("tables should list");
            assert!(tables.is_empty(), "`{table}` should be dropped on open");
            assert!(!deleted_tables().contains(&table));
            assert!(!RETAINED_ON_DELETE.contains(&table));
        }

        db.delete_session("session-a")
            .await
            .expect("delete should succeed");
        assert_eq!(count(&db, "session_runtimes").await, 1);
        assert_eq!(count(&db, "messages").await, 1);
        let removed = db
            .delete_all_sessions()
            .await
            .expect("delete all should succeed");
        assert_eq!(removed, 1);
        assert_eq!(count(&db, "session_runtimes").await, 0);
        assert_eq!(count(&db, "sessions").await, 0);
    }

    /// A write lock held on one pool delays registration on a second pool
    /// only until the holder commits; the whole registration then lands.
    #[tokio::test]
    async fn register_ask_user_request_completes_after_held_write_commits() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let holder = ChatDb::open(&db_path).await.expect("holder db should open");
        holder
            .create_session("session-a", Some("Session"), Some("model"))
            .await
            .expect("session should be created");
        // Open the second pool first: migrations at open time take a write
        // lock and would queue behind the held transaction.
        let waiter = ChatDb::open(&db_path).await.expect("waiter db should open");

        // A write statement on an open transaction takes the write lock.
        let mut tx = holder.db.begin().await.expect("tx should begin");
        query!(
            tx,
            "UPDATE sessions SET title = ? WHERE id = ?",
            "held",
            "session-a"
        )
        .execute()
        .await
        .expect("held write should succeed");

        let task = tokio::spawn(async move {
            waiter
                .register_ask_user_request("session-a", &ask_user_request("req-1"))
                .await
        });
        async_io::Timer::after(std::time::Duration::from_millis(300)).await;
        tx.commit().await.expect("held write should commit");

        let summary = task
            .await
            .expect("registration task should finish")
            .expect("registration should succeed");
        let waiting = summary
            .waiting_ask_user
            .expect("the request should be waiting");
        assert_eq!(waiting.id, "req-1");
    }
}
