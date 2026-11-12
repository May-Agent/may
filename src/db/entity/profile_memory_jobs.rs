//! Per-profile durable memory refresh job row model.

/// Durable dirty-marker for asynchronous profile memory refresh.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Profile whose memories need rebuilding.
    pub profile_id: String,
    /// Whether new content awaits processing.
    pub dirty: bool,
    /// Last successful processing time (ISO 8601).
    pub last_processed_at: Option<String>,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
