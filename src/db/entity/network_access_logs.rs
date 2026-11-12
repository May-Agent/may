//! Persisted network audit log row model.

/// One network access audit entry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Row id.
    pub id: i32,
    /// Session the access was attributed to, if any.
    pub session_id: Option<String>,
    /// Profile the access was attributed to, if any.
    pub profile_id: Option<String>,
    /// Which subsystem attempted the access.
    pub source: String,
    /// Access mode string.
    pub mode: String,
    /// Target host.
    pub domain: String,
    /// Target port.
    pub port: i32,
    /// Whether policy allowed the access.
    pub allowed: bool,
    /// Policy decision reason, if any.
    pub reason: Option<String>,
    /// Access timestamp (ISO 8601).
    pub timestamp: String,
}
