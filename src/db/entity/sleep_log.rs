//! Persisted `/sleep` times.

/// One `/sleep`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Row id.
    pub id: i32,
    /// When the user ran `/sleep`, Unix milliseconds.
    pub slept_at_ms: i64,
}
