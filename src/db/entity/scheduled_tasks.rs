//! Scheduled task row model for the chat history database.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: String,
    /// Profile this task belongs to.
    pub profile_id: String,
    /// Optional session that created the task.
    pub session_id: Option<String>,
    /// Human-readable task name.
    pub name: String,
    /// Trigger kind/category: "time" or "event".
    pub trigger_kind: String,
    /// Time trigger type: "cron" or "once".
    pub trigger_type: String,
    /// Cron expression (for `trigger_type` = "cron").
    pub cron_expression: Option<String>,
    /// ISO 8601 UTC timestamp for one-time tasks (`trigger_type` = "once").
    pub run_at: Option<String>,
    /// Path to a script file to execute.
    pub script_path: Option<String>,
    /// Inline script content to execute.
    pub script_inline: Option<String>,
    /// Whether this task requires network access (heel-only flag).
    pub needs_network: bool,
    /// Whether the task has been approved for execution.
    pub approved: bool,
    /// Whether the task is enabled.
    pub enabled: bool,
    /// Last execution timestamp (ISO 8601 UTC).
    pub last_run_at: Option<String>,
    /// Last run status: "success" or "failed".
    pub last_run_status: Option<String>,
    /// Last run output (truncated).
    pub last_run_output: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
