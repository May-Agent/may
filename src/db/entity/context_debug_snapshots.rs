//! Per-session context-debug snapshot row model.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub session_id: String,
    pub timestamp: String,
    pub model: String,
    pub provider_id: String,
    pub prompt: String,
    pub prompt_attachments_json: String,
    pub assembled_messages_json: String,
}
