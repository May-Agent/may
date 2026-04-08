//! Persisted plan-mode row model for sessions.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub session_id: String,
    pub plan_state: String,
    pub updated_at: String,
}
