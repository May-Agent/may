//! Durable session run lease row model for active agent execution.

/// Durable lease recording an active agent run for a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Whether a run lease is held.
    pub active: bool,
    /// Whether cancellation was requested.
    pub cancellation_requested: bool,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
