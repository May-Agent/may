//! Durable session run lease row model for active agent execution.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub session_id: String,
    pub active: bool,
    pub cancellation_requested: bool,
    pub updated_at: String,
}
