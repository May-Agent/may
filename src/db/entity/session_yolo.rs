//! Persisted yolo-mode row model for sessions.

/// Persisted yolo switch of a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Whether every approval request of the session is granted at once.
    pub enabled: bool,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
