use std::borrow::Cow;
use std::str::FromStr;

use aither_agent::tool_request::{
    ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};
use aither_core::llm::{Tool, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::task_trigger::{
    TRIGGER_TYPE_CRON, TRIGGER_TYPE_ONCE, TaskTriggerKind, normalize_event_name,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRuntimeKind {
    Local,
    Container,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TaskArgs {
    /// Human-readable task name.
    pub name: String,
    /// Trigger category (`time` or `event`).
    pub trigger_kind: String,
    /// Run once at this UTC timestamp (must end with `Z`).
    #[serde(default)]
    pub run_at_utc: Option<String>,
    /// Cron expression evaluated in UTC (e.g. `0 6 * * *`).
    #[serde(default)]
    pub cron_utc: Option<String>,
    /// Event name for `trigger_kind=event` (e.g. `chat.turn.completed`).
    #[serde(default)]
    pub event_name: Option<String>,
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
pub struct TaskTriggerSpec {
    pub trigger_type: String,
    pub run_at: Option<String>,
    pub cron_expression: Option<String>,
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
    pub trigger_kind: String,
    pub trigger: TaskTriggerSpec,
    pub script: TaskScriptSpec,
    pub needs_network: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreateResult {
    pub task_id: String,
    pub profile_id: String,
    pub trigger_kind: String,
    pub trigger_type: String,
    pub run_at: Option<String>,
    pub cron_expression: Option<String>,
    pub approved: bool,
    pub enabled: bool,
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

        let trigger_kind = normalize_trigger_kind(&self.args)?;
        let trigger = normalize_trigger(&self.args, trigger_kind)?;
        let script = normalize_script(&self.args)?;

        let needs_network = match self.runtime {
            TaskRuntimeKind::Local => self.args.needs_network.ok_or_else(|| {
                "local(leash) task must explicitly set needs_network to true or false".to_string()
            })?,
            TaskRuntimeKind::Container => false,
        };

        Ok(NormalizedTaskCreateRequest {
            session_id: self.session_id,
            name: name.to_string(),
            trigger_kind: trigger_kind.as_str().to_string(),
            trigger,
            script,
            needs_network,
        })
    }
}

fn normalize_trigger_kind(args: &TaskArgs) -> Result<TaskTriggerKind, String> {
    let raw = args.trigger_kind.trim();
    if raw.is_empty() {
        return Err("task trigger_kind must not be empty".to_string());
    }
    TaskTriggerKind::from_str(raw)
}

fn normalize_trigger(
    args: &TaskArgs,
    trigger_kind: TaskTriggerKind,
) -> Result<TaskTriggerSpec, String> {
    let run_at = args
        .run_at_utc
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let cron = args
        .cron_utc
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let event_name = args
        .event_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    match trigger_kind {
        TaskTriggerKind::Time => match (run_at, cron, event_name) {
            (Some(run_at), None, None) => {
                if !run_at.ends_with('Z') {
                    return Err(
                        "run_at_utc must be an ISO 8601 UTC timestamp ending with 'Z'".into(),
                    );
                }
                let parsed = jiff::Timestamp::from_str(&run_at)
                    .map_err(|e| format!("invalid run_at_utc timestamp: {e}"))?;
                Ok(TaskTriggerSpec {
                    trigger_type: TRIGGER_TYPE_ONCE.to_string(),
                    run_at: Some(parsed.to_string()),
                    cron_expression: None,
                })
            }
            (None, Some(cron_expression), None) => {
                cron::Schedule::from_str(&cron_expression)
                    .map_err(|e| format!("invalid cron_utc expression: {e}"))?;
                Ok(TaskTriggerSpec {
                    trigger_type: TRIGGER_TYPE_CRON.to_string(),
                    run_at: None,
                    cron_expression: Some(cron_expression),
                })
            }
            (Some(_), Some(_), _) => {
                Err("provide exactly one time trigger: run_at_utc or cron_utc".into())
            }
            (_, _, Some(_)) => Err("event_name is only valid when trigger_kind is 'event'".into()),
            (None, None, None) => Err("time trigger requires run_at_utc or cron_utc".into()),
        },
        TaskTriggerKind::Event => match (run_at, cron, event_name) {
            (None, None, Some(name)) => Ok(TaskTriggerSpec {
                trigger_type: normalize_event_name(&name)?,
                run_at: None,
                cron_expression: None,
            }),
            (Some(_), _, _) | (_, Some(_), _) => {
                Err("event trigger must not set run_at_utc or cron_utc".into())
            }
            (None, None, None) => Err("event trigger requires event_name".into()),
        },
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

        ToolOutput::json(&serde_json::json!({
            "ok": true,
            "task_id": model.task_id,
            "profile_id": model.profile_id,
            "trigger_kind": model.trigger_kind,
            "trigger_type": model.trigger_type,
            "run_at": model.run_at,
            "cron_expression": model.cron_expression,
            "approved": model.approved,
            "enabled": model.enabled,
            "approval_required": true
        }))
    }
}
