use aither_agent::{AgentCheckpoint, CheckpointContext};

use crate::{
    MayAgent,
    db::{ChatDb, RunFence},
};

/// A context snapshot persisted for a session, used to resume without a full
/// history replay.
#[derive(Debug, Clone)]
pub struct PersistedContextSnapshot {
    /// The agent checkpoint captured at snapshot time.
    pub checkpoint: AgentCheckpoint,
    /// Highest transcript `seq` the checkpoint covers; restore appends only
    /// entries after it.
    pub applied_seq: i64,
}

/// Convert a stored snapshot row into a [`PersistedContextSnapshot`].
#[must_use]
pub fn parse_persisted_context_snapshot(
    snapshot: crate::db::StoredSessionContextSnapshot,
) -> PersistedContextSnapshot {
    PersistedContextSnapshot {
        checkpoint: snapshot.checkpoint,
        applied_seq: snapshot.applied_seq,
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
    fence: &RunFence,
    checkpoint: AgentCheckpoint,
) -> Result<(), crate::db::ChatDbError> {
    // The transcript records exactly what the agent committed: every
    // checkpoint item with its seq. `applied_seq` is the highest committed
    // seq, so a restore appends only items committed after the checkpoint.
    let transcript_entries = checkpoint
        .context
        .items()
        .iter()
        .map(|item| {
            Ok(crate::db::TranscriptEntry {
                seq: i64::try_from(item.seq())
                    .map_err(|e| crate::db::ChatDbError::Storage(format!("seq too large: {e}")))?,
                message_blob: rmp_serde::to_vec_named(item.message()).map_err(|e| {
                    crate::db::ChatDbError::Storage(format!(
                        "failed to encode transcript message: {e}"
                    ))
                })?,
                committed_at: item.committed_at().to_string(),
            })
        })
        .collect::<Result<Vec<_>, crate::db::ChatDbError>>()?;
    let applied_seq = checkpoint
        .context
        .items()
        .last()
        .map_or(0, |item| i64::try_from(item.seq()).unwrap_or(0));
    db.upsert_session_context_snapshot_as_run(
        fence,
        crate::db::SessionContextSnapshotUpsert {
            checkpoint,
            applied_seq,
            transcript_entries,
        },
    )
    .await?;
    Ok(())
}

/// Persist a checkpoint produced by a checkpoint hook as the session's
/// context snapshot, as the run holding `fence`.
///
/// # Errors
///
/// Returns [`ChatDbError`](crate::db::ChatDbError) when reading the latest
/// message ID or upserting the snapshot fails.
pub async fn persist_checkpoint_from_hook(
    db: &ChatDb,
    fence: &RunFence,
    ctx: &CheckpointContext<'_>,
) -> Result<(), crate::db::ChatDbError> {
    persist_checkpoint(db, fence, checkpoint_from_hook(ctx)).await
}

/// Export the agent's checkpoint and persist it as the session's context
/// snapshot, as the run holding `fence`.
///
/// # Errors
///
/// Returns [`PersistContextSnapshotError::ExportCheckpoint`] when the agent
/// cannot export its checkpoint, or [`PersistContextSnapshotError::Persist`]
/// when storing the snapshot fails.
pub async fn persist_agent_context_snapshot(
    db: &ChatDb,
    fence: &RunFence,
    agent: &mut MayAgent,
) -> Result<(), PersistContextSnapshotError> {
    let checkpoint = agent.export_checkpoint().await?;
    persist_checkpoint(db, fence, checkpoint).await?;
    Ok(())
}
