//! Persisted session time intervals behind the `/usage` wall-clock split.

/// One recorded span of a session's time.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Row id.
    pub id: i32,
    /// Session the span belongs to.
    pub session_id: String,
    /// What the session spent the span on: `tool` or `user_wait`.
    pub kind: String,
    /// Start, Unix milliseconds.
    pub start_ms: i64,
    /// End, Unix milliseconds.
    pub end_ms: i64,
}
