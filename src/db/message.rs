//! Message persistence queries on [`ChatDb`].
//!
//! All methods operating on `messages` and its satellite tables:
//! `append_message`, the typed helpers (permission request/response,
//! workspace request/response, `ask_user` request/response), the chat
//! message stats writers, and the message readers (`get_messages`,
//! `llm_message_count`, `get_messages_by_role`, `assistant_cache_usage_summary`).

use crate::PermissionHistoryMode;
use crate::ask_user::AnswerValue;
use crate::session::{ChatMessage, ChatMessageStats, MessageRole};

use super::common::{
    AssistantCacheUsageSummary, AssistantCacheUsageSummaryRow, I64ValueRow, MessageStatsParams,
    message_stats_params_from_chat_stats, overflow_error, row_not_found, storage_error, to_i32,
    to_i64_from_u64, to_u64, to_usize,
};
use super::entity::messages;
use super::run_lease::RunFence;
use super::{ChatDb, ChatDbError, query, truncate_preview};

/// Optional columns carried by an appended message row.
#[derive(Debug, Default)]
pub struct MessageMeta<'a> {
    /// Tool call this message is the result of.
    pub tool_call_id: Option<&'a str>,
    /// Usage/context stats for assistant messages.
    pub stats: Option<&'a MessageStatsParams>,
    /// Serialized attachment records (JSON), when the message carries them.
    pub attachments: Option<&'a str>,
    /// Serialized reasoning states (JSON) on an assistant message.
    pub reasoning: Option<&'a str>,
    /// Serialized tool calls (JSON) on an assistant message.
    pub tool_calls: Option<&'a str>,
}

impl ChatDb {
    /// Append a message to a session. Updates session preview and message count.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
        timestamp: Option<&str>,
        meta: MessageMeta<'_>,
    ) -> Result<messages::Model, ChatDbError> {
        self.append_message_fenced(None, session_id, role, content, timestamp, meta)
            .await
    }

    /// Append a message as part of a run: with a `fence`, the append commits
    /// only while that lease is still held.
    pub(super) async fn append_message_fenced(
        &self,
        fence: Option<&RunFence>,
        session_id: &str,
        role: MessageRole,
        content: &str,
        timestamp: Option<&str>,
        meta: MessageMeta<'_>,
    ) -> Result<messages::Model, ChatDbError> {
        let MessageMeta {
            tool_call_id,
            stats,
            attachments,
            reasoning,
            tool_calls,
        } = meta;
        let now = jiff::Timestamp::now().to_string();
        let prompt_tokens = stats
            .and_then(|s| s.prompt_tokens)
            .map(|value| to_i32(value, "prompt_tokens"))
            .transpose()?;
        let completion_tokens = stats
            .and_then(|s| s.completion_tokens)
            .map(|value| to_i32(value, "completion_tokens"))
            .transpose()?;
        let total_tokens = stats
            .and_then(|s| s.total_tokens)
            .map(|value| to_i32(value, "total_tokens"))
            .transpose()?;
        let cache_read_tokens = stats
            .and_then(|s| s.cache_read_tokens)
            .map(|value| to_i32(value, "cache_read_tokens"))
            .transpose()?;
        let cache_write_tokens = stats
            .and_then(|s| s.cache_write_tokens)
            .map(|value| to_i32(value, "cache_write_tokens"))
            .transpose()?;
        let context_window = stats
            .and_then(|s| s.context_window)
            .map(|value| to_i32(value, "context_window"))
            .transpose()?;
        let elapsed_ms = stats
            .and_then(|s| s.elapsed_ms)
            .map(|value| to_i64_from_u64(value, "elapsed_ms"))
            .transpose()?;
        let provider_id = stats.and_then(|s| s.provider_id.clone());
        let session_update = role.session_summary_update();
        let role_str = role.as_str();
        let mut tx = match fence {
            Some(fence) => self.begin_fenced(fence).await?,
            None => self.db.begin().await?,
        };
        let model = query!(
            tx,
            "INSERT INTO messages (session_id, role, content, timestamp, tool_call_id, provider_id, prompt_tokens, completion_tokens, total_tokens, cache_read_tokens, cache_write_tokens, context_window, elapsed_ms, attachments, reasoning, tool_calls) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
            session_id,
            role_str,
            content,
            timestamp.map(String::from).or_else(|| Some(now.clone())),
            tool_call_id,
            provider_id,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_read_tokens,
            cache_write_tokens,
            context_window,
            elapsed_ms,
            attachments,
            reasoning,
            tool_calls
        )
        .fetch_one::<messages::Model>()
        .await?;

        if session_update.touch_updated_at {
            let preview = session_update
                .update_preview
                .then(|| truncate_preview(content));
            Self::update_session_stats_in(
                &mut tx,
                session_id,
                &now,
                preview.as_deref(),
                session_update.increment_message_count,
            )
            .await?;
        }
        tx.commit().await?;

        Ok(model)
    }

    /// Insert a structured message row within an existing transaction.
    async fn append_structured_message_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        role: MessageRole,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let role_str = role.as_str();
        let model = query!(
            tx,
            "INSERT INTO messages (session_id, role, content, timestamp, tool_call_id, provider_id, prompt_tokens, completion_tokens, total_tokens, cache_read_tokens, cache_write_tokens, context_window, elapsed_ms, attachments, reasoning, tool_calls) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
            session_id,
            role_str,
            "",
            timestamp.map(String::from).or_else(|| Some(now.clone())),
            Option::<String>::None,
            Option::<String>::None,
            Option::<i32>::None,
            Option::<i32>::None,
            Option::<i32>::None,
            Option::<i32>::None,
            Option::<i32>::None,
            Option::<i32>::None,
            Option::<i64>::None,
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None
        )
        .fetch_one::<messages::Model>()
        .await?;
        let session_update = role.session_summary_update();
        if session_update.touch_updated_at {
            Self::update_session_stats_in(
                tx,
                session_id,
                &now,
                None,
                session_update.increment_message_count,
            )
            .await?;
        }
        Ok(model)
    }

    /// Append a permission-request message for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_permission_request_message(
        &self,
        session_id: &str,
        request_id: &str,
        mode: PermissionHistoryMode,
        script: &str,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let message = self
            .append_structured_message_in_tx(
                &mut tx,
                session_id,
                MessageRole::PermissionRequest,
                timestamp,
            )
            .await?;
        query!(
            tx,
            "INSERT INTO message_permission_requests (message_id, request_id, mode, script) VALUES (?, ?, ?, ?)",
            message.id,
            request_id,
            mode.as_str(),
            script
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Append a permission-response message for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_permission_response_message(
        &self,
        session_id: &str,
        request_id: &str,
        approved: bool,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let message = self
            .append_permission_response_message_in_tx(
                &mut tx, session_id, request_id, approved, timestamp,
            )
            .await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Transactional body of [`Self::append_permission_response_message`].
    pub(super) async fn append_permission_response_message_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        request_id: &str,
        approved: bool,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let message = self
            .append_structured_message_in_tx(
                tx,
                session_id,
                MessageRole::PermissionResponse,
                timestamp,
            )
            .await?;
        query!(
            tx,
            "INSERT INTO message_permission_responses (message_id, request_id, approved) VALUES (?, ?, ?)",
            message.id,
            request_id,
            approved
        )
        .execute()
        .await?;
        Ok(message)
    }

    /// Append a workspace-request message for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_workspace_request_message(
        &self,
        session_id: &str,
        request_id: &str,
        path: &str,
        reason: &str,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let message = self
            .append_structured_message_in_tx(
                &mut tx,
                session_id,
                MessageRole::WorkspaceRequest,
                timestamp,
            )
            .await?;
        query!(
            tx,
            "INSERT INTO message_workspace_requests (message_id, request_id, path, reason) VALUES (?, ?, ?, ?)",
            message.id,
            request_id,
            path,
            reason
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Append a workspace-response message for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_workspace_response_message(
        &self,
        session_id: &str,
        request_id: &str,
        approved: bool,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let message = self
            .append_workspace_response_message_in_tx(
                &mut tx, session_id, request_id, approved, timestamp,
            )
            .await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Transactional body of [`Self::append_workspace_response_message`].
    pub(super) async fn append_workspace_response_message_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        request_id: &str,
        approved: bool,
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let message = self
            .append_structured_message_in_tx(
                tx,
                session_id,
                MessageRole::WorkspaceResponse,
                timestamp,
            )
            .await?;
        query!(
            tx,
            "INSERT INTO message_workspace_responses (message_id, request_id, approved) VALUES (?, ?, ?)",
            message.id,
            request_id,
            approved
        )
        .execute()
        .await?;
        Ok(message)
    }

    /// Append an ask-user request message with its questions and options.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_ask_user_request_message(
        &self,
        session_id: &str,
        request_id: &str,
        questions: &[crate::ask_user::Question],
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let message = self
            .append_ask_user_request_message_in_tx(
                &mut tx, session_id, request_id, questions, timestamp,
            )
            .await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Transactional body of [`Self::append_ask_user_request_message`].
    pub(super) async fn append_ask_user_request_message_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        request_id: &str,
        questions: &[crate::ask_user::Question],
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let message = self
            .append_structured_message_in_tx(tx, session_id, MessageRole::AskUserRequest, timestamp)
            .await?;
        query!(
            tx,
            "INSERT INTO message_ask_user_requests (message_id, request_id) VALUES (?, ?)",
            message.id,
            request_id
        )
        .execute()
        .await?;
        for (question_ordinal, question) in questions.iter().enumerate() {
            let question_ordinal = i64::try_from(question_ordinal)
                .map_err(|error| overflow_error("ask-user history question ordinal", &error))?;
            query!(
                tx,
                "INSERT INTO message_ask_user_request_questions (message_id, ordinal, section, question, multi_select) VALUES (?, ?, ?, ?, ?)",
                message.id,
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
                    "INSERT INTO message_ask_user_request_options (message_id, question_ordinal, option_ordinal, value) VALUES (?, ?, ?, ?)",
                    message.id,
                    question_ordinal,
                    i64::try_from(option_ordinal).map_err(|error| overflow_error("ask-user history option ordinal", &error))?,
                    option.as_str()
                )
                .execute()
                .await?;
            }
        }
        Ok(message)
    }

    /// Append an ask-user response message with its answers.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_ask_user_response_message(
        &self,
        session_id: &str,
        request_id: &str,
        answers: &[crate::ask_user::QuestionAnswer],
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let message = self
            .append_ask_user_response_message_in_tx(
                &mut tx, session_id, request_id, answers, timestamp,
            )
            .await?;
        tx.commit().await?;
        Ok(message)
    }

    /// Transactional body of [`Self::append_ask_user_response_message`].
    pub(super) async fn append_ask_user_response_message_in_tx(
        &self,
        tx: &mut skyzen_services::DbTransaction,
        session_id: &str,
        request_id: &str,
        answers: &[crate::ask_user::QuestionAnswer],
        timestamp: Option<&str>,
    ) -> Result<messages::Model, ChatDbError> {
        let message = self
            .append_structured_message_in_tx(
                tx,
                session_id,
                MessageRole::AskUserResponse,
                timestamp,
            )
            .await?;
        query!(
            tx,
            "INSERT INTO message_ask_user_responses (message_id, request_id) VALUES (?, ?)",
            message.id,
            request_id
        )
        .execute()
        .await?;
        for (answer_ordinal, answer) in answers.iter().enumerate() {
            let answer_ordinal = i64::try_from(answer_ordinal)
                .map_err(|error| overflow_error("ask-user history answer ordinal", &error))?;
            let (answer_kind, custom_text, selected_values) = match &answer.answer {
                AnswerValue::Selected(values) => ("selected", None, values.as_slice()),
                AnswerValue::Custom(value) => ("custom", Some(value.as_str()), &[][..]),
                AnswerValue::Skipped => {
                    unreachable!("May never commits a skipped answer: check_answers refuses it")
                }
            };
            query!(
                tx,
                "INSERT INTO message_ask_user_response_answers (message_id, ordinal, section, answer_kind, custom_text) VALUES (?, ?, ?, ?, ?)",
                message.id,
                answer_ordinal,
                answer.section.as_str(),
                answer_kind,
                custom_text
            )
            .execute()
            .await?;
            for (option_ordinal, value) in selected_values.iter().enumerate() {
                query!(
                    tx,
                    "INSERT INTO message_ask_user_response_selected_options (message_id, answer_ordinal, option_ordinal, value) VALUES (?, ?, ?, ?)",
                    message.id,
                    answer_ordinal,
                    i64::try_from(option_ordinal).map_err(|error| overflow_error("ask-user history selected option ordinal", &error))?,
                    value.as_str()
                )
                .execute()
                .await?;
            }
        }
        Ok(message)
    }

    /// Append a chat history message to a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_chat_message(
        &self,
        session_id: &str,
        message: &ChatMessage,
    ) -> Result<messages::Model, ChatDbError> {
        self.append_chat_message_fenced(None, session_id, message)
            .await
    }

    /// Append a chat history message as part of a run: committed only while
    /// the run's lease `fence` still holds.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RunLeaseLost`] when the lease was taken over or
    /// released, or [`ChatDbError`] on storage failure.
    pub async fn append_chat_message_as_run(
        &self,
        fence: &RunFence,
        message: &ChatMessage,
    ) -> Result<messages::Model, ChatDbError> {
        self.append_chat_message_fenced(Some(fence), &fence.session_id, message)
            .await
    }

    async fn append_chat_message_fenced(
        &self,
        fence: Option<&RunFence>,
        session_id: &str,
        message: &ChatMessage,
    ) -> Result<messages::Model, ChatDbError> {
        let timestamp = message.timestamp.map(|value| value.to_string());
        let stats = message
            .stats
            .as_ref()
            .map(message_stats_params_from_chat_stats);
        let attachments = if message.attachments.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&message.attachments).map_err(|error| {
                    storage_error(format!("failed to serialize message attachments: {error}"))
                })?,
            )
        };
        let reasoning = if message.reasoning.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&message.reasoning).map_err(|error| {
                storage_error(format!("failed to serialize message reasoning: {error}"))
            })?)
        };
        let tool_calls = if message.tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&message.tool_calls).map_err(|error| {
                storage_error(format!("failed to serialize message tool calls: {error}"))
            })?)
        };
        self.append_message_fenced(
            fence,
            session_id,
            message.role.clone(),
            &message.content,
            timestamp.as_deref(),
            MessageMeta {
                tool_call_id: message.tool_call_id.as_deref(),
                stats: stats.as_ref(),
                attachments: attachments.as_deref(),
                reasoning: reasoning.as_deref(),
                tool_calls: tool_calls.as_deref(),
            },
        )
        .await
    }

    /// Update usage/context stats on the latest assistant message in a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn set_latest_assistant_stats(
        &self,
        session_id: &str,
        stats: &MessageStatsParams,
    ) -> Result<(), ChatDbError> {
        let row = query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? AND role = ? ORDER BY id DESC LIMIT 1",
            session_id,
            MessageRole::Assistant.as_str()
        )
        .fetch_optional::<messages::Model>()
        .await?
        .ok_or_else(|| {
            row_not_found(format!(
                "no assistant message found in session {session_id}"
            ))
        })?;

        let prompt_tokens = stats
            .prompt_tokens
            .map(|value| to_i32(value, "prompt_tokens"))
            .transpose()?;
        let completion_tokens = stats
            .completion_tokens
            .map(|value| to_i32(value, "completion_tokens"))
            .transpose()?;
        let total_tokens = stats
            .total_tokens
            .map(|value| to_i32(value, "total_tokens"))
            .transpose()?;
        let cache_read_tokens = stats
            .cache_read_tokens
            .map(|value| to_i32(value, "cache_read_tokens"))
            .transpose()?;
        let cache_write_tokens = stats
            .cache_write_tokens
            .map(|value| to_i32(value, "cache_write_tokens"))
            .transpose()?;
        let context_window = stats
            .context_window
            .map(|value| to_i32(value, "context_window"))
            .transpose()?;
        let elapsed_ms = stats
            .elapsed_ms
            .map(|value| to_i64_from_u64(value, "elapsed_ms"))
            .transpose()?;
        query!(
            self.db,
            "UPDATE messages SET provider_id = ?, prompt_tokens = ?, completion_tokens = ?, total_tokens = ?, cache_read_tokens = ?, cache_write_tokens = ?, context_window = ?, elapsed_ms = ? WHERE id = ?",
            stats.provider_id.as_deref(),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_read_tokens,
            cache_write_tokens,
            context_window,
            elapsed_ms,
            row.id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Update usage/context stats on the latest assistant message from a [`ChatMessageStats`].
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn set_latest_assistant_chat_stats(
        &self,
        session_id: &str,
        stats: &ChatMessageStats,
    ) -> Result<(), ChatDbError> {
        self.set_latest_assistant_stats(session_id, &message_stats_params_from_chat_stats(stats))
            .await
    }

    /// Get all messages for a session, ordered by id.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<messages::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? ORDER BY id ASC",
            session_id
        )
        .fetch_all::<messages::Model>()
        .await?)
    }

    /// Delete every message in `session_id` whose id is `from_id` or later,
    /// together with its structured satellite rows, then refresh the
    /// session's preview, message count, and `updated_at`. Used to rewind a
    /// conversation back to an earlier prompt boundary.
    ///
    /// Returns the number of deleted `messages` rows.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_messages_from(
        &self,
        session_id: &str,
        from_id: i64,
    ) -> Result<u64, ChatDbError> {
        const SATELLITE_TABLES: &[&str] = &[
            "message_permission_requests",
            "message_permission_responses",
            "message_workspace_requests",
            "message_workspace_responses",
            "message_ask_user_requests",
            "message_ask_user_request_questions",
            "message_ask_user_request_options",
            "message_ask_user_responses",
            "message_ask_user_response_answers",
            "message_ask_user_response_selected_options",
        ];
        let mut tx = self.db.begin().await?;
        for table in SATELLITE_TABLES {
            let sql = format!(
                "DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE session_id = ? AND id >= ?)"
            );
            query!(tx, sql.as_str(), session_id, from_id)
                .execute()
                .await?;
        }
        let deleted = query!(
            tx,
            "DELETE FROM messages WHERE session_id = ? AND id >= ?",
            session_id,
            from_id
        )
        .execute()
        .await?
        .rows_written;
        let preview_row = query!(
            tx,
            "SELECT content AS value FROM messages WHERE session_id = ? AND role IN (?, ?, ?) ORDER BY id DESC LIMIT 1",
            session_id,
            MessageRole::User.as_str(),
            MessageRole::Assistant.as_str(),
            MessageRole::System.as_str()
        )
        .fetch_optional::<super::common::StringValueRow>()
        .await?;
        let count_row = query!(
            tx,
            "SELECT COUNT(*) AS value FROM messages WHERE session_id = ? AND role IN (?, ?, ?)",
            session_id,
            MessageRole::User.as_str(),
            MessageRole::Assistant.as_str(),
            MessageRole::System.as_str()
        )
        .fetch_one::<I64ValueRow>()
        .await?;
        query!(
            tx,
            "UPDATE sessions SET message_count = ?, preview = ?, updated_at = ? WHERE id = ?",
            to_i32(
                u32::try_from(count_row.value)
                    .map_err(|error| overflow_error("message_count", &error))?,
                "message_count",
            )?,
            preview_row.map(|row| truncate_preview(&row.value)),
            jiff::Timestamp::now().to_string(),
            session_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// Return the highest `messages.id` for a session, or `None` if empty.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn max_message_id(&self, session_id: &str) -> Result<Option<i64>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT MAX(id) AS value FROM messages WHERE session_id = ?",
            session_id
        )
        .fetch_one::<crate::db::common::OptionalI64ValueRow>()
        .await?;
        Ok(row.value)
    }

    /// Count messages that contribute to the LLM-visible conversation history.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn llm_message_count(&self, session_id: &str) -> Result<usize, ChatDbError> {
        let count = query!(
            self.db,
            "SELECT COUNT(*) AS value FROM messages WHERE session_id = ? AND role IN (?, ?, ?, ?, ?)",
            session_id,
            MessageRole::User.as_str(),
            MessageRole::Assistant.as_str(),
            MessageRole::System.as_str(),
            MessageRole::HiddenContext.as_str(),
            MessageRole::Tool.as_str()
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value;
        to_usize(count, "llm_message_count")
    }

    /// Get messages for a session filtered by role.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_messages_by_role(
        &self,
        session_id: &str,
        role: &str,
    ) -> Result<Vec<messages::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? AND role = ? ORDER BY id ASC",
            session_id,
            role
        )
        .fetch_all::<messages::Model>()
        .await?)
    }

    /// Summarize persisted cache-token usage across assistant messages.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn assistant_cache_usage_summary(
        &self,
    ) -> Result<AssistantCacheUsageSummary, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT COUNT(*) AS assistant_message_count, COALESCE(SUM(CASE WHEN COALESCE(cache_read_tokens, 0) > 0 THEN 1 ELSE 0 END), 0) AS cache_hit_message_count, COALESCE(SUM(COALESCE(prompt_tokens, 0)), 0) AS prompt_tokens_total, COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0) AS cache_read_tokens_total, COALESCE(SUM(COALESCE(cache_write_tokens, 0)), 0) AS cache_write_tokens_total FROM messages WHERE role = ?",
            MessageRole::Assistant.as_str()
        )
        .fetch_one::<AssistantCacheUsageSummaryRow>()
        .await?;
        Ok(AssistantCacheUsageSummary {
            assistant_message_count: to_u64(
                row.assistant_message_count,
                "assistant_message_count",
            )?,
            cache_hit_message_count: to_u64(
                row.cache_hit_message_count,
                "cache_hit_message_count",
            )?,
            prompt_tokens_total: to_u64(row.prompt_tokens_total, "prompt_tokens_total")?,
            cache_read_tokens_total: to_u64(
                row.cache_read_tokens_total,
                "cache_read_tokens_total",
            )?,
            cache_write_tokens_total: to_u64(
                row.cache_write_tokens_total,
                "cache_write_tokens_total",
            )?,
        })
    }
}
