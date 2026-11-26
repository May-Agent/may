//! Persisted companion-session link row model (for example a Fusion sidekick).

/// Link between a hidden companion session and its parent user session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Companion session id.
    pub session_id: String,
    /// Parent session id that owns this companion.
    pub parent_session_id: String,
    /// Companion role (`"sidekick"`).
    pub role: String,
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
}
