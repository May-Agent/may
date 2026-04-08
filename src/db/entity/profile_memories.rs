//! Per-profile memory entity for the chat history database.
//!
//! Stores preference and recency memory extracted from conversations
//! by a fast LLM, maintained asynchronously.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: String,
    /// Profile this memory belongs to.
    pub profile_id: String,
    /// Memory type: "preference" or "recency".
    pub memory_type: String,
    /// Memory content (markdown).
    pub content: String,
    pub updated_at: String,
}
