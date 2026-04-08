//! Persisted notification mailbox row model.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: String,
    pub title: String,
    pub body: String,
    pub session_id: Option<String>,
    pub callback_context_xml: Option<String>,
    pub callback_message: Option<String>,
    pub status: String,
    pub sent_count: i32,
    pub last_error: Option<String>,
    pub delivered_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
