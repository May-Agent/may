//! Scheduled task execution runtime.
//!
//! Background tokio task that ticks every 30 seconds, checks for due tasks
//! (cron or one-time), and executes them through profile-specific runtimes.

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use aither_core::llm::Tool;
use aither_sandbox::{
    AnyContainerExec, BollardContainerExec, Configured, ContainerLaunchSpec, ContainerShellRuntime,
    OutputFormat, PermissionHandler, ShellRuntimeAvailability, ShellSessionRegistry, TerminalArgs,
    TerminalExecutionMode, TerminalMode, TerminalResult, TerminalTool, ToolRegistryBuilder,
    permission::PermissionError,
};
use arc_swap::ArcSwap;
use askama::Template;
use base64::Engine;
use executor_core::DefaultExecutor;
use serde::Serialize;
use shell_escape::escape as shell_escape;

use crate::agent::task_tool::{TaskCreateRequest, TaskCreateResult, TaskRuntimeKind};
use crate::container::lifecycle::ContainerManager;
use crate::db::entity::{profiles, scheduled_tasks};
use crate::db::{ChatDb, NetworkAccessLogParams, NetworkAccessMode, NetworkAccessSource};
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyConfig, NetworkPolicyError};
use crate::notification::{NotificationPayload, NotificationService};
use crate::profile::ProfileEnvironment;
use crate::task_trigger::{
    PersistedTaskTrigger, TaskInvocation, TaskTriggerResponse, TaskWebhookRequest, TimeTriggerType,
    normalize_event_name,
};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// Shared task execution facility used by the scheduler, REST API, and agent tools.
#[derive(Debug, Clone)]
pub struct TaskRuntime {
    db: ChatDb,
    notifier: Option<Arc<NotificationService>>,
    ui_event_bus: Option<UiEventBus>,
    may_dir: PathBuf,
    container_manager: Option<Arc<ContainerManager>>,
    /// Baseline tool distribution prepared for every task terminal.
    tool_distribution: Arc<aither_sandbox::ToolDistribution>,
    network_policy: Arc<ArcSwap<Option<CompiledNetworkPolicy>>>,
    active_tasks: Arc<ArcSwap<TaskCatalog>>,
}

/// Time-based wrapper that periodically asks [`TaskRuntime`] to run due tasks.
#[derive(Debug)]
pub struct TaskScheduler {
    runtime: Arc<TaskRuntime>,
}

#[derive(Template)]
#[template(path = "task_validate_script.sh", escape = "none")]
struct TaskValidationScriptTemplate<'a> {
    validate_path: &'a str,
    encoded_script: &'a str,
}

#[derive(Template)]
#[template(path = "task_execute_script.sh", escape = "none")]
struct TaskExecutionScriptTemplate<'a> {
    trigger_file: &'a str,
    script_file: &'a str,
}

#[derive(Debug, Default)]
struct TaskCatalog {
    tasks: Vec<scheduled_tasks::Model>,
}

/// Terminal state of a scheduled task run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    /// Run is in progress.
    Running,
    /// Run completed successfully.
    Success,
    /// Run failed.
    Failed,
    /// Run was interrupted (restart/shutdown).
    Interrupted,
}

impl TaskRunStatus {
    /// Persisted `snake_case` string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse a persisted status string.
    ///
    /// # Errors
    /// Returns the unrecognized value when it is not a known status.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(format!("unknown task run status: {other}")),
        }
    }
}

impl TaskCatalog {
    const fn from_tasks(tasks: Vec<scheduled_tasks::Model>) -> Self {
        Self { tasks }
    }
}

fn prefixed_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    format!("{prefix}{error}")
}

fn profile_load_error(profile_id: &str, error: &impl std::fmt::Display) -> String {
    format!("failed to load profile {profile_id}: {error}")
}

fn profile_not_found_error(profile_id: &str) -> String {
    format!("profile {profile_id} not found")
}

fn invalid_profile_error(profile_id: &str, error: &impl std::fmt::Display) -> String {
    format!("invalid profile {profile_id}: {error}")
}

fn session_load_error(session_id: &str, error: &impl std::fmt::Display) -> String {
    format!("failed to load session {session_id}: {error}")
}

fn session_not_found_error(session_id: &str) -> String {
    format!("session not found: {session_id}")
}

fn task_notification_title(task_name: &str) -> String {
    format!("Task: {task_name}")
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum TaskCallbackStatus {
    Completed,
    Failed,
}

impl TaskCallbackStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    const fn notification_body(self) -> &'static str {
        match self {
            Self::Completed => "Completed successfully",
            Self::Failed => "Execution failed",
        }
    }
}

fn task_notification_callback_message(task_name: &str, status: TaskCallbackStatus) -> String {
    format!(
        "Continue from scheduled task \"{task_name}\" ({}). Review the hidden task result and choose the next action.",
        status.as_str()
    )
}

fn task_script_failure(exit_code: i32, output: &str) -> String {
    format!("task script failed with exit code {exit_code}\n{output}")
}

fn temp_task_file_name(prefix: &str, execution_id: &str, suffix: &str) -> String {
    format!("{prefix}{execution_id}{suffix}")
}

fn profile_workspace_key(profile: &profiles::Model) -> &str {
    profile.slug.as_deref().unwrap_or(&profile.id)
}

fn container_profile_name(profile: &profiles::Model) -> String {
    format!("may-{}", profile_workspace_key(profile))
}

fn task_notification_callback_context(
    task: &scheduled_tasks::Model,
    status: TaskCallbackStatus,
    summary: &str,
) -> Option<crate::XmlBlock> {
    task.session_id.as_ref()?;
    let xml = crate::xml::serialize_xml_block(
        "system-reminder",
        &TaskNotificationCallbackXml {
            alert_type: "task_callback",
            task_id: task.id.as_str(),
            task_name: task.name.as_str(),
            profile_id: task.profile_id.as_str(),
            trigger_kind: task.trigger_kind.as_str(),
            trigger_type: task.trigger_type.as_str(),
            status: status.as_str(),
            summary,
        },
    )
    .ok()?;
    crate::XmlBlock::parse(xml).ok()
}

fn task_notification_callback_payload(
    task: &scheduled_tasks::Model,
    status: TaskCallbackStatus,
    summary: &str,
) -> (Option<crate::XmlBlock>, Option<String>) {
    if task.session_id.is_none() {
        return (None, None);
    }

    (
        task_notification_callback_context(task, status, summary),
        Some(task_notification_callback_message(
            task.name.as_str(),
            status,
        )),
    )
}

const fn task_db_error(message: String) -> crate::db::ChatDbError {
    crate::db::ChatDbError::Storage(message)
}

fn task_field_error(task_id: &str, prefix: &str, field: &str) -> crate::db::ChatDbError {
    task_db_error(format!("{prefix}{task_id} is missing {field}"))
}

fn task_value_error(
    task_id: &str,
    prefix: &str,
    error: &impl std::fmt::Display,
) -> crate::db::ChatDbError {
    task_db_error(format!("{prefix}{task_id}: {error}"))
}

fn stored_task_trigger(
    task: &scheduled_tasks::Model,
) -> Result<PersistedTaskTrigger, crate::db::ChatDbError> {
    PersistedTaskTrigger::from_storage(task.trigger_kind.as_str(), task.trigger_type.as_str())
        .map_err(|error| {
            task_value_error(task.id.as_str(), "invalid stored trigger for task ", &error)
        })
}

fn workspace_prepare_error(path: &std::path::Path, error: &impl std::fmt::Display) -> String {
    format!("failed to prepare workspace {}: {error}", path.display())
}

fn init_terminal_tool_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    prefixed_error(prefix, error)
}

fn invalid_profile_component_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    prefixed_error(prefix, error)
}

#[derive(Debug, Clone)]
struct TaskScript {
    content: String,
    host_working_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct MaterializedTaskExecution {
    command: String,
    cleanup_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct TaskExecutionEnvelope {
    task: TaskExecutionTask,
    invocation: TaskInvocation,
}

#[derive(Debug, Clone, Serialize)]
struct TaskExecutionTask {
    id: String,
    name: String,
    profile_id: String,
    session_id: Option<String>,
    trigger: TaskTriggerResponse,
}

#[derive(Debug, Clone, Serialize)]
struct TaskNotificationCallbackXml<'a> {
    #[serde(rename = "@type")]
    alert_type: &'static str,
    task_id: &'a str,
    task_name: &'a str,
    profile_id: &'a str,
    trigger_kind: &'a str,
    trigger_type: &'a str,
    status: &'a str,
    summary: &'a str,
}

impl TaskExecutionEnvelope {
    fn new(task: &scheduled_tasks::Model, invocation: TaskInvocation) -> Result<Self, String> {
        let trigger = PersistedTaskTrigger::from_storage(
            task.trigger_kind.as_str(),
            task.trigger_type.as_str(),
        )
        .map_err(|error| prefixed_error("invalid persisted task trigger: ", &error))?
        .response(task.run_at.clone(), task.cron_expression.clone());
        Ok(Self {
            task: TaskExecutionTask {
                id: task.id.clone(),
                name: task.name.clone(),
                profile_id: task.profile_id.clone(),
                session_id: task.session_id.clone(),
                trigger,
            },
            invocation,
        })
    }
}

#[derive(Debug, Clone)]
struct SchedulerPermissionHandler {
    db: ChatDb,
    ui_event_bus: Option<UiEventBus>,
    profile_id: String,
    source: NetworkAccessSource,
    allow_network: bool,
    network_policy: Option<CompiledNetworkPolicy>,
}

impl PermissionHandler for SchedulerPermissionHandler {
    fn check(
        &self,
        _mode: TerminalMode,
        _script: &str,
    ) -> impl Future<Output = Result<bool, PermissionError>> + Send {
        std::future::ready(Ok(true))
    }

    async fn check_domain(&self, domain: &str, port: u16) -> bool {
        let reason = if self.allow_network {
            self.network_policy
                .as_ref()
                .map(|policy| policy.mode().as_str())
        } else {
            Some("needs_network_disabled")
        };
        let _ = self
            .db
            .append_network_access_log(NetworkAccessLogParams {
                session_id: None,
                profile_id: Some(self.profile_id.as_str()),
                source: self.source,
                mode: NetworkAccessMode::Network,
                domain,
                port,
                allowed: true,
                reason,
            })
            .await;
        if let Some(ui_event_bus) = self.ui_event_bus.as_ref() {
            ui_event_bus.publish(UiEventTopics::NETWORK_ACCESS_LOGS);
        }
        true
    }
}

impl TaskRuntime {
    /// Build the runtime: load config, open the database, and restore active tasks.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] when config loading, database setup, or task restoration fails.
    pub async fn load(
        db: ChatDb,
        may_dir: PathBuf,
        tool_distribution: Arc<aither_sandbox::ToolDistribution>,
    ) -> Result<Self, crate::db::ChatDbError> {
        let runtime = Self {
            db,
            notifier: None,
            ui_event_bus: None,
            may_dir,
            container_manager: None,
            tool_distribution,
            network_policy: Arc::new(ArcSwap::from_pointee(None)),
            active_tasks: Arc::new(ArcSwap::from_pointee(TaskCatalog::default())),
        };
        runtime.reload_active_tasks().await?;
        Ok(runtime)
    }

    /// Attach a notification manager for sending push notifications on task completion.
    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<NotificationService>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Attach the UI event bus used for runtime notifications.
    #[must_use]
    pub fn with_ui_event_bus(mut self, ui_event_bus: UiEventBus) -> Self {
        self.ui_event_bus = Some(ui_event_bus);
        self
    }

    /// Attach container manager for container-profile task execution.
    #[must_use]
    pub fn with_container_manager(mut self, container_manager: Arc<ContainerManager>) -> Self {
        self.container_manager = Some(container_manager);
        self
    }

    /// Set the compiled network policy used for container task runs.
    ///
    /// # Errors
    /// Returns [`NetworkPolicyError`] when the policy fails to compile.
    pub fn with_network_policy(
        self,
        network_policy: NetworkPolicyConfig,
    ) -> Result<Self, NetworkPolicyError> {
        self.set_network_policy(&network_policy)?;
        Ok(self)
    }

    /// Replace the network policy used for container task runs.
    ///
    /// # Errors
    /// Returns [`NetworkPolicyError`] when the policy fails to compile.
    pub fn set_network_policy(
        &self,
        network_policy: &NetworkPolicyConfig,
    ) -> Result<(), NetworkPolicyError> {
        let compiled = network_policy.compile()?;
        self.network_policy.store(Arc::new(compiled));
        Ok(())
    }

    /// Reload due/active tasks from the database into the in-memory set.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn reload_active_tasks(&self) -> Result<(), crate::db::ChatDbError> {
        let tasks = self.db.list_due_tasks().await?;
        self.active_tasks
            .store(Arc::new(TaskCatalog::from_tasks(tasks)));
        Ok(())
    }

    /// Validate that a task script can be parsed in the target profile runtime.
    ///
    /// # Errors
    /// Returns a validation error message when the profile lookup fails or the script fails validation.
    pub async fn validate_task_script_for_profile(
        &self,
        profile_id: &str,
        script_path: Option<&str>,
        script_inline: Option<&str>,
    ) -> Result<(), String> {
        let profile = self
            .db
            .get_profile(profile_id)
            .await
            .map_err(|error| profile_load_error(profile_id, &error))?
            .ok_or_else(|| profile_not_found_error(profile_id))?;
        Box::pin(self.validate_task_script_for_profile_model(&profile, script_path, script_inline))
            .await
    }

    /// Create a scheduled task requested by the in-chat `task` tool.
    /// Persist a scheduled task created through the `task` tool, validating its script first.
    ///
    /// # Errors
    /// Returns an error message when the payload is malformed, script validation fails, or the insert errors.
    pub async fn create_task_from_tool_request(
        &self,
        request: TaskCreateRequest,
    ) -> Result<TaskCreateResult, String> {
        let runtime_kind = request.runtime;
        let normalized = request.normalize()?;

        let session = self
            .db
            .get_session(&normalized.session_id)
            .await
            .map_err(|error| session_load_error(normalized.session_id.as_str(), &error))?
            .ok_or_else(|| session_not_found_error(normalized.session_id.as_str()))?;

        let profile_id = session.profile_id.ok_or_else(|| {
            format!(
                "session {} is not attached to a profile",
                normalized.session_id
            )
        })?;
        let profile = self
            .db
            .get_profile(&profile_id)
            .await
            .map_err(|error| profile_load_error(profile_id.as_str(), &error))?
            .ok_or_else(|| profile_not_found_error(profile_id.as_str()))?;
        let profile_info = self
            .db
            .profile_info_from_model(profile.clone())
            .await
            .map_err(|error| invalid_profile_error(profile_id.as_str(), &error))?;
        let actual_runtime = match profile_info.environment.kind() {
            ProfileEnvironment::Local => TaskRuntimeKind::Local,
            ProfileEnvironment::Container { .. } => TaskRuntimeKind::Container,
        };
        if runtime_kind != actual_runtime {
            return Err(format!(
                "task runtime mismatch: session profile uses {}, request declared {}",
                actual_runtime.as_str(),
                runtime_kind.as_str()
            ));
        }

        Box::pin(self.validate_task_script_for_profile_model(
            &profile,
            normalized.script.script_path.as_deref(),
            normalized.script.script_inline.as_deref(),
        ))
        .await?;

        let task_id = uuid::Uuid::new_v4().to_string();
        let persisted_trigger = normalized.trigger.clone().into_persisted();
        let model = self
            .db
            .create_scheduled_task(crate::db::CreateScheduledTaskParams {
                id: &task_id,
                profile_id: &profile_id,
                session_id: Some(normalized.session_id.as_str()),
                name: &normalized.name,
                trigger: &persisted_trigger,
                cron_expression: normalized.trigger.cron_expression(),
                run_at: normalized.trigger.run_at(),
                script_path: normalized.script.script_path.as_deref(),
                script_inline: normalized.script.script_inline.as_deref(),
                needs_network: normalized.needs_network,
            })
            .await
            .map_err(|error| prefixed_error("failed to create scheduled task: ", &error))?;
        self.reload_active_tasks()
            .await
            .map_err(|e| e.to_string())?;
        let trigger = PersistedTaskTrigger::from_storage(
            model.trigger_kind.as_str(),
            model.trigger_type.as_str(),
        )
        .map_err(|error| prefixed_error("failed to decode persisted task trigger: ", &error))?
        .response(model.run_at.clone(), model.cron_expression.clone());

        Ok(TaskCreateResult {
            task_id: model.id,
            profile_id: model.profile_id,
            trigger,
            approved: model.approved,
            enabled: model.enabled,
        })
    }

    /// Run a task immediately outside its schedule.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure or task spawn failure.
    pub async fn run_manual_task(
        &self,
        task: scheduled_tasks::Model,
    ) -> Result<(), crate::db::ChatDbError> {
        self.spawn_task(task, TaskInvocation::Manual).await
    }

    /// Check for due tasks and execute them.
    /// Dispatch every active task whose time trigger is due.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] when task rows fail to load or spawn.
    ///
    /// # Panics
    /// Panics if `is_due` reports a non-time trigger as due (internal invariant).
    pub async fn tick_due_tasks(&self) -> Result<(), crate::db::ChatDbError> {
        let tasks = self.active_tasks.load();
        let now = jiff::Timestamp::now();

        for task in &tasks.tasks {
            let trigger = stored_task_trigger(task)?;
            let is_due = Self::is_due(task, &trigger, now)?;

            if is_due {
                let PersistedTaskTrigger::Time { trigger_type } = trigger else {
                    panic!("is_due only returns true for time triggers");
                };
                self.spawn_task(task.clone(), TaskInvocation::Time { trigger_type })
                    .await?;
            }
        }

        Ok(())
    }

    /// Dispatch event-triggered tasks.
    ///
    /// Returns the number of matching tasks that were started.
    /// Run every enabled event-triggered task whose event name matches.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure or task spawn failure.
    pub async fn dispatch_event(
        &self,
        event_name: &str,
        profile_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<usize, crate::db::ChatDbError> {
        let event_name =
            normalize_event_name(event_name).map_err(crate::db::ChatDbError::Storage)?;
        let tasks = self.active_tasks.load();
        let mut dispatched = 0usize;

        for task in &tasks.tasks {
            let trigger = stored_task_trigger(task)?;
            let PersistedTaskTrigger::Event {
                event_name: task_event_name,
            } = trigger
            else {
                continue;
            };
            if task_event_name != event_name {
                continue;
            }
            if let Some(expected_profile) = profile_id
                && task.profile_id != expected_profile
            {
                continue;
            }
            if let Some(expected_session) = session_id
                && task.session_id.as_deref() != Some(expected_session)
            {
                continue;
            }
            dispatched += 1;
            self.spawn_task(
                task.clone(),
                TaskInvocation::Event {
                    event_name: event_name.clone(),
                    profile_id: profile_id.map(ToOwned::to_owned),
                    session_id: session_id.map(ToOwned::to_owned),
                },
            )
            .await?;
        }

        Ok(dispatched)
    }

    /// Run every enabled task whose webhook trigger matches `token`.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure or task spawn failure.
    pub async fn dispatch_webhook(
        &self,
        token: String,
        request: TaskWebhookRequest,
    ) -> Result<usize, crate::db::ChatDbError> {
        let tasks = self.db.list_tasks_for_webhook(&token).await?;
        let triggered = tasks.len();
        for task in tasks {
            self.spawn_task(
                task,
                TaskInvocation::Webhook {
                    token: token.clone(),
                    request: request.clone(),
                },
            )
            .await?;
        }
        Ok(triggered)
    }

    fn is_due(
        task: &scheduled_tasks::Model,
        trigger: &PersistedTaskTrigger,
        now: jiff::Timestamp,
    ) -> Result<bool, crate::db::ChatDbError> {
        match trigger {
            PersistedTaskTrigger::Time {
                trigger_type: TimeTriggerType::Cron,
            } => Self::is_cron_due(task, now),
            PersistedTaskTrigger::Time {
                trigger_type: TimeTriggerType::Once,
            } => Self::is_once_due(task, now),
            PersistedTaskTrigger::Event { .. } | PersistedTaskTrigger::Webhook { .. } => Ok(false),
        }
    }

    /// Check if a cron task is due.
    fn is_cron_due(
        task: &scheduled_tasks::Model,
        now: jiff::Timestamp,
    ) -> Result<bool, crate::db::ChatDbError> {
        let Some(ref expr) = task.cron_expression else {
            return Err(task_field_error(
                task.id.as_str(),
                "cron task ",
                "cron_expression",
            ));
        };

        let schedule = cron::Schedule::from_str(expr).map_err(|error| {
            task_value_error(
                task.id.as_str(),
                "invalid cron expression stored in task ",
                &error,
            )
        })?;

        // Check if any scheduled time falls within the last 30 seconds.
        let now_chrono = timestamp_to_chrono(now);
        let window_start = now_chrono - chrono::Duration::seconds(30);

        // Skip if we already ran within this window.
        if let Some(ref last_run) = task.last_run_at {
            let last = jiff::Timestamp::from_str(last_run).map_err(|error| {
                task_value_error(
                    task.id.as_str(),
                    "invalid last_run_at stored in task ",
                    &error,
                )
            })?;
            let last_chrono = timestamp_to_chrono(last);
            if last_chrono > window_start {
                return Ok(false);
            }
        }

        Ok(schedule
            .after(&window_start)
            .take(1)
            .any(|next| next <= now_chrono))
    }

    /// Check if a one-time task is due.
    fn is_once_due(
        task: &scheduled_tasks::Model,
        now: jiff::Timestamp,
    ) -> Result<bool, crate::db::ChatDbError> {
        let Some(ref run_at) = task.run_at else {
            return Err(task_field_error(
                task.id.as_str(),
                "one-time task ",
                "run_at",
            ));
        };

        // Skip if already executed.
        if task.last_run_at.is_some() {
            return Ok(false);
        }

        let target = jiff::Timestamp::from_str(run_at).map_err(|error| {
            task_value_error(task.id.as_str(), "invalid run_at stored in task ", &error)
        })?;

        Ok(now >= target)
    }

    async fn spawn_task(
        &self,
        task: scheduled_tasks::Model,
        invocation: TaskInvocation,
    ) -> Result<(), crate::db::ChatDbError> {
        let run_id = uuid::Uuid::new_v4().to_string();
        self.db
            .start_task_run(&run_id, &task, Self::invocation_kind(&invocation))
            .await?;
        let runtime = self.clone();
        tokio::spawn(async move {
            Box::pin(runtime.execute_task(run_id.as_str(), &task, invocation)).await;
        });
        Ok(())
    }

    async fn validate_task_script_for_profile_model(
        &self,
        profile: &profiles::Model,
        script_path: Option<&str>,
        script_inline: Option<&str>,
    ) -> Result<(), String> {
        let profile_info = self
            .db
            .profile_info_from_model(profile.clone())
            .await
            .map_err(|error| invalid_profile_error(profile.id.as_str(), &error))?;
        let script = self
            .resolve_script_source(script_path, script_inline)
            .await?;

        match profile_info.environment.kind() {
            ProfileEnvironment::Local => self.validate_local(&profile.id, &script).await,
            ProfileEnvironment::Container { .. } => {
                Box::pin(self.validate_container(profile, &script)).await
            }
        }
    }

    /// Execute a task (run its script).
    async fn execute_task(
        &self,
        run_id: &str,
        task: &scheduled_tasks::Model,
        invocation: TaskInvocation,
    ) {
        tracing::info!(
            task_id = %task.id,
            name = %task.name,
            trigger = %task.trigger_type,
            profile_id = %task.profile_id,
            "Executing scheduled task"
        );

        let result = Box::pin(self.execute_task_inner(task, invocation)).await;

        match result {
            Ok(output) => {
                let truncated = truncate_output(&output);
                if let Err(e) = self
                    .db
                    .record_task_run(&task.id, TaskRunStatus::Success, Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, error = %e, "Failed to record task run");
                }
                if let Err(error) = self
                    .db
                    .finish_task_run(run_id, TaskRunStatus::Success, Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, run_id, error = %error, "Failed to record task run history");
                }

                if let Some(ref notifier) = self.notifier {
                    let (callback_context_xml, callback_message) =
                        task_notification_callback_payload(
                            task,
                            TaskCallbackStatus::Completed,
                            truncated.as_str(),
                        );
                    let payload = NotificationPayload {
                        notification_id: None,
                        title: task_notification_title(task.name.as_str()),
                        body: TaskCallbackStatus::Completed
                            .notification_body()
                            .to_string(),
                        session_id: task.session_id.clone(),
                        callback_context_xml,
                        callback_message,
                    };
                    if let Err(e) = notifier.send_payload(payload).await {
                        tracing::warn!(task_id = %task.id, error = %e, "Failed to send push notification");
                    }
                }
                if let Err(error) = self.reload_active_tasks().await {
                    tracing::error!(task_id = %task.id, error = %error, "Failed to refresh active task cache");
                }

                tracing::info!(task_id = %task.id, "Scheduled task completed");
            }
            Err(error_message) => {
                let truncated = truncate_output(&error_message);
                if let Err(e) = self
                    .db
                    .record_task_run(&task.id, TaskRunStatus::Failed, Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, error = %e, "Failed to record task run");
                }
                if let Err(error) = self
                    .db
                    .finish_task_run(run_id, TaskRunStatus::Failed, Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, run_id, error = %error, "Failed to record task run history");
                }

                if let Some(ref notifier) = self.notifier {
                    let (callback_context_xml, callback_message) =
                        task_notification_callback_payload(
                            task,
                            TaskCallbackStatus::Failed,
                            truncated.as_str(),
                        );
                    let payload = NotificationPayload {
                        notification_id: None,
                        title: task_notification_title(task.name.as_str()),
                        body: TaskCallbackStatus::Failed.notification_body().to_string(),
                        session_id: task.session_id.clone(),
                        callback_context_xml,
                        callback_message,
                    };
                    if let Err(e) = notifier.send_payload(payload).await {
                        tracing::warn!(task_id = %task.id, error = %e, "Failed to send push notification");
                    }
                }
                if let Err(error) = self.reload_active_tasks().await {
                    tracing::error!(task_id = %task.id, error = %error, "Failed to refresh active task cache");
                }

                tracing::error!(task_id = %task.id, error = %error_message, "Scheduled task execution failed");
            }
        }
    }

    async fn execute_task_inner(
        &self,
        task: &scheduled_tasks::Model,
        invocation: TaskInvocation,
    ) -> Result<String, String> {
        let profile = self
            .db
            .get_profile(&task.profile_id)
            .await
            .map_err(|error| profile_load_error(task.profile_id.as_str(), &error))?
            .ok_or_else(|| profile_not_found_error(task.profile_id.as_str()))?;
        let profile_info = self
            .db
            .profile_info_from_model(profile.clone())
            .await
            .map_err(|error| invalid_profile_error(task.profile_id.as_str(), &error))?;

        let script = self.resolve_script(task).await?;
        let materialized = self
            .materialize_task_execution(task, &script, invocation)
            .await?;

        let result = match profile_info.environment.kind() {
            ProfileEnvironment::Local => {
                self.execute_local(task, &script, materialized.command.clone())
                    .await
            }
            ProfileEnvironment::Container { .. } => {
                Box::pin(self.execute_container(task, &profile, materialized.command.clone())).await
            }
        };

        self.cleanup_materialized_execution(materialized.cleanup_paths)
            .await;
        result
    }

    async fn materialize_task_execution(
        &self,
        task: &scheduled_tasks::Model,
        script: &TaskScript,
        invocation: TaskInvocation,
    ) -> Result<MaterializedTaskExecution, String> {
        let execution_id = uuid::Uuid::new_v4().to_string();
        let script_path = script.host_working_dir.join(temp_task_file_name(
            ".may-task-script-",
            execution_id.as_str(),
            ".sh",
        ));
        let trigger_path = script.host_working_dir.join(temp_task_file_name(
            ".may-task-trigger-",
            execution_id.as_str(),
            ".json",
        ));
        let trigger_payload = serde_json::to_vec(
            &TaskExecutionEnvelope::new(task, invocation).map_err(|error| {
                prefixed_error("failed to build task trigger payload: ", &error)
            })?,
        )
        .map_err(|error| prefixed_error("failed to encode task trigger payload: ", &error))?;

        tokio::fs::write(&script_path, script.content.as_bytes())
            .await
            .map_err(|error| {
                format!(
                    "failed to write task script {}: {error}",
                    script_path.display()
                )
            })?;
        tokio::fs::write(&trigger_path, trigger_payload)
            .await
            .map_err(|error| {
                let script_path = script_path.clone();
                tokio::spawn(async move {
                    let _ = tokio::fs::remove_file(script_path).await;
                });
                format!(
                    "failed to write task trigger payload {}: {error}",
                    trigger_path.display()
                )
            })?;

        let trigger_file =
            shell_escape(Cow::Owned(trigger_path.display().to_string())).into_owned();
        let script_file = shell_escape(Cow::Owned(script_path.display().to_string())).into_owned();
        let command = TaskExecutionScriptTemplate {
            trigger_file: &trigger_file,
            script_file: &script_file,
        }
        .render()
        .expect("task execution template must render");

        Ok(MaterializedTaskExecution {
            command,
            cleanup_paths: vec![script_path, trigger_path],
        })
    }

    async fn cleanup_materialized_execution(&self, cleanup_paths: Vec<PathBuf>) {
        for path in cleanup_paths {
            if let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), error = %error, "failed to remove materialized task file");
            }
        }
    }

    async fn resolve_script_source(
        &self,
        script_path: Option<&str>,
        script_inline: Option<&str>,
    ) -> Result<TaskScript, String> {
        match (
            script_inline.map(str::trim).filter(|s| !s.is_empty()),
            script_path.map(str::trim).filter(|s| !s.is_empty()),
        ) {
            (Some(inline), None) => Ok(TaskScript {
                content: inline.to_string(),
                host_working_dir: self.may_dir.clone(),
            }),
            (None, Some(path)) => {
                let canonical = tokio::fs::canonicalize(path)
                    .await
                    .map_err(|error| prefixed_error("failed to resolve script_path: ", &error))?;
                let content = tokio::fs::read_to_string(&canonical)
                    .await
                    .map_err(|error| {
                        format!(
                            "failed to read script_path {}: {error}",
                            canonical.display()
                        )
                    })?;
                let working_dir = canonical
                    .parent()
                    .ok_or_else(|| {
                        format!(
                            "script_path {} has no parent directory",
                            canonical.display()
                        )
                    })?
                    .to_path_buf();
                Ok(TaskScript {
                    content,
                    host_working_dir: working_dir,
                })
            }
            (Some(_), Some(_)) => Err("task has both script_inline and script_path".to_string()),
            (None, None) => Err("task has neither script_inline nor script_path".to_string()),
        }
    }

    async fn resolve_script(&self, task: &scheduled_tasks::Model) -> Result<TaskScript, String> {
        self.resolve_script_source(task.script_path.as_deref(), task.script_inline.as_deref())
            .await
    }

    async fn execute_local(
        &self,
        task: &scheduled_tasks::Model,
        script: &TaskScript,
        command: String,
    ) -> Result<String, String> {
        let terminal_tool = self
            .local_terminal_tool(
                &task.profile_id,
                NetworkAccessSource::TaskLocal,
                script,
                task.needs_network,
            )
            .await?;
        run_terminal_command(
            &terminal_tool,
            local_task_execution_mode(task.needs_network),
            None,
            command,
        )
        .await
    }

    async fn validate_local(&self, profile_id: &str, script: &TaskScript) -> Result<(), String> {
        let terminal_tool = self
            .local_terminal_tool(
                profile_id,
                NetworkAccessSource::TaskValidationLocal,
                script,
                false,
            )
            .await?;

        run_terminal_command(
            &terminal_tool,
            local_task_execution_mode(false),
            None,
            build_validation_command(script),
        )
        .await
        .map(|_| ())
    }

    /// Builds a terminal over `shell_sessions`, grants `workspace` read-write
    /// access and prepares the baseline tools through
    /// [`TerminalTool::initialize_tools`], so scheduled commands run the same
    /// verified tools — including the read-only and version checks — as agent
    /// terminals. The command priority is read from the current config on
    /// every launch, so a saved change reaches the next scheduled command
    /// without global state.
    async fn initialized_terminal_tool(
        &self,
        permission_handler: SchedulerPermissionHandler,
        workspace: PathBuf,
        shell_sessions: ShellSessionRegistry,
    ) -> Result<TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured>, String> {
        let (terminal_tool, (network_audit, priority)) = futures_lite::future::try_zip(
            async {
                TerminalTool::new_exact(&workspace, permission_handler, DefaultExecutor)
                    .await
                    .map_err(|error| {
                        init_terminal_tool_error("failed to initialize terminal tool: ", &error)
                    })
            },
            futures_lite::future::try_zip(
                async {
                    aither_sandbox::open_network_audit_log(
                        self.may_dir.join(crate::config::NETWORK_AUDIT_LOG_SUBDIR),
                        crate::config::NETWORK_AUDIT_MAX_FILES,
                    )
                    .await
                    .map_err(|error| {
                        init_terminal_tool_error("failed to open network audit log: ", &error)
                    })
                },
                async {
                    crate::config::MayConfig::load_from(&self.may_dir.join("config.toml"))
                        .await
                        .map(|config| config.sandbox.command_priority.process_priority())
                        .map_err(|error| {
                            init_terminal_tool_error(
                                "failed to load May config for command priority: ",
                                &error,
                            )
                        })
                },
            ),
        )
        .await?;
        let registry = Arc::new(ToolRegistryBuilder::new().build(workspace.join("outputs")));
        terminal_tool
            .with_network_audit_log(network_audit)
            .with_shell_sessions(shell_sessions)
            .with_writable_paths([workspace.clone()])
            .with_readable_paths([workspace])
            .with_process_priority(priority)
            .initialize_tools(
                &self.tool_distribution,
                &crate::session::MayHomePaths::new(self.may_dir.clone()).tools_dir(),
            )
            .await
            .map(|terminal_tool| terminal_tool.with_registry(registry))
            .map_err(|error| init_terminal_tool_error("failed to prepare baseline tools: ", &error))
    }

    async fn local_terminal_tool(
        &self,
        profile_id: &str,
        source: NetworkAccessSource,
        script: &TaskScript,
        allow_network: bool,
    ) -> Result<TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured>, String> {
        self.initialized_terminal_tool(
            SchedulerPermissionHandler {
                db: self.db.clone(),
                ui_event_bus: self.ui_event_bus.clone(),
                profile_id: profile_id.to_string(),
                source,
                allow_network,
                network_policy: self.network_policy.load().as_ref().clone(),
            },
            script.host_working_dir.clone(),
            ShellSessionRegistry::new(ShellRuntimeAvailability {
                local: true,
                container: false,
                ssh: false,
            }),
        )
        .await
    }

    /// Starts the profile's container and builds a terminal that executes in
    /// it, storing outputs in the host-side task workspace mounted at
    /// `/workspace`. Returns the container manager and container ID so callers
    /// can record activity.
    async fn container_terminal_tool(
        &self,
        profile: &profiles::Model,
        source: NetworkAccessSource,
    ) -> Result<
        (
            TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured>,
            Arc<ContainerManager>,
            String,
        ),
        String,
    > {
        let container_manager = self.container_manager.clone().ok_or_else(|| {
            "container profile task cannot run: container runtime unavailable".to_string()
        })?;
        let container_preferences = self
            .db
            .load_container_profile_preferences(profile)
            .await
            .map_err(|error| {
                invalid_profile_component_error("invalid container profile preferences: ", &error)
            })?;

        let default_image_tag = Box::pin(crate::container::profile::default_image_tag(
            &self.may_dir,
            profile,
            container_preferences.clone(),
        ))
        .await
        .map_err(|error| {
            invalid_profile_component_error("invalid container profile image tag: ", &error)
        })?;
        let image_spec = crate::container::profile::image_spec_for_profile(
            &self.may_dir,
            profile,
            &default_image_tag,
            container_preferences.clone(),
        )
        .map_err(|error| {
            invalid_profile_component_error(
                "invalid container profile image configuration: ",
                &error,
            )
        })?;
        let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences)
            .map_err(|error| {
                invalid_profile_component_error(
                    "invalid container profile mount configuration: ",
                    &error,
                )
            })?;

        crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
            .await
            .map_err(|error| prefixed_error("container image check/build failed: ", &error))?;

        let container_name = container_profile_name(profile);
        let workspace_dir = self
            .may_dir
            .join("task-workspaces")
            .join(profile_workspace_key(profile));
        tokio::fs::create_dir_all(&workspace_dir)
            .await
            .map_err(|error| workspace_prepare_error(&workspace_dir, &error))?;

        let mut launch_spec = ContainerLaunchSpec::new(
            container_name,
            image_spec.tag.clone(),
            workspace_dir.clone(),
        );
        for mount in extra_mounts {
            launch_spec = launch_spec.with_mount(mount);
        }
        let container_id = container_manager
            .ensure_running(&launch_spec)
            .await
            .map_err(|error| prefixed_error("failed to start container: ", &error))?;
        container_manager.touch_activity(&container_id);

        let container_ipc_route = container_manager
            .runtime()
            .ipc_route(&container_id)
            .await
            .map_err(|error| prefixed_error("failed to resolve container IPC route: ", &error))?;
        let container_runtime = ContainerShellRuntime::new(
            container_id.clone(),
            container_ipc_route,
            AnyContainerExec::new(Arc::new(BollardContainerExec::new(Arc::new(
                container_manager.runtime().client.clone(),
            )))),
        );
        let terminal_tool = self
            .initialized_terminal_tool(
                SchedulerPermissionHandler {
                    db: self.db.clone(),
                    ui_event_bus: self.ui_event_bus.clone(),
                    profile_id: profile.id.clone(),
                    source,
                    allow_network: true,
                    network_policy: self.network_policy.load().as_ref().clone(),
                },
                workspace_dir,
                ShellSessionRegistry::new(ShellRuntimeAvailability {
                    local: false,
                    container: true,
                    ssh: false,
                })
                .with_container_runtime(container_runtime),
            )
            .await?;
        Ok((terminal_tool, container_manager, container_id))
    }

    async fn execute_container(
        &self,
        _task: &scheduled_tasks::Model,
        profile: &profiles::Model,
        command: String,
    ) -> Result<String, String> {
        let (terminal_tool, container_manager, container_id) =
            Box::pin(self.container_terminal_tool(profile, NetworkAccessSource::TaskContainer))
                .await?;
        let result = run_terminal_command(
            &terminal_tool,
            TerminalExecutionMode::Default,
            None,
            command,
        )
        .await;
        container_manager.touch_activity(&container_id);
        result
    }

    #[must_use]
    const fn invocation_kind(invocation: &TaskInvocation) -> &'static str {
        match invocation {
            TaskInvocation::Time { .. } => "time",
            TaskInvocation::Manual => "manual",
            TaskInvocation::Event { .. } => "event",
            TaskInvocation::Webhook { .. } => "webhook",
        }
    }

    async fn validate_container(
        &self,
        profile: &profiles::Model,
        script: &TaskScript,
    ) -> Result<(), String> {
        let (terminal_tool, container_manager, container_id) = Box::pin(
            self.container_terminal_tool(profile, NetworkAccessSource::TaskValidationContainer),
        )
        .await?;
        let result = run_terminal_command(
            &terminal_tool,
            TerminalExecutionMode::Default,
            None,
            build_validation_command(script),
        )
        .await
        .map(|_| ());
        container_manager.touch_activity(&container_id);
        result
    }
}

const fn local_task_execution_mode(needs_network: bool) -> TerminalExecutionMode {
    if needs_network {
        TerminalExecutionMode::Default
    } else {
        TerminalExecutionMode::Sandboxed
    }
}

impl TaskScheduler {
    /// Create the handler over the shared task runtime.
    #[must_use]
    pub const fn new(runtime: Arc<TaskRuntime>) -> Self {
        Self { runtime }
    }

    /// Spawn the scheduler as a background task.
    #[must_use]
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = self.runtime.tick_due_tasks().await {
                    tracing::error!(error = %e, "Scheduler tick failed");
                }
            }
        })
    }
}

async fn run_terminal_command(
    terminal_tool: &TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured>,
    mode: TerminalExecutionMode,
    ssh_server_id: Option<String>,
    script: String,
) -> Result<String, String> {
    let output = terminal_tool
        .call(
            TerminalArgs {
                description: "run the scheduled task script".to_string(),
                script,
                mode,
                ssh_server_id,
                expect: OutputFormat::Text,
                resolution: aither_sandbox::MediaResolution::Auto,
                timeout: 300,
                max_lines: 800,
                raw: false,
            },
            aither_core::llm::ToolContext::new(),
        )
        .await
        .map_err(|error| prefixed_error("task terminal execution failed: ", &error))?;

    if output.is_error() {
        return Err(output
            .error_message()
            .unwrap_or("task terminal execution failed")
            .to_string());
    }
    let payload = output
        .render_for_model()
        .map_err(|error| prefixed_error("failed to render terminal tool result: ", &error))?;

    parse_terminal_result(&payload)
}

fn parse_terminal_result(payload: &str) -> Result<String, String> {
    let result: TerminalResult = serde_json::from_str(payload)
        .map_err(|error| prefixed_error("failed to parse terminal result: ", &error))?;

    if result.exit_code != 0 {
        return Err(task_script_failure(
            result.exit_code,
            combine_output(
                &result.stdout.to_string(),
                result.stderr.as_ref().map(ToString::to_string),
            )
            .as_str(),
        ));
    }

    Ok(combine_output(
        &result.stdout.to_string(),
        result.stderr.as_ref().map(ToString::to_string),
    ))
}

fn combine_output(stdout: &str, stderr: Option<String>) -> String {
    match stderr {
        Some(stderr_text) if !stderr_text.trim().is_empty() => {
            format!("{stdout}\n--- stderr ---\n{stderr_text}")
        }
        _ => stdout.to_string(),
    }
}

fn truncate_output(output: &str) -> String {
    if output.len() > 10240 {
        format!("{}...(truncated)", &output[..10240])
    } else {
        output.to_string()
    }
}

fn build_validation_command(script: &TaskScript) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(script.content.as_bytes());
    let encoded_script = shell_escape(Cow::Borrowed(encoded.as_str())).into_owned();
    TaskValidationScriptTemplate {
        validate_path: ".may-task-validate.sh",
        encoded_script: &encoded_script,
    }
    .render()
    .expect("task validation template must render")
}

/// Convert a `jiff::Timestamp` to `chrono::DateTime`<chrono::Utc> for cron compatibility.
fn timestamp_to_chrono(ts: jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(
        ts.as_second(),
        u32::try_from(ts.subsec_nanosecond()).expect("jiff subsecond nanos are non-negative"),
    )
    .expect("jiff timestamp must fit within chrono DateTime range")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{TaskRuntime, TaskScript, local_task_execution_mode};
    use aither_sandbox::TerminalExecutionMode;
    use tempfile::tempdir;

    use crate::db::{ChatDb, CreateScheduledTaskParams, NetworkAccessSource};
    use crate::task_trigger::PersistedTaskTrigger;

    #[test]
    fn local_tasks_respect_needs_network_flag() {
        assert_eq!(
            local_task_execution_mode(true),
            TerminalExecutionMode::Default
        );
        assert_eq!(
            local_task_execution_mode(false),
            TerminalExecutionMode::Sandboxed
        );
    }

    #[tokio::test]
    async fn dispatch_event_filters_by_profile_and_session() {
        let dir = tempdir().expect("temp dir should be created");
        let may_dir = dir.path().join(".may");
        std::fs::create_dir_all(&may_dir).expect("may dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize");
        db.ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        let local_profile = db
            .get_profile_by_slug("local")
            .await
            .expect("profile lookup should succeed")
            .expect("local profile should exist");

        for (task_id, session_id, approved) in [
            ("task-match", Some("session-a"), true),
            ("task-other-session", Some("session-b"), true),
            ("task-unapproved", Some("session-a"), false),
        ] {
            let trigger = PersistedTaskTrigger::Event {
                event_name: "chat.turn.completed".to_string(),
            };
            db.create_scheduled_task(CreateScheduledTaskParams {
                id: task_id,
                profile_id: local_profile.id.as_str(),
                session_id,
                name: task_id,
                trigger: &trigger,
                cron_expression: None,
                run_at: None,
                script_path: None,
                script_inline: Some("echo hi"),
                needs_network: false,
            })
            .await
            .expect("task should persist");
            if approved {
                db.approve_scheduled_task(task_id)
                    .await
                    .expect("task should be approved");
            }
        }

        let runtime = TaskRuntime::load(
            db.clone(),
            may_dir,
            Arc::new(crate::tools::embedded_distribution().expect("embedded distribution")),
        )
        .await
        .expect("task runtime should load");

        let triggered = runtime
            .dispatch_event(
                "chat.turn.completed",
                Some(local_profile.id.as_str()),
                Some("session-a"),
            )
            .await
            .expect("event dispatch should succeed");

        assert_eq!(triggered, 1);
    }

    #[tokio::test]
    async fn local_terminal_fails_before_exec_without_prepared_tools() {
        // Terminal construction spawns its job registry on the global
        // executor, which the `may` binary installs at startup.
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let dir = tempdir().expect("temp dir should be created");
        let may_dir = dir.path().join(".may");
        std::fs::create_dir_all(&may_dir).expect("may dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize");
        let empty_distribution =
            aither_sandbox::ToolDistribution::from_json(br#"{"artifacts":[]}"#)
                .expect("an empty distribution is well-formed");
        let runtime = TaskRuntime::load(db, may_dir.clone(), Arc::new(empty_distribution))
            .await
            .expect("task runtime should load");

        let error = runtime
            .local_terminal_tool(
                "profile",
                NetworkAccessSource::TaskValidationLocal,
                &TaskScript {
                    content: "true".to_string(),
                    host_working_dir: may_dir.join("task"),
                },
                false,
            )
            .await
            .expect_err("a distribution without native tools must fail before any exec");
        assert!(
            error.starts_with("failed to prepare baseline tools: "),
            "unexpected error: {error}"
        );
    }
}
