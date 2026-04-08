//! Pending user message row model owned by the server runtime.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: i64,
    pub session_id: String,
    pub delivery_mode: String,
    pub content: String,
    pub attachments_json: String,
    pub hidden_context_xml: Option<String>,
    pub created_at: String,
}
