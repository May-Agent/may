//! Scheduled task row model for the chat history database.

/// Scheduled task row model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    /// Task row ID.
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
    /// Maximum runtime of one run in seconds; the run is killed when it passes.
    pub timeout_secs: i64,
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
    /// Creation timestamp (ISO 8601).
    pub created_at: String,
    /// Last update timestamp (ISO 8601).
    pub updated_at: String,
    /// Secret name whose value keys the signature a webhook delivery must carry.
    pub webhook_signature_secret: Option<String>,
    /// Lowercase request header the webhook signature arrives in.
    pub webhook_signature_header: Option<String>,
    /// The word the model knows the task by, from its session's pool; a task
    /// bound to no session has none.
    pub word: Option<String>,
    /// When a time-triggered task next fires (ISO 8601 UTC). Advanced in the
    /// write that starts the fire's run; empty for other triggers and for a
    /// one-time task that has run.
    pub next_fire_at: Option<String>,
}

impl Model {
    /// The first fire of this task after `after`, for a time trigger; `None`
    /// for other triggers and for a one-time task that already ran
    /// (`time_run_exists`).
    ///
    /// # Errors
    /// Returns a message when the stored trigger or its schedule is not valid.
    pub fn next_fire_after(
        &self,
        after: jiff::Timestamp,
        time_run_exists: bool,
    ) -> Result<Option<jiff::Timestamp>, String> {
        use crate::task_trigger::{PersistedTaskTrigger, TimeTriggerType};
        let PersistedTaskTrigger::Time { trigger_type } = self.persisted_trigger()? else {
            return Ok(None);
        };
        if trigger_type == TimeTriggerType::Once && time_run_exists {
            return Ok(None);
        }
        trigger_type
            .next_fire(
                self.cron_expression.as_deref(),
                self.run_at.as_deref(),
                after,
            )
            .map(Some)
    }

    /// The trigger this row stores.
    ///
    /// # Errors
    /// Returns a message when the stored trigger is not valid.
    pub fn persisted_trigger(&self) -> Result<crate::task_trigger::PersistedTaskTrigger, String> {
        let signature = match (
            &self.webhook_signature_secret,
            &self.webhook_signature_header,
        ) {
            (Some(secret), Some(header)) => Some(crate::task_trigger::WebhookSignature {
                secret: secret.clone(),
                header: header.clone(),
            }),
            _ => None,
        };
        crate::task_trigger::PersistedTaskTrigger::from_storage(
            &self.trigger_kind,
            &self.trigger_type,
            signature,
        )
    }
}
