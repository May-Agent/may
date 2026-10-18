use aither_agent::{AgentCheckpoint, CheckpointContext};
use aither_core::llm::Message as LlmMessage;

use crate::{MayAgent, db::ChatDb};

/// A context snapshot persisted for a session, used to resume without a full
/// history replay.
#[derive(Debug, Clone)]
pub struct PersistedContextSnapshot {
    /// The agent checkpoint captured at snapshot time.
    pub checkpoint: AgentCheckpoint,
    /// The DB `messages.id` of the last message at snapshot time.
    pub last_message_db_id: Option<i64>,
}

/// Convert a stored snapshot row into a [`PersistedContextSnapshot`].
#[must_use]
pub fn parse_persisted_context_snapshot(
    snapshot: crate::db::StoredSessionContextSnapshot,
) -> PersistedContextSnapshot {
    PersistedContextSnapshot {
        checkpoint: snapshot.checkpoint,
        last_message_db_id: snapshot.last_message_db_id,
    }
}

/// Build an [`AgentCheckpoint`] from a checkpoint-hook context.
#[must_use]
pub fn checkpoint_from_hook(ctx: &CheckpointContext<'_>) -> AgentCheckpoint {
    AgentCheckpoint {
        context: ctx.context.clone(),
        tool_surface_hash: ctx.tool_surface_hash.to_string(),
        context_window: ctx.window.clone(),
        has_background_tasks: ctx.has_background_tasks,
    }
}

/// `messages` row ids of the rows that produce LLM history messages.
///
/// Row order is kept — the `db_message_ids` argument of
/// [`history_delta_since_snapshot`]. Rows of interaction roles carry no
/// LLM message and must not shift the index alignment.
#[must_use]
pub fn llm_row_ids(db_messages: &[crate::db::entity::messages::Model]) -> Vec<i64> {
    db_messages
        .iter()
        .filter(|row| {
            row.role
                .parse::<crate::session::MessageRole>()
                .is_ok_and(|role| role.is_llm_visible())
        })
        .map(|row| i64::from(row.id))
        .collect()
}

/// Compute the history messages that were added after the snapshot was taken.
///
/// `db_message_ids` should be the `id` column of each row returned by
/// `get_messages(session_id)`, in the same order as `history_messages`.
/// The caller must guarantee that `history_messages[i]` came from the DB
/// row whose id is `db_message_ids[i]`.
///
/// Returns `None` if the snapshot anchor is stale (the anchor ID is not found
/// among current messages), signalling the caller should fall back to full
/// history replay.
#[must_use]
pub fn history_delta_since_snapshot(
    history_messages: &[LlmMessage],
    db_message_ids: &[i64],
    last_message_db_id: Option<i64>,
) -> Option<Vec<LlmMessage>> {
    let Some(anchor_id) = last_message_db_id else {
        // Snapshot was taken with empty history — replay everything
        return Some(history_messages.to_vec());
    };
    // Find the position of the first message *after* the anchor
    let start_idx = db_message_ids.iter().position(|&id| id > anchor_id);
    start_idx.map_or_else(
        || {
            // Either all messages are <= anchor (no new messages — empty delta)
            // or the anchor is missing entirely (stale snapshot)
            if db_message_ids.contains(&anchor_id) {
                Some(Vec::new()) // anchor exists, just no newer messages
            } else {
                None // stale anchor — caller should fall back
            }
        },
        |idx| Some(history_messages[idx..].to_vec()),
    )
}

/// Error type for persisting a session's context snapshot.
#[derive(Debug, thiserror::Error)]
pub enum PersistContextSnapshotError {
    /// Failed to export agent checkpoint.
    #[error("failed to export agent checkpoint: {0}")]
    ExportCheckpoint(#[from] aither_agent::AgentError),
    /// Failed to persist context snapshot.
    #[error("failed to persist context snapshot: {0}")]
    Persist(#[from] crate::db::ChatDbError),
}

/// Load the stored context snapshot for `session_id`, if one exists.
///
/// # Errors
///
/// Returns [`ChatDbError`](crate::db::ChatDbError) when the query fails.
pub async fn load_persisted_context_snapshot(
    db: &ChatDb,
    session_id: &str,
) -> Result<Option<PersistedContextSnapshot>, crate::db::ChatDbError> {
    Ok(db
        .get_session_context_snapshot(session_id)
        .await?
        .map(parse_persisted_context_snapshot))
}

async fn persist_checkpoint(
    db: &ChatDb,
    session_id: &str,
    checkpoint: AgentCheckpoint,
) -> Result<(), crate::db::ChatDbError> {
    let last_message_db_id = db.max_message_id(session_id).await?;
    db.upsert_session_context_snapshot(
        session_id,
        crate::db::SessionContextSnapshotUpsert {
            checkpoint,
            last_message_db_id,
        },
    )
    .await?;
    Ok(())
}

/// Persist a checkpoint produced by a checkpoint hook as the session's
/// context snapshot.
///
/// # Errors
///
/// Returns [`ChatDbError`](crate::db::ChatDbError) when reading the latest
/// message ID or upserting the snapshot fails.
pub async fn persist_checkpoint_from_hook(
    db: &ChatDb,
    session_id: &str,
    ctx: &CheckpointContext<'_>,
) -> Result<(), crate::db::ChatDbError> {
    persist_checkpoint(db, session_id, checkpoint_from_hook(ctx)).await
}

/// Export the agent's checkpoint and persist it as the session's context
/// snapshot.
///
/// # Errors
///
/// Returns [`PersistContextSnapshotError::ExportCheckpoint`] when the agent
/// cannot export its checkpoint, or [`PersistContextSnapshotError::Persist`]
/// when storing the snapshot fails.
pub async fn persist_agent_context_snapshot(
    db: &ChatDb,
    session_id: &str,
    agent: &mut MayAgent,
) -> Result<(), PersistContextSnapshotError> {
    let checkpoint = agent.export_checkpoint().await?;
    persist_checkpoint(db, session_id, checkpoint).await?;
    Ok(())
}
