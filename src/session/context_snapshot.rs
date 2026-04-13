use aither_agent::{AgentCheckpoint, CheckpointContext};
use aither_core::llm::Message as LlmMessage;

use crate::{MayAgent, db::ChatDb};

#[derive(Debug, Clone)]
pub struct PersistedContextSnapshot {
    pub checkpoint: AgentCheckpoint,
    /// The DB `messages.id` of the last message at snapshot time.
    pub last_message_db_id: Option<i64>,
}

#[must_use]
pub fn parse_persisted_context_snapshot(
    snapshot: crate::db::StoredSessionContextSnapshot,
) -> PersistedContextSnapshot {
    PersistedContextSnapshot {
        checkpoint: snapshot.checkpoint,
        last_message_db_id: snapshot.last_message_db_id,
    }
}

#[must_use]
pub fn checkpoint_from_hook(ctx: &CheckpointContext<'_>) -> AgentCheckpoint {
    AgentCheckpoint {
        context: ctx.context.clone(),
        todo_items: ctx.todo_items.to_vec(),
        tool_surface_hash: ctx.tool_surface_hash.to_string(),
        context_window: ctx.window.clone(),
        has_background_tasks: ctx.has_background_tasks,
    }
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
    let start_idx = db_message_ids
        .iter()
        .position(|&id| id > anchor_id);
    match start_idx {
        Some(idx) => Some(history_messages[idx..].to_vec()),
        None => {
            // Either all messages are <= anchor (no new messages — empty delta)
            // or the anchor is missing entirely (stale snapshot)
            if db_message_ids.iter().any(|&id| id == anchor_id) {
                Some(Vec::new()) // anchor exists, just no newer messages
            } else {
                None // stale anchor — caller should fall back
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PersistContextSnapshotError {
    #[error("failed to export agent checkpoint: {0}")]
    ExportCheckpoint(#[from] aither_agent::AgentError),
    #[error("failed to persist context snapshot: {0}")]
    Persist(#[from] crate::db::ChatDbError),
}

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

pub async fn persist_checkpoint_from_hook(
    db: &ChatDb,
    session_id: &str,
    ctx: &CheckpointContext<'_>,
) -> Result<(), crate::db::ChatDbError> {
    persist_checkpoint(db, session_id, checkpoint_from_hook(ctx)).await
}

pub async fn persist_agent_context_snapshot(
    db: &ChatDb,
    session_id: &str,
    agent: &mut MayAgent,
) -> Result<(), PersistContextSnapshotError> {
    let checkpoint = agent.export_checkpoint().await?;
    persist_checkpoint(db, session_id, checkpoint).await?;
    Ok(())
}
