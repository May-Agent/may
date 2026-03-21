use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::task_trigger::{NormalizedTaskTrigger, TaskTriggerRequest, TaskTriggerResponse};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRuntimeKind {
    Local,
    Container,
    Remote,
}

impl TaskRuntimeKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Container => "container",
            Self::Remote => "remote",
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
    /// Inline bash script to execute.
    #[serde(default)]
    pub script_inline: Option<String>,
    /// Required for local(leash) tasks. Ignored for container tasks.
    #[serde(default)]
    pub needs_network: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreateRequest {
    pub session_id: String,
    pub runtime: TaskRuntimeKind,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreateResult {
    pub task_id: String,
    pub profile_id: String,
    pub trigger: TaskTriggerResponse,
    pub approved: bool,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
struct TaskToolResponse {
    ok: bool,
    task_id: String,
    profile_id: String,
    trigger: TaskTriggerResponse,
    approved: bool,
    enabled: bool,
    approval_required: bool,
}

pub type TaskBroker = ToolRequestBroker<TaskCreateRequest, Result<TaskCreateResult, String>>;
pub type TaskQueue = ToolRequestQueue<TaskCreateRequest, Result<TaskCreateResult, String>>;

#[must_use]
pub fn channel() -> (TaskBroker, TaskQueue) {
    tool_request_channel()
}

impl TaskCreateRequest {
    pub fn normalize(self) -> Result<NormalizedTaskCreateRequest, String> {
        let name = self.args.name.trim();
        if name.is_empty() {
            return Err("task name must not be empty".to_string());
        }

        let trigger = self.args.trigger.normalize()?;
        let script = normalize_script(&self.args)?;

        let needs_network = match self.runtime {
            TaskRuntimeKind::Local => self.args.needs_network.ok_or_else(|| {
                "local(leash) task must explicitly set needs_network to true or false".to_string()
            })?,
            TaskRuntimeKind::Container | TaskRuntimeKind::Remote => false,
        };

        Ok(NormalizedTaskCreateRequest {
            session_id: self.session_id,
            name: name.to_string(),
            trigger,
            script,
            needs_network,
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
    pub fn new(session_id: String, runtime: TaskRuntimeKind, broker: Option<TaskBroker>) -> Self {
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

    async fn call(&self, args: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let broker = self
            .broker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("task command is unavailable in this runtime"))?;
        let request = TaskCreateRequest {
            session_id: self.session_id.clone(),
            runtime: self.runtime,
            args,
        };
        let response = broker.request(request).await?;
        let model = response.map_err(anyhow::Error::msg)?;

        ToolOutput::json(&TaskToolResponse {
            ok: true,
            task_id: model.task_id,
            profile_id: model.profile_id,
            trigger: model.trigger,
            approved: model.approved,
            enabled: model.enabled,
            approval_required: true,
        })
    }
}
