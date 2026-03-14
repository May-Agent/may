use aither_core::llm::Message as LlmMessage;

use crate::{MayAgent, db::ChatDb};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StoredContextWindowMetadata {
    pub phase: aither_agent::ContextWindowPhase,
    pub metrics: aither_agent::ContextWindowMetrics,
    #[serde(default)]
    pub history_message_count: usize,
}

#[derive(Debug, Clone)]
pub struct PersistedContextSnapshot {
    pub context_json: String,
    pub history_message_count: usize,
}

pub fn parse_persisted_context_snapshot(
    snapshot: crate::db::entity::session_context_snapshots::Model,
) -> anyhow::Result<PersistedContextSnapshot> {
    let metadata_json = snapshot
        .metadata_json
        .ok_or_else(|| anyhow::anyhow!("session context snapshot is missing metadata_json"))?;
    let metadata: StoredContextWindowMetadata = serde_json::from_str(&metadata_json)?;
    Ok(PersistedContextSnapshot {
        context_json: snapshot.context_json,
        history_message_count: metadata.history_message_count,
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

pub async fn persist_agent_context_snapshot(
    db: &ChatDb,
    session_id: &str,
    agent: &mut MayAgent,
) -> anyhow::Result<()> {
    let context_json = agent.export_context_json().map_err(anyhow::Error::from)?;
    let window_snapshot = agent.snapshot_context_window().await;
    let assembled_messages_json = serde_json::to_string(&window_snapshot.messages)?;
    let metadata_json = serde_json::to_string(&StoredContextWindowMetadata {
        phase: window_snapshot.phase,
        metrics: window_snapshot.metrics,
        history_message_count: db
            .llm_message_count(session_id)
            .await
            .map_err(anyhow::Error::from)?,
    })?;

    db.upsert_session_context_snapshot(
        session_id,
        crate::db::SessionContextSnapshotUpsert {
            context_json: &context_json,
            assembled_messages_json: Some(&assembled_messages_json),
            metadata_json: Some(&metadata_json),
        },
    )
    .await
    .map_err(anyhow::Error::from)
}
