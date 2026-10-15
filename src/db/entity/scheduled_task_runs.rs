//! Scheduled task execution row model.

/// One scheduled task execution record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Run id.
    pub id: String,
    /// Task that ran.
    pub task_id: String,
    /// Profile the task ran under.
    pub profile_id: String,
    /// Session produced by the run, if any.
    pub session_id: Option<String>,
    /// What triggered the run (`time`, `event`, `webhook`, `manual`).
    pub invocation_kind: String,
    /// Run status string (`success`/`failed`/`running`).
    pub status: String,
    /// Captured run output (truncated).
    pub output: Option<String>,
    /// Start timestamp (ISO 8601).
    pub started_at: String,
    /// Completion timestamp (ISO 8601), when finished.
    pub finished_at: Option<String>,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
}
