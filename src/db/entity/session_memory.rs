//! Persisted memory switch row model for sessions.

/// Persisted memory switch of a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Owning session id.
    pub session_id: String,
    /// Whether reading and extracting memories are disabled for the rest of this session.
    pub disabled: bool,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
