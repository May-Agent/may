//! Scheduled task execution row model.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    pub id: String,
    pub task_id: String,
    pub profile_id: String,
    pub session_id: Option<String>,
    pub invocation_kind: String,
    pub status: String,
    pub output: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub updated_at: String,
}
