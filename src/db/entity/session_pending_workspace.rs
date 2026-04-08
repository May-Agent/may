//! Pending workspace request row model owned by the server runtime.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub request_id: String,
    pub session_id: String,
    pub path: String,
    pub reason: String,
    pub created_at: String,
}
