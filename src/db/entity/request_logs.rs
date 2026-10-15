//! Persisted request log row model for UI observability.

/// One provider request log entry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Row id.
    pub id: i32,
    /// Session the request belongs to.
    pub session_id: String,
    /// Request timestamp (ISO 8601).
    pub timestamp: String,
    /// Model id the request targeted.
    pub model: String,
    /// Provider that served the request.
    pub provider_id: String,
    /// Prompt token count.
    pub prompt_tokens: Option<i32>,
    /// Completion token count.
    pub completion_tokens: Option<i32>,
    /// Total tokens billed.
    pub total_tokens: Option<i32>,
    /// Tokens served from the provider cache.
    pub cache_read_tokens: Option<i32>,
    /// Tokens written to the provider cache.
    pub cache_write_tokens: Option<i32>,
    /// Model context window at request time.
    pub context_window: Option<i32>,
    /// Request wall-clock time in milliseconds.
    pub elapsed_ms: i64,
    /// Whether the request succeeded.
    pub ok: bool,
    /// Error summary when `ok` is false.
    pub error: Option<String>,
}
