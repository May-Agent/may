//! Pending workspace request row model owned by the server runtime.

/// A workspace-access request awaiting a decision.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Request id.
    pub request_id: String,
    /// Owning session id.
    pub session_id: String,
    /// Directory being requested.
    pub path: String,
    /// Why access is needed.
    pub reason: String,
    /// Request timestamp (ISO 8601).
    pub created_at: String,
}
