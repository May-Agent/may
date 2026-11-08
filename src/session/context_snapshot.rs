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
        document: ctx.document.cloned(),
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

/// Highest transcript `seq` a checkpoint covers (`-1` when it covers none).
///
/// This is the checkpoint's own boundary, not its last item: a compaction whose
/// handoff keeps no items leaves an empty window that still covers every seq
/// below its fork point. A checkpoint serialized before aither recorded the
/// fork point reads it as 0, so the last item still bounds it from below, as
/// aither's own restore does.
///
/// # Panics
///
/// Panics when a seq exceeds `i64::MAX`, which the counter cannot reach.
#[must_use]
pub fn checkpoint_watermark(context: &aither_agent::ContextCheckpoint) -> i64 {
    let after_last_item = context
        .items()
        .last()
        .map_or(0, |item| item.seq().saturating_add(1));
    let covered = context.fork_point().max(after_last_item);
    i64::try_from(covered).expect("checkpoint seq fits in i64") - 1
}

fn snapshot_upsert(
    checkpoint: AgentCheckpoint,
) -> Result<crate::db::SessionContextSnapshotUpsert, crate::db::ChatDbError> {
    // The transcript records exactly what the agent committed: every
    // checkpoint item with its seq. `applied_seq` is the checkpoint's own
    // boundary, so a restore appends only items committed after the checkpoint.
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
    let applied_seq = checkpoint_watermark(&checkpoint.context);
    Ok(crate::db::SessionContextSnapshotUpsert {
        checkpoint,
        applied_seq,
        transcript_entries,
    })
}

async fn persist_checkpoint(
    db: &ChatDb,
    fence: &RunFence,
    checkpoint: AgentCheckpoint,
) -> Result<(), crate::db::ChatDbError> {
    db.upsert_session_context_snapshot_as_run(fence, snapshot_upsert(checkpoint)?)
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

#[cfg(test)]
mod tests {
    use aither_agent::{
        AgentCheckpoint, Context, ContextWindowMetrics, ContextWindowPhase, ContextWindowSnapshot,
    };
    use aither_core::llm::Message;

    use super::{checkpoint_watermark, snapshot_upsert};
    use crate::db::{ChatDb, TranscriptEntry};

    fn checkpoint_of(context: &Context) -> AgentCheckpoint {
        AgentCheckpoint {
            context: context.checkpoint(),
            tool_surface_hash: "tool-surface-hash".to_string(),
            context_window: ContextWindowSnapshot {
                phase: ContextWindowPhase::Stable,
                metrics: ContextWindowMetrics {
                    usage_fraction: 0.5,
                    context_window: 200_000,
                    estimated_tokens: 0,
                    chars_per_token: 4.0,
                    system_block_count: 1,
                    recent_message_count: 0,
                    recent_system_message_count: 0,
                    has_handoff: true,
                    has_staged_system_updates: false,
                    prompt_cache_epoch: 0,
                },
                messages: Vec::new(),
            },
            has_background_tasks: false,
            document: None,
        }
    }

    fn entry(seq: i64, message: &Message) -> TranscriptEntry {
        TranscriptEntry {
            seq,
            message_blob: rmp_serde::to_vec_named(message).expect("message encodes"),
            committed_at: jiff::Timestamp::now().to_string(),
        }
    }

    /// A compaction that keeps no items leaves an empty window; the saved
    /// watermark is still its fork point, so a restore appends only the
    /// entries committed after the handoff.
    #[tokio::test]
    async fn an_empty_post_compaction_window_keeps_its_watermark() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "empty-window";
        let question = Message::user("question");
        let answer = Message::assistant("answer");

        let mut context = Context::new();
        let first = context.push(question.clone());
        let second = context.push(answer.clone());
        assert_eq!((first, second), (0, 1));
        let fork_point = context.fork_point();
        context.apply_handoff(
            aither_agent::Handoff::new(""),
            fork_point,
            0,
            &std::collections::HashSet::new(),
        );
        assert!(
            context.checkpoint().items().is_empty(),
            "the handoff covers every item"
        );

        let mut upsert = snapshot_upsert(checkpoint_of(&context)).expect("upsert builds");
        assert_eq!(upsert.applied_seq, 1, "the watermark is the fork boundary");
        // The transcript keeps every entry the handoff covers.
        upsert.transcript_entries = vec![entry(0, &question), entry(1, &answer)];
        db.upsert_session_context_snapshot(session_id, upsert)
            .await
            .expect("upsert");

        let stored = db
            .get_session_context_snapshot(session_id)
            .await
            .expect("snapshot")
            .expect("snapshot exists");
        assert_eq!(
            stored.applied_seq,
            checkpoint_watermark(&stored.checkpoint.context)
        );
        assert!(
            db.transcript_entries_after(session_id, stored.applied_seq)
                .await
                .expect("delta")
                .is_empty(),
            "restore replays nothing the handoff covers"
        );

        let next = context.push(Message::user("after the handoff"));
        assert_eq!(next, 2);
        assert_eq!(
            checkpoint_watermark(&context.checkpoint()),
            2,
            "the watermark follows later items"
        );
    }
}
