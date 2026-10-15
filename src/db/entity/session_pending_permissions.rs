//! Pending permission request row model owned by the server runtime.

/// A permission request awaiting a decision.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Request id.
    pub request_id: String,
    /// Owning session id.
    pub session_id: String,
    /// Requested permission mode.
    pub mode: String,
    /// Script or command the request covers.
    pub script: String,
    /// Request timestamp (ISO 8601).
    pub created_at: String,
}
