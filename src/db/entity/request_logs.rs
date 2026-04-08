//! Persisted request log row model for UI observability.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: i32,
    pub session_id: String,
    pub timestamp: String,
    pub model: String,
    pub provider_id: String,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub context_window: Option<i32>,
    pub elapsed_ms: i64,
    pub ok: bool,
    pub error: Option<String>,
}
