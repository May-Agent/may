//! Context debug snapshots and session context snapshot storage queries.

use super::common::{
    ContextDebugMessageAttachmentRow, ContextDebugMessageRow, ContextDebugMessageToolCallRow,
    ContextDebugPromptAttachmentRow, ContextDebugSnapshot, ContextDebugSnapshotHeaderRow,
    ContextDebugSnapshotMessage, SessionContextSnapshotUpsert, StoredSessionContextSnapshot,
    StoredSessionContextSnapshotRow, overflow_error, storage_error,
};
use super::{ChatDb, ChatDbError, query};

impl ChatDb {
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
        let checkpoint_blob = rmp_serde::to_vec_named(&snapshot.checkpoint)
            .map_err(|error| storage_error(format!("failed to encode checkpoint blob: {error}")))?;
        query!(
            self.db,
            "INSERT INTO session_context_snapshots (session_id, checkpoint_blob, last_message_db_id, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET checkpoint_blob = excluded.checkpoint_blob, last_message_db_id = excluded.last_message_db_id, updated_at = excluded.updated_at",
            session_id,
            checkpoint_blob,
            snapshot.last_message_db_id,
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
            "SELECT checkpoint_blob, last_message_db_id, updated_at FROM session_context_snapshots WHERE session_id = ?",
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
        Ok(Some(StoredSessionContextSnapshot {
            checkpoint,
            last_message_db_id: row.last_message_db_id,
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
}
