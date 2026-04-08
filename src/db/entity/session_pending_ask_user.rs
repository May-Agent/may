//! Pending ask-user request row model owned by the server runtime.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub request_id: String,
    pub session_id: String,
    pub questions_json: String,
    pub created_at: String,
}
