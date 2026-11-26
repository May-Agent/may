//! Persisted execution-mode row model for sessions.

/// Persisted execution mode of a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Execution mode tag (`standard`, `orchestrator`, ...).
    pub mode: String,
    /// Optional sidekick `provider::model` selector for paired execution modes.
    pub sidekick_model: Option<String>,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
