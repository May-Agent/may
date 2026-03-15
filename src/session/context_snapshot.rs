use aither_core::llm::Message as LlmMessage;

use crate::{MayAgent, db::ChatDb};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StoredContextWindowMetadata {
    pub phase: aither_agent::ContextWindowPhase,
    pub metrics: aither_agent::ContextWindowMetrics,
    #[serde(default)]
    pub history_message_count: usize,
    #[serde(default)]
    pub tool_surface_hash: Option<String>,
    #[serde(default)]
    pub has_background_tasks: bool,
}

#[derive(Debug, Clone)]
pub struct PersistedContextSnapshot {
    pub checkpoint: aither_agent::AgentCheckpoint,
    pub history_message_count: usize,
}

#[derive(Debug, Clone)]
pub struct ContextSnapshotRecord {
    pub context_json: String,
    pub assembled_messages_json: String,
    pub phase: aither_agent::ContextWindowPhase,
    pub metrics: aither_agent::ContextWindowMetrics,
    pub tool_surface_hash: Option<String>,
    pub has_background_tasks: bool,
}

impl ContextSnapshotRecord {
    pub fn from_checkpoint(
        checkpoint: &aither_agent::CheckpointContext<'_>,
    ) -> anyhow::Result<Self> {
        let assembled_messages_json = serde_json::to_string(&checkpoint.window.messages)?;
        Ok(Self {
            context_json: checkpoint.context_json.to_string(),
            assembled_messages_json,
            phase: checkpoint.window.phase,
            metrics: checkpoint.window.metrics.clone(),
            tool_surface_hash: Some(checkpoint.tool_surface_hash.to_string()),
            has_background_tasks: checkpoint.has_background_tasks,
        })
    }
}

pub fn parse_persisted_context_snapshot(
    snapshot: crate::db::entity::session_context_snapshots::Model,
) -> anyhow::Result<PersistedContextSnapshot> {
    let checkpoint: aither_agent::AgentCheckpoint = serde_json::from_str(&snapshot.context_json)?;
    let history_message_count = checkpoint
        .history_anchor
        .ok_or_else(|| anyhow::anyhow!("agent checkpoint is missing history_anchor"))?;
    Ok(PersistedContextSnapshot {
        checkpoint,
        history_message_count,
    })
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
    db.get_session_context_snapshot(session_id)
        .await
        .map_err(anyhow::Error::from)?
        .map(parse_persisted_context_snapshot)
        .transpose()
}

pub async fn snapshot_record_from_agent(
    agent: &mut MayAgent,
) -> anyhow::Result<ContextSnapshotRecord> {
    let checkpoint = agent
        .export_checkpoint(Some(0))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(ContextSnapshotRecord {
        context_json: checkpoint.context_json,
        assembled_messages_json: serde_json::to_string(&checkpoint.context_window.messages)?,
        phase: checkpoint.context_window.phase,
        metrics: checkpoint.context_window.metrics,
        tool_surface_hash: Some(checkpoint.tool_surface_hash),
        has_background_tasks: checkpoint.has_background_tasks,
    })
}

pub async fn persist_context_snapshot_record(
    db: &ChatDb,
    session_id: &str,
    mut record: ContextSnapshotRecord,
) -> anyhow::Result<()> {
    let history_message_count = db
        .llm_message_count(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let mut checkpoint: aither_agent::AgentCheckpoint = serde_json::from_str(&record.context_json)?;
    checkpoint.history_anchor = Some(history_message_count);
    record.context_json = serde_json::to_string(&checkpoint)?;
    let metadata_json = serde_json::to_string(&StoredContextWindowMetadata {
        phase: record.phase,
        metrics: record.metrics,
        history_message_count,
        tool_surface_hash: record.tool_surface_hash,
        has_background_tasks: record.has_background_tasks,
    })?;

    db.upsert_session_context_snapshot(
        session_id,
        crate::db::SessionContextSnapshotUpsert {
            context_json: &record.context_json,
            assembled_messages_json: Some(&record.assembled_messages_json),
            metadata_json: Some(&metadata_json),
        },
    )
    .await
    .map_err(anyhow::Error::from)
}

pub async fn persist_agent_context_snapshot(
    db: &ChatDb,
    session_id: &str,
    agent: &mut MayAgent,
) -> anyhow::Result<()> {
    let record = snapshot_record_from_agent(agent).await?;
    persist_context_snapshot_record(db, session_id, record).await
}
