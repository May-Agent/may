use aither_agent::{AgentCheckpoint, CheckpointContext};
use aither_core::llm::Message as LlmMessage;

use crate::{MayAgent, db::ChatDb};

#[derive(Debug, Clone)]
pub struct PersistedContextSnapshot {
    pub checkpoint: AgentCheckpoint,
    pub history_message_count: usize,
}

#[must_use]
pub fn parse_persisted_context_snapshot(
    snapshot: crate::db::StoredSessionContextSnapshot,
) -> PersistedContextSnapshot {
    PersistedContextSnapshot {
        checkpoint: snapshot.checkpoint,
        history_message_count: snapshot.history_message_count,
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

pub fn history_delta_since_snapshot(
    history_messages: &[LlmMessage],
    history_message_count: usize,
) -> Result<Vec<LlmMessage>, HistoryDeltaError> {
    if history_message_count > history_messages.len() {
        return Err(HistoryDeltaError::AnchorExceedsAvailableHistory {
            anchor: history_message_count,
            history: history_messages.len(),
        });
    }
    Ok(history_messages[history_message_count..].to_vec())
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryDeltaError {
    #[error(
        "persisted context snapshot anchor exceeds available history: anchor={anchor}, history={history}"
    )]
    AnchorExceedsAvailableHistory { anchor: usize, history: usize },
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
    let history_message_count = db.llm_message_count(session_id).await?;
    db.upsert_session_context_snapshot(
        session_id,
        crate::db::SessionContextSnapshotUpsert {
            checkpoint,
            history_message_count,
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
