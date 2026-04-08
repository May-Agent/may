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
        active_skill_names: ctx.active_skill_names.to_vec(),
        tool_surface_hash: ctx.tool_surface_hash.to_string(),
        context_window: ctx.window.clone(),
        has_background_tasks: ctx.has_background_tasks,
    }
}

pub fn history_delta_since_snapshot(
    history_messages: &[LlmMessage],
    history_message_count: usize,
) -> anyhow::Result<Vec<LlmMessage>> {
    if history_message_count > history_messages.len() {
        return Err(anyhow::anyhow!(
            "persisted context snapshot anchor exceeds available history: anchor={}, history={}",
            history_message_count,
            history_messages.len()
        ));
    }
    Ok(history_messages[history_message_count..].to_vec())
}

pub async fn load_persisted_context_snapshot(
    db: &ChatDb,
    session_id: &str,
) -> anyhow::Result<Option<PersistedContextSnapshot>> {
    Ok(db
        .get_session_context_snapshot(session_id)
        .await
        .map_err(anyhow::Error::from)?
        .map(parse_persisted_context_snapshot))
}

async fn persist_checkpoint(
    db: &ChatDb,
    session_id: &str,
    checkpoint: AgentCheckpoint,
) -> anyhow::Result<()> {
    let history_message_count = db
        .llm_message_count(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    db.upsert_session_context_snapshot(
        session_id,
        crate::db::SessionContextSnapshotUpsert {
            checkpoint,
            history_message_count,
        },
    )
    .await
    .map_err(anyhow::Error::from)
}

pub async fn persist_checkpoint_from_hook(
    db: &ChatDb,
    session_id: &str,
    ctx: &CheckpointContext<'_>,
) -> anyhow::Result<()> {
    persist_checkpoint(db, session_id, checkpoint_from_hook(ctx)).await
}

pub async fn persist_agent_context_snapshot(
    db: &ChatDb,
    session_id: &str,
    agent: &mut MayAgent,
) -> anyhow::Result<()> {
    let checkpoint = agent
        .export_checkpoint()
        .await
        .map_err(anyhow::Error::from)?;
    persist_checkpoint(db, session_id, checkpoint).await
}
