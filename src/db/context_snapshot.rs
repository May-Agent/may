//! Context debug snapshots and session context snapshot storage queries.

use super::common::{
    ContextDebugMessageAttachmentRow, ContextDebugMessageRow, ContextDebugMessageToolCallRow,
    ContextDebugPromptAttachmentRow, ContextDebugSnapshot, ContextDebugSnapshotHeaderRow,
    ContextDebugSnapshotMessage, SessionContextSnapshotUpsert, StoredSessionContextSnapshot,
    StoredSessionContextSnapshotRow, overflow_error, storage_error,
};
use super::{ChatDb, ChatDbError, query};

/// Format version of the serialized [`aither_agent::AgentCheckpoint`] stored in
/// `session_context_snapshots.checkpoint_blob`.
///
/// The blob is produced by `rmp_serde::to_vec_named` and decoded into a type
/// marked `deny_unknown_fields`, so any aither upgrade that changes the
/// checkpoint's serialized shape makes every older blob undecodable. Bump this
/// constant whenever that shape changes; rows written under a different
/// version are invalidated when the database is opened.
///
/// 1 = every blob written before versioning existed (the pre-0.7 and 0.7
/// shapes alike, which are indistinguishable without a recorded version).
/// 2 = the current `AgentCheckpoint` shape.
pub(super) const CONTEXT_SNAPSHOT_FORMAT: i64 = 2;

/// Inputs for [`ChatDb::upsert_context_debug_snapshot`].
#[derive(Debug)]
pub struct ContextDebugSnapshotWrite<'a> {
    /// Session the snapshot belongs to.
    pub session_id: &'a str,
    /// ISO 8601 capture timestamp.
    pub timestamp: &'a str,
    /// Model that produced the context.
    pub model: &'a str,
    /// Provider that produced the context.
    pub provider_id: &'a str,
    /// User prompt that seeded the context.
    pub prompt: &'a str,
    /// Attachment URLs recorded on the prompt.
    pub prompt_attachments: &'a [String],
    /// Assembled messages that made up the context window.
    pub assembled_messages: &'a [ContextDebugSnapshotMessage],
}

impl ChatDb {
    // ── Context Debug Snapshot ────────────────────────────────

    /// Upsert the latest context-debug snapshot for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn upsert_context_debug_snapshot(
        &self,
        snapshot: &ContextDebugSnapshotWrite<'_>,
    ) -> Result<(), ChatDbError> {
        let &ContextDebugSnapshotWrite {
            session_id,
            timestamp,
            model,
            provider_id,
            prompt,
            prompt_attachments,
            assembled_messages,
        } = snapshot;
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
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
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
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
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
            "INSERT INTO session_context_snapshots (session_id, checkpoint_blob, last_message_db_id, updated_at, format_version) VALUES (?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET checkpoint_blob = excluded.checkpoint_blob, last_message_db_id = excluded.last_message_db_id, updated_at = excluded.updated_at, format_version = excluded.format_version",
            session_id,
            checkpoint_blob,
            snapshot.last_message_db_id,
            updated_at,
            CONTEXT_SNAPSHOT_FORMAT
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Load the serialized agent context snapshot for a session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_session_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<StoredSessionContextSnapshot>, ChatDbError> {
        let Some(row) = query!(
            self.db,
            "SELECT format_version, checkpoint_blob, last_message_db_id, updated_at FROM session_context_snapshots WHERE session_id = ?",
            session_id
        )
            .fetch_optional::<StoredSessionContextSnapshotRow>()
            .await?
        else {
            return Ok(None);
        };

        if row.format_version != CONTEXT_SNAPSHOT_FORMAT {
            return Err(storage_error(format!(
                "context snapshot for session {session_id} has blob format {}, expected {CONTEXT_SNAPSHOT_FORMAT}",
                row.format_version
            )));
        }
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
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
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

#[cfg(test)]
mod tests {
    use skyzen_services::Db as SkyDb;
    use tempfile::tempdir;

    use super::{CONTEXT_SNAPSHOT_FORMAT, ChatDb};
    use crate::db::SessionContextSnapshotUpsert;

    fn representative_checkpoint() -> aither_agent::AgentCheckpoint {
        aither_agent::AgentCheckpoint {
            context: aither_agent::ContextCheckpoint::new(None, Vec::new()),
            tool_surface_hash: "tool-surface-hash".to_string(),
            context_window: aither_agent::ContextWindowSnapshot {
                phase: aither_agent::ContextWindowPhase::Stable,
                metrics: aither_agent::ContextWindowMetrics {
                    usage_fraction: 0.5,
                    selected_model_context_window: Some(200_000),
                    fast_model_context_window: None,
                    effective_context_window: 200_000,
                    system_block_count: 1,
                    reminder_count: 0,
                    recent_message_count: 1,
                    recent_system_message_count: 1,
                    has_handoff: false,
                },
                messages: vec![aither_core::llm::Message::user("hello")],
            },
            has_background_tasks: false,
        }
    }

    /// Guards `CONTEXT_SNAPSHOT_FORMAT`: when this fails after an aither
    /// upgrade, the serialized `AgentCheckpoint` shape changed — bump
    /// `CONTEXT_SNAPSHOT_FORMAT` and update the expected key list below.
    #[test]
    fn checkpoint_blob_shape_matches_recorded_format() {
        let blob = rmp_serde::to_vec_named(&representative_checkpoint())
            .expect("checkpoint should encode");
        let value = rmp_serde::from_slice::<serde_json::Value>(&blob)
            .expect("blob should decode as a msgpack map");
        let mut keys = value
            .as_object()
            .expect("blob top level must be a map")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "context",
                "context_window",
                "has_background_tasks",
                "tool_surface_hash"
            ]
        );
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips_checkpoint() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");

        let session_id = "session-round-trip";
        db.upsert_session_context_snapshot(
            session_id,
            SessionContextSnapshotUpsert {
                checkpoint: representative_checkpoint(),
                last_message_db_id: Some(42),
            },
        )
        .await
        .expect("upsert should succeed");

        let stored = db
            .get_session_context_snapshot(session_id)
            .await
            .expect("get should succeed")
            .expect("snapshot should exist");
        assert_eq!(stored.last_message_db_id, Some(42));
        assert_eq!(stored.checkpoint.tool_surface_hash, "tool-surface-hash");
        assert_eq!(stored.checkpoint.context_window.messages.len(), 1);
        assert!(!stored.checkpoint.has_background_tasks);
    }

    #[tokio::test]
    async fn legacy_unversioned_snapshot_is_invalidated_on_open() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let session_id = "session-legacy";

        // Seed a database whose snapshot table predates format versioning,
        // holding a pre-0.7-style blob (current shape plus `todo_items`).
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let raw = SkyDb::connect_sqlite(&url)
            .await
            .expect("raw db should connect");
        raw.query("CREATE TABLE session_context_snapshots (session_id TEXT PRIMARY KEY, checkpoint_blob BLOB NOT NULL, last_message_db_id INTEGER, updated_at TEXT NOT NULL)")
            .execute()
            .await
            .expect("legacy table should be created");
        let mut legacy_json =
            serde_json::to_value(representative_checkpoint()).expect("checkpoint should serialize");
        legacy_json
            .as_object_mut()
            .expect("checkpoint serializes to an object")
            .insert("todo_items".to_string(), serde_json::json!([]));
        let legacy_blob = rmp_serde::to_vec_named(&legacy_json).expect("legacy blob should encode");
        // The seeded blob must actually be undecodable today, otherwise the
        // test proves nothing about the migration.
        assert!(
            rmp_serde::from_slice::<aither_agent::AgentCheckpoint>(&legacy_blob).is_err(),
            "seeded legacy blob must fail to decode"
        );
        raw.query("INSERT INTO session_context_snapshots (session_id, checkpoint_blob, last_message_db_id, updated_at) VALUES (?, ?, NULL, ?)")
            .bind(session_id)
            .bind(legacy_blob)
            .bind(jiff::Timestamp::now().to_string())
            .execute()
            .await
            .expect("legacy snapshot should be inserted");
        drop(raw);

        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");
        let snapshot = db
            .get_session_context_snapshot(session_id)
            .await
            .expect("get should succeed");
        assert!(snapshot.is_none(), "legacy snapshot must be invalidated");

        // The chat-preparation path failed at this load step before the
        // migration; with the row invalidated it now falls back to history
        // replay.
        let loaded = crate::session::load_persisted_context_snapshot(&db, session_id)
            .await
            .expect("snapshot load must succeed");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn corrupt_current_format_blob_is_an_error() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-corrupt";
        db.db
            .query("INSERT INTO session_context_snapshots (session_id, checkpoint_blob, last_message_db_id, updated_at, format_version) VALUES (?, ?, NULL, ?, ?)")
            .bind(session_id)
            .bind(vec![0xde, 0xad, 0xbe, 0xef])
            .bind(jiff::Timestamp::now().to_string())
            .bind(CONTEXT_SNAPSHOT_FORMAT)
            .execute()
            .await
            .expect("corrupt snapshot should be inserted");

        let error = db
            .get_session_context_snapshot(session_id)
            .await
            .expect_err("corrupt blob must error");
        assert!(
            error.to_string().contains(session_id),
            "error should name the session: {error}"
        );
    }

    #[tokio::test]
    async fn mismatched_format_version_is_an_error() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-future";
        let blob = rmp_serde::to_vec_named(&representative_checkpoint())
            .expect("checkpoint should encode");
        let other_version = CONTEXT_SNAPSHOT_FORMAT + 1;
        db.db
            .query("INSERT INTO session_context_snapshots (session_id, checkpoint_blob, last_message_db_id, updated_at, format_version) VALUES (?, ?, NULL, ?, ?)")
            .bind(session_id)
            .bind(blob)
            .bind(jiff::Timestamp::now().to_string())
            .bind(other_version)
            .execute()
            .await
            .expect("snapshot should be inserted");

        let error = db
            .get_session_context_snapshot(session_id)
            .await
            .expect_err("foreign-format blob must error");
        let message = error.to_string();
        assert!(
            message.contains(session_id)
                && message.contains(&other_version.to_string())
                && message.contains(&CONTEXT_SNAPSHOT_FORMAT.to_string()),
            "error should name the session and both versions: {message}"
        );
    }
}
