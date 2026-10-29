use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::task_trigger::{NormalizedTaskTrigger, TaskTriggerRequest, TaskTriggerResponse};

/// Runtime a scheduled task executes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRuntimeKind {
    /// Runs on the host via the local heel runtime.
    Local,
    /// Runs inside the session's container.
    Container,
}

impl TaskRuntimeKind {
    /// Return the serialized name of the runtime kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Container => "container",
        }
    }
}

impl std::fmt::Display for TaskRuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TaskArgs {
    /// Human-readable task name.
    pub name: String,
    /// Trigger definition.
    pub trigger: TaskTriggerRequest,
    /// Path to a shell script file to execute.
    #[serde(default)]
    pub script_path: Option<String>,
    /// Inline terminal command payload to execute.
    #[serde(default)]
    pub script_inline: Option<String>,
    /// Required for local(heel) tasks. Ignored for container tasks.
    #[serde(default)]
    pub needs_network: Option<bool>,
    /// Maximum runtime of one run in seconds; the run is killed when it passes.
    pub timeout_secs: u32,
}

/// Request to create a scheduled task, delivered over the task broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreateRequest {
    /// Session the task belongs to.
    pub session_id: String,
    /// Runtime the task executes in.
    pub runtime: TaskRuntimeKind,
    /// Task arguments supplied by the tool call.
    pub args: TaskArgs,
}

#[derive(Debug, Clone)]
pub struct TaskScriptSpec {
    pub script_path: Option<String>,
    pub script_inline: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NormalizedTaskCreateRequest {
    pub session_id: String,
    pub name: String,
    pub trigger: NormalizedTaskTrigger,
    pub script: TaskScriptSpec,
    pub needs_network: bool,
    pub timeout_secs: u32,
}

/// Outcome of a handled task-create request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreateResult {
    /// ID assigned to the created task.
    pub task_id: String,
    /// Profile the task was created under.
    pub profile_id: String,
    /// Normalized trigger stored for the task.
    pub trigger: TaskTriggerResponse,
    /// Whether the user approved creating the task.
    pub approved: bool,
    /// Whether the task is enabled to fire.
    pub enabled: bool,
}

/// What the model reads after `task` created a scheduled task.
#[derive(Debug, Template)]
#[template(path = "task_created.txt", escape = "none")]
struct TaskCreatedText {
    task_id: String,
    profile_id: String,
    trigger: TaskTriggerResponse,
    approved: bool,
    enabled: bool,
}

/// Broker half of the task-request channel: submits create requests to the
/// session runtime.
pub type TaskBroker = ToolRequestBroker<TaskCreateRequest, Result<TaskCreateResult, String>>;
/// Queue half of the task-request channel: the session runtime drains pending
/// create requests from it.
pub type TaskQueue = ToolRequestQueue<TaskCreateRequest, Result<TaskCreateResult, String>>;

#[derive(Debug, thiserror::Error)]
enum TaskToolError {
    #[error("task command is unavailable in this runtime")]
    Unavailable,
    #[error("task request was rejected: {0}")]
    Rejected(String),
}

/// Create a paired task broker and queue for a session.
#[must_use]
pub fn channel() -> (TaskBroker, TaskQueue) {
    tool_request_channel()
}

impl TaskCreateRequest {
    /// Validate the request and produce the normalized form used to create the
    /// task.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when the task name is empty, the trigger
    /// fails normalization, the script spec is invalid (both or neither of
    /// `script_path`/`script_inline` set), or a local task does not set
    /// `needs_network` explicitly.
    pub fn normalize(self) -> Result<NormalizedTaskCreateRequest, String> {
        let name = self.args.name.trim();
        if name.is_empty() {
            return Err("task name must not be empty".to_string());
        }

        let trigger = self.args.trigger.normalize()?;
        let script = normalize_script(&self.args)?;

        let needs_network = match self.runtime {
            TaskRuntimeKind::Local => self.args.needs_network.ok_or_else(|| {
                "local(heel) task must explicitly set needs_network to true or false".to_string()
            })?,
            TaskRuntimeKind::Container => false,
        };

        Ok(NormalizedTaskCreateRequest {
            session_id: self.session_id,
            name: name.to_string(),
            trigger,
            script,
            needs_network,
            timeout_secs: self.args.timeout_secs,
        })
    }
}

fn normalize_script(args: &TaskArgs) -> Result<TaskScriptSpec, String> {
    let script_path = args
        .script_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let script_inline = args
        .script_inline
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    match (&script_path, &script_inline) {
        (Some(_), Some(_)) => {
            Err("provide either script_path or script_inline, not both".to_string())
        }
        (None, None) => Err("task script is required: script_path or script_inline".to_string()),
        _ => Ok(TaskScriptSpec {
            script_path,
            script_inline,
        }),
    }
}

#[derive(Debug, Clone)]
pub struct TaskTool {
    session_id: String,
    runtime: TaskRuntimeKind,
    broker: Option<TaskBroker>,
}

impl TaskTool {
    #[must_use]
    pub const fn new(
        session_id: String,
        runtime: TaskRuntimeKind,
        broker: Option<TaskBroker>,
    ) -> Self {
        Self {
            session_id,
            runtime,
            broker,
        }
    }
}

impl Tool for TaskTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("task")
    }

    type Arguments = TaskArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let broker = self.broker.as_ref().ok_or(TaskToolError::Unavailable)?;
        let request = TaskCreateRequest {
            session_id: self.session_id.clone(),
            runtime: self.runtime,
            args,
        };
        let response = broker.request(request).await?;
        let model = response.map_err(TaskToolError::Rejected)?;

        let text = TaskCreatedText {
            task_id: model.task_id,
            profile_id: model.profile_id,
            trigger: model.trigger,
            approved: model.approved,
            enabled: model.enabled,
        }
        .render()
        .expect("task created template must render");
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::TaskCreatedText;
    use crate::task_trigger::{TaskTriggerResponse, TimeTriggerType};

    fn text(trigger: TaskTriggerResponse, approved: bool, enabled: bool) -> String {
        TaskCreatedText {
            task_id: "t1".to_string(),
            profile_id: "p1".to_string(),
            trigger,
            approved,
            enabled,
        }
        .render()
        .expect("task created template must render")
    }

    #[test]
    fn once_trigger_result_names_its_time_and_approval_state() {
        assert_eq!(
            text(
                TaskTriggerResponse::Time {
                    trigger_type: TimeTriggerType::Once,
                    run_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
                    cron_utc: None,
                },
                false,
                false,
            ),
            "Scheduled task t1 was created under profile p1. It runs on a time trigger (once) at 2026-01-01T00:00:00Z UTC. The task is not approved and disabled. It does not run until the user approves it."
        );
    }

    #[test]
    fn cron_trigger_result_names_its_expression() {
        assert_eq!(
            text(
                TaskTriggerResponse::Time {
                    trigger_type: TimeTriggerType::Cron,
                    run_at_utc: None,
                    cron_utc: Some("0 9 * * *".to_string()),
                },
                true,
                true,
            ),
            "Scheduled task t1 was created under profile p1. It runs on a time trigger (cron) with cron expression 0 9 * * * (UTC). The task is approved and enabled."
        );
    }

    #[test]
    fn event_and_webhook_trigger_results_name_what_fires_them() {
        assert_eq!(
            text(
                TaskTriggerResponse::Event {
                    event_name: "deploy".to_string(),
                },
                true,
                true,
            ),
            "Scheduled task t1 was created under profile p1. It runs when the event deploy is emitted. The task is approved and enabled."
        );
        assert_eq!(
            text(
                TaskTriggerResponse::Webhook {
                    token: "secret".to_string(),
                    webhook_path: "/hooks/secret".to_string(),
                },
                true,
                false,
            ),
            "Scheduled task t1 was created under profile p1. It runs when its webhook path /hooks/secret is called. The task is approved and disabled."
        );
    }
}
