//! Message row model for the chat history database.

/// Chat message row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Row id.
    pub id: i32,
    /// Owning session id.
    pub session_id: String,
    /// Message role (`user`/`assistant`/`tool`).
    pub role: String,
    /// Message body.
    pub content: String,
    /// Message timestamp (ISO 8601), when recorded.
    pub timestamp: Option<String>,
    /// Id of the tool call this message answers, if any.
    pub tool_call_id: Option<String>,
    /// Provider that produced the message.
    pub provider_id: Option<String>,
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
    pub elapsed_ms: Option<i64>,
}
