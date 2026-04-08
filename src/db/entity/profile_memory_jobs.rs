//! Per-profile durable memory refresh job row model.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub profile_id: String,
    pub dirty: bool,
    pub last_processed_at: Option<String>,
    pub updated_at: String,
}
