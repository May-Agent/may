//! Pending permission request row model owned by the server runtime.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub request_id: String,
    pub session_id: String,
    pub mode: String,
    pub script: String,
    pub created_at: String,
}
