//! Scheduled task entity for the chat history database.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "scheduled_tasks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
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
    /// Cron expression (for trigger_type = "cron").
    pub cron_expression: Option<String>,
    /// ISO 8601 UTC timestamp for one-time tasks (trigger_type = "once").
    pub run_at: Option<String>,
    /// Path to a script file to execute.
    pub script_path: Option<String>,
    /// Inline script content to execute.
    pub script_inline: Option<String>,
    /// Whether this task requires network access (leash-only flag).
    #[sea_orm(default_value = "0")]
    pub needs_network: bool,
    /// Whether the task has been approved for execution.
    #[sea_orm(default_value = "0")]
    pub approved: bool,
    /// Whether the task is enabled.
    #[sea_orm(default_value = "1")]
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

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
