//! Persisted plan-mode row model for sessions.

/// Persisted plan-mode state for a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Plan state string (`disabled`/`planning`/..).
    pub plan_state: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
