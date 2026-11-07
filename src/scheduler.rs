//! Scheduled task execution runtime.
//!
//! Background tokio task that ticks every 30 seconds, checks for due tasks
//! (cron or one-time), and executes them through profile-specific runtimes.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use aither_core::llm::CommandOutcome;
use aither_sandbox::{
    AnyContainerExec, BollardContainerExec, CompletedCommand, Configured, ContainerLaunchSpec,
    ContainerShellRuntime, JobStatus, OutputFormat, PermissionHandler, ShellRuntimeAvailability,
    ShellSessionRegistry, TerminalArgs, TerminalExecutionMode, TerminalMode, TerminalTool,
    ToolRegistryBuilder, permission::PermissionError,
};
use arc_swap::ArcSwap;
use askama::Template;
use base64::Engine;
use executor_core::DefaultExecutor;
use serde::Serialize;
use shell_escape::escape as shell_escape;

use crate::agent::task_tool::{TaskCreateRequest, TaskCreateResult, TaskRuntimeKind};
use crate::container::lifecycle::{ContainerManager, container_workspace_path};
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

/// What a webhook delivery did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookOutcome {
    /// No enabled, approved task has the presented token.
    NotFound,
    /// Tasks match the token but every one required a signature the delivery lacked.
    SignatureRefused,
    /// This many tasks were started.
    Triggered(usize),
}

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
    /// The daemon's connectivity source; task terminals record the outages
    /// their commands ran through.
    connectivity: Option<aither_connectivity::Connectivity>,
    /// The daemon's jobserver pool local task commands draw jobs from.
    job_server: Option<crate::jobserver::JobServer>,
    /// Where a run's `wake` command stores its prompt.
    session_prompts: Option<crate::task_wake::SessionPrompts>,
    /// Where webhook signing secrets are read.
    secrets: crate::secrets::SecretStore,
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

#[derive(Template)]
#[template(path = "task_validate_script_file.sh", escape = "none")]
struct TaskFileValidationTemplate<'a> {
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

#[derive(Debug, Clone, Copy)]
enum TaskOutcome {
    Completed,
    Failed,
}

impl TaskOutcome {
    const fn notification_body(self) -> &'static str {
        match self {
            Self::Completed => "Completed successfully",
            Self::Failed => "Execution failed",
        }
    }
}

fn task_script_failure(exit_code: i32, output: &str) -> String {
    format!("task script failed with exit code {exit_code}\n{output}")
}

fn temp_task_file_name(prefix: &str, execution_id: &str, suffix: &str) -> String {
    format!("{prefix}{execution_id}{suffix}")
}

/// Creates the directory a local task runs in; an existing one is kept.
async fn prepare_working_dir(dir: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|error| workspace_prepare_error(dir, &error))
}

fn profile_workspace_key(profile: &profiles::Model) -> &str {
    profile.slug.as_deref().unwrap_or(&profile.id)
}

fn container_profile_name(profile: &profiles::Model) -> String {
    format!("may-{}", profile_workspace_key(profile))
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
    task.persisted_trigger().map_err(|error| {
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

/// The task whose run a terminal serves, which gets the `wake` command.
/// Validation runs have none.
#[derive(Debug, Clone, Copy)]
struct WakeScope<'a> {
    task: &'a scheduled_tasks::Model,
    in_container: bool,
}

#[derive(Debug, Clone)]
struct TaskScript {
    content: String,
    host_working_dir: PathBuf,
}

/// Where a task's script and trigger files are written on the host, and how
/// the command that reads them refers to them. A container sees the host
/// directory only through its workspace mount, so the two differ there.
#[derive(Debug, Clone)]
struct TaskFilePlacement {
    host_dir: PathBuf,
    in_container: bool,
}

impl TaskFilePlacement {
    const fn local(host_dir: PathBuf) -> Self {
        Self {
            host_dir,
            in_container: false,
        }
    }

    /// Files go into the container's host-side `workspace_dir`, which the
    /// container mounts at [`CONTAINER_WORKSPACE`](crate::container::lifecycle::CONTAINER_WORKSPACE).
    const fn container(workspace_dir: PathBuf) -> Self {
        Self {
            host_dir: workspace_dir,
            in_container: true,
        }
    }

    /// The path of `file_name` as the command's shell sees it.
    fn command_path(&self, file_name: &str) -> String {
        if self.in_container {
            container_workspace_path(file_name)
        } else {
            self.host_dir.join(file_name).display().to_string()
        }
    }

    async fn write(&self, file_name: String, contents: &[u8]) -> Result<PlacedTaskFile, String> {
        let host_path = self.host_dir.join(&file_name);
        tokio::fs::write(&host_path, contents)
            .await
            .map_err(|error| {
                format!("failed to write task file {}: {error}", host_path.display())
            })?;
        Ok(PlacedTaskFile {
            command_path: self.command_path(&file_name),
            host_path,
        })
    }
}

#[derive(Debug, Clone)]
struct PlacedTaskFile {
    host_path: PathBuf,
    command_path: String,
}

#[derive(Debug, Clone)]
struct MaterializedTaskExecution {
    command: String,
    cleanup_paths: Vec<PathBuf>,
}

fn task_execution_command(trigger_file: &str, script_file: &str) -> String {
    let trigger_file = shell_escape(Cow::Borrowed(trigger_file));
    let script_file = shell_escape(Cow::Borrowed(script_file));
    TaskExecutionScriptTemplate {
        trigger_file: &trigger_file,
        script_file: &script_file,
    }
    .render()
    .expect("task execution template must render")
}

fn task_file_validation_command(script_file: &str) -> String {
    let script_file = shell_escape(Cow::Borrowed(script_file));
    TaskFileValidationTemplate {
        script_file: &script_file,
    }
    .render()
    .expect("task file validation template must render")
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

impl TaskExecutionEnvelope {
    fn new(task: &scheduled_tasks::Model, invocation: TaskInvocation) -> Result<Self, String> {
        let trigger = task
            .persisted_trigger()
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
        _language: aither_sandbox::ScriptLanguage,
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
            secrets: crate::secrets::SecretStore::new(&may_dir),
            may_dir,
            container_manager: None,
            tool_distribution,
            network_policy: Arc::new(ArcSwap::from_pointee(None)),
            active_tasks: Arc::new(ArcSwap::from_pointee(TaskCatalog::default())),
            connectivity: None,
            job_server: None,
            session_prompts: None,
        };
        runtime.reload_active_tasks().await?;
        Ok(runtime)
    }

    /// Read webhook signing secrets from `secrets` instead of the OS keychain.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_secret_store(mut self, secrets: crate::secrets::SecretStore) -> Self {
        self.secrets = secrets;
        self
    }

    /// Give task runs a `wake` command that prompts the session that created
    /// the task through `session_prompts`.
    #[must_use]
    pub fn with_session_prompts(
        mut self,
        session_prompts: crate::task_wake::SessionPrompts,
    ) -> Self {
        self.session_prompts = Some(session_prompts);
        self
    }

    /// Share the daemon's jobserver pool with the commands of local tasks.
    #[must_use]
    pub fn with_job_server(mut self, job_server: crate::jobserver::JobServer) -> Self {
        self.job_server = Some(job_server);
        self
    }

    /// Record the sleep and offline intervals commands of local tasks run
    /// through.
    #[must_use]
    pub fn with_connectivity(mut self, connectivity: aither_connectivity::Connectivity) -> Self {
        self.connectivity = Some(connectivity);
        self
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
        network_policy: &NetworkPolicyConfig,
    ) -> Result<Self, NetworkPolicyError> {
        self.set_network_policy(network_policy)?;
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

    /// Delete a task, then its working directory, which only it owns, and
    /// drop it from the active set.
    ///
    /// # Errors
    /// Returns a message when the row or the directory cannot be removed.
    pub async fn delete_task(&self, id: &str) -> Result<(), String> {
        self.db
            .delete_scheduled_task(id)
            .await
            .map_err(|error| prefixed_error("failed to delete task: ", &error))?;
        let dir = crate::session::MayHomePaths::new(self.may_dir.clone()).inline_task_dir(id);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to remove task directory {}: {error}",
                    dir.display()
                ));
            }
        }
        self.reload_active_tasks()
            .await
            .map_err(|error| prefixed_error("failed to reload tasks: ", &error))
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
        timeout_secs: u32,
    ) -> Result<(), String> {
        let profile = self
            .db
            .get_profile(profile_id)
            .await
            .map_err(|error| profile_load_error(profile_id, &error))?
            .ok_or_else(|| profile_not_found_error(profile_id))?;
        Box::pin(self.validate_task_script_for_profile_model(
            &profile,
            script_path,
            script_inline,
            timeout_secs,
        ))
        .await
    }

    /// Create a scheduled task requested by the in-chat `task` tool.
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
            normalized.timeout_secs,
        ))
        .await?;

        let mut task_id = crate::id::new();
        while self
            .db
            .get_scheduled_task(&task_id)
            .await
            .map_err(|error| prefixed_error("failed to check task id: ", &error))?
            .is_some()
        {
            task_id = crate::id::new();
        }
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
                timeout_secs: normalized.timeout_secs,
            })
            .await
            .map_err(|error| prefixed_error("failed to create scheduled task: ", &error))?;
        // A yolo session stores its tasks approved and enabled.
        let model = self
            .db
            .grant_task_under_yolo(&normalized.session_id, model)
            .await
            .map_err(|error| prefixed_error("failed to approve scheduled task: ", &error))?;
        self.reload_active_tasks()
            .await
            .map_err(|e| e.to_string())?;
        let trigger = model
            .persisted_trigger()
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

    /// Run every enabled task whose webhook token matches `token` and whose
    /// required signature, keyed by a secret from the keychain, verifies.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure or task spawn failure.
    pub async fn dispatch_webhook(
        &self,
        token: &str,
        request: TaskWebhookRequest,
    ) -> Result<WebhookOutcome, crate::db::ChatDbError> {
        let tasks = self.webhook_tasks_for_token(token).await?;
        let names = tasks
            .iter()
            .filter_map(|task| task.webhook_signature_secret.clone())
            .collect::<Vec<_>>();
        let store = self.secrets.clone();
        let secrets = blocking::unblock(move || {
            names
                .into_iter()
                .filter_map(|name| match store.get(&name) {
                    Ok(Some(value)) => Some((name, value)),
                    Ok(None) => None,
                    Err(error) => {
                        tracing::warn!(secret = %name, %error, "failed to read webhook signing secret");
                        None
                    }
                })
                .collect::<BTreeMap<_, _>>()
        })
        .await;
        self.fire_webhook_tasks(tasks, &secrets, request).await
    }

    /// Enabled, approved webhook tasks whose token equals `token`. Every
    /// stored token is compared in constant time with no early exit.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn webhook_tasks_for_token(
        &self,
        token: &str,
    ) -> Result<Vec<scheduled_tasks::Model>, crate::db::ChatDbError> {
        let mut matched = Vec::new();
        for task in self.db.list_webhook_tasks().await? {
            if crate::task_trigger::webhook_tokens_match(&task.trigger_type, token) {
                matched.push(task);
            }
        }
        Ok(matched)
    }

    /// Run the `tasks` whose signature requirement `secrets` satisfies for
    /// `request`; a task whose secret is absent or whose signature is
    /// missing or wrong does not run.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] when a task fails to spawn.
    pub async fn fire_webhook_tasks(
        &self,
        tasks: Vec<scheduled_tasks::Model>,
        secrets: &BTreeMap<String, String>,
        mut request: TaskWebhookRequest,
    ) -> Result<WebhookOutcome, crate::db::ChatDbError> {
        if tasks.is_empty() {
            return Ok(WebhookOutcome::NotFound);
        }
        // The path carries the token; tasks and run records never see it.
        request.path = crate::task_trigger::WEBHOOK_ROUTE_REDACTED.to_string();
        let mut triggered = 0;
        for task in tasks {
            let trigger = stored_task_trigger(&task)?;
            if let Some(signature) = trigger.webhook_signature() {
                let verdict = secrets.get(&signature.secret).map_or_else(
                    || Err(format!("secret {} is not stored", signature.secret)),
                    |secret| {
                        signature
                            .verify(secret, &request)
                            .map_err(|error| error.to_string())
                    },
                );
                if let Err(reason) = verdict {
                    tracing::warn!(task_id = %task.id, %reason, "refused a webhook delivery");
                    continue;
                }
            }
            triggered += 1;
            self.spawn_task(
                task,
                TaskInvocation::Webhook {
                    request: request.clone(),
                },
            )
            .await?;
        }
        Ok(if triggered == 0 {
            WebhookOutcome::SignatureRefused
        } else {
            WebhookOutcome::Triggered(triggered)
        })
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
        let run_id = crate::id::new();
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
        timeout_secs: u32,
    ) -> Result<(), String> {
        let timeout = TaskTimeout::new(timeout_secs)?;
        let profile_info = self
            .db
            .profile_info_from_model(profile.clone())
            .await
            .map_err(|error| invalid_profile_error(profile.id.as_str(), &error))?;
        let script = self
            .resolve_script_source(
                script_path,
                script_inline,
                crate::session::MayHomePaths::new(self.may_dir.clone()).task_validation_dir(),
            )
            .await?;

        match profile_info.environment.kind() {
            ProfileEnvironment::Local => self.validate_local(&profile.id, &script, timeout).await,
            ProfileEnvironment::Container { .. } => {
                Box::pin(self.validate_container(profile, &script, timeout)).await
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
                    let payload = NotificationPayload {
                        notification_id: None,
                        title: task_notification_title(task.name.as_str()),
                        body: TaskOutcome::Completed.notification_body().to_string(),
                        session_id: task.session_id.clone(),
                        profile_id: Some(task.profile_id.clone()),
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
                    let payload = NotificationPayload {
                        notification_id: None,
                        title: task_notification_title(task.name.as_str()),
                        body: TaskOutcome::Failed.notification_body().to_string(),
                        session_id: task.session_id.clone(),
                        profile_id: Some(task.profile_id.clone()),
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
        let placement = self
            .task_file_placement(&profile, profile_info.environment.kind(), &script)
            .await?;
        let materialized = self
            .materialize_task_execution(task, &script, invocation, &placement)
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

    /// The host directory a task's files are written to and the paths its
    /// command uses for them, which depend on where the profile runs it.
    async fn task_file_placement(
        &self,
        profile: &profiles::Model,
        environment: &ProfileEnvironment,
        script: &TaskScript,
    ) -> Result<TaskFilePlacement, String> {
        match environment {
            ProfileEnvironment::Local => {
                prepare_working_dir(&script.host_working_dir).await?;
                Ok(TaskFilePlacement::local(script.host_working_dir.clone()))
            }
            ProfileEnvironment::Container { .. } => self
                .container_task_workspace(profile)
                .await
                .map(TaskFilePlacement::container),
        }
    }

    /// The profile's host-side task workspace, created on demand; its
    /// container mounts it at [`CONTAINER_WORKSPACE`](crate::container::lifecycle::CONTAINER_WORKSPACE).
    async fn container_task_workspace(&self, profile: &profiles::Model) -> Result<PathBuf, String> {
        let workspace_dir = self
            .may_dir
            .join("task-workspaces")
            .join(profile_workspace_key(profile));
        tokio::fs::create_dir_all(&workspace_dir)
            .await
            .map_err(|error| workspace_prepare_error(&workspace_dir, &error))?;
        Ok(workspace_dir)
    }

    async fn materialize_task_execution(
        &self,
        task: &scheduled_tasks::Model,
        script: &TaskScript,
        invocation: TaskInvocation,
        placement: &TaskFilePlacement,
    ) -> Result<MaterializedTaskExecution, String> {
        let execution_id = crate::id::new();
        let trigger_payload = serde_json::to_vec(
            &TaskExecutionEnvelope::new(task, invocation).map_err(|error| {
                prefixed_error("failed to build task trigger payload: ", &error)
            })?,
        )
        .map_err(|error| prefixed_error("failed to encode task trigger payload: ", &error))?;

        let script_file = placement
            .write(
                temp_task_file_name(".may-task-script-", execution_id.as_str(), ".sh"),
                script.content.as_bytes(),
            )
            .await?;
        let trigger_file = match placement
            .write(
                temp_task_file_name(".may-task-trigger-", execution_id.as_str(), ".json"),
                &trigger_payload,
            )
            .await
        {
            Ok(trigger_file) => trigger_file,
            Err(error) => {
                self.cleanup_materialized_execution(vec![script_file.host_path])
                    .await;
                return Err(error);
            }
        };

        Ok(MaterializedTaskExecution {
            command: task_execution_command(&trigger_file.command_path, &script_file.command_path),
            cleanup_paths: vec![script_file.host_path, trigger_file.host_path],
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
        inline_dir: PathBuf,
    ) -> Result<TaskScript, String> {
        match (
            script_inline.map(str::trim).filter(|s| !s.is_empty()),
            script_path.map(str::trim).filter(|s| !s.is_empty()),
        ) {
            (Some(inline), None) => Ok(TaskScript {
                content: inline.to_string(),
                host_working_dir: inline_dir,
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
        self.resolve_script_source(
            task.script_path.as_deref(),
            task.script_inline.as_deref(),
            crate::session::MayHomePaths::new(self.may_dir.clone()).inline_task_dir(&task.id),
        )
        .await
    }

    async fn execute_local(
        &self,
        task: &scheduled_tasks::Model,
        script: &TaskScript,
        command: String,
    ) -> Result<String, String> {
        let timeout = TaskTimeout::from_stored(task.timeout_secs)?;
        let terminal_tool = self
            .local_terminal_tool(
                &task.profile_id,
                NetworkAccessSource::TaskLocal,
                script,
                task.needs_network,
                Some(WakeScope {
                    task,
                    in_container: false,
                }),
            )
            .await?;
        run_terminal_command(
            &terminal_tool,
            local_task_execution_mode(task.needs_network),
            None,
            command,
            timeout,
        )
        .await
    }

    async fn validate_local(
        &self,
        profile_id: &str,
        script: &TaskScript,
        timeout: TaskTimeout,
    ) -> Result<(), String> {
        prepare_working_dir(&script.host_working_dir).await?;
        let terminal_tool = self
            .local_terminal_tool(
                profile_id,
                NetworkAccessSource::TaskValidationLocal,
                script,
                false,
                None,
            )
            .await?;

        run_terminal_command(
            &terminal_tool,
            local_task_execution_mode(false),
            None,
            build_validation_command(script),
            timeout,
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
        wake: Option<WakeScope<'_>>,
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
        let mut commands = ToolRegistryBuilder::new();
        crate::command_policy::register(
            &mut commands,
            crate::command_policy::runs_managed_bash(shell_sessions.availability().container),
        );
        if let Some(scope) = wake {
            commands.configure_tool(crate::task_wake::WakeTool::new(
                self.session_prompts.clone(),
                scope.task.id.clone(),
                scope.task.name.clone(),
                scope.task.session_id.clone(),
                workspace.clone(),
                scope
                    .in_container
                    .then(|| crate::container::lifecycle::workspace_mount(workspace.clone())),
            ));
        }
        let registry = Arc::new(commands.build(workspace.join("outputs")));
        let terminal_tool = match &self.connectivity {
            Some(connectivity) => terminal_tool.with_connectivity(connectivity),
            None => terminal_tool,
        };
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
        wake: Option<WakeScope<'_>>,
    ) -> Result<TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured>, String> {
        let terminal_tool = self
            .initialized_terminal_tool(
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
                wake,
            )
            .await?;
        // Only local commands can open the pool; container tasks run without
        // one until aither brokers tokens into containers.
        match &self.job_server {
            Some(job_server) => {
                let connection = job_server.connect().map_err(|error| {
                    init_terminal_tool_error("failed to connect to the jobserver: ", &error)
                })?;
                terminal_tool
                    .with_jobserver_pool(connection.pool, connection.fifo_dir)
                    .map_err(|error| {
                        init_terminal_tool_error("failed to share the jobserver: ", &error)
                    })
            }
            None => Ok(terminal_tool),
        }
    }

    /// Starts the profile's container and builds a terminal that executes in
    /// it, storing outputs in the host-side task workspace mounted at
    /// [`CONTAINER_WORKSPACE`](crate::container::lifecycle::CONTAINER_WORKSPACE). Returns the container manager and container ID so callers
    /// can record activity.
    async fn container_terminal_tool(
        &self,
        profile: &profiles::Model,
        source: NetworkAccessSource,
        task: Option<&scheduled_tasks::Model>,
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

        let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences);
        let image = crate::container::image::MAY_WORKSPACE_IMAGE;

        crate::container::image::ensure_image(container_manager.runtime(), image)
            .await
            .map_err(|error| prefixed_error("container image pull failed: ", &error))?;

        let container_name = container_profile_name(profile);
        let workspace_dir = self.container_task_workspace(profile).await?;

        let mut launch_spec =
            ContainerLaunchSpec::new(container_name, image.to_string(), workspace_dir.clone());
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
        )
        .with_workspace_mount(crate::container::lifecycle::workspace_mount(&workspace_dir));
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
                task.map(|task| WakeScope {
                    task,
                    in_container: true,
                }),
            )
            .await?;
        Ok((terminal_tool, container_manager, container_id))
    }

    async fn execute_container(
        &self,
        task: &scheduled_tasks::Model,
        profile: &profiles::Model,
        command: String,
    ) -> Result<String, String> {
        let timeout = TaskTimeout::from_stored(task.timeout_secs)?;
        let (terminal_tool, container_manager, container_id) = Box::pin(
            self.container_terminal_tool(profile, NetworkAccessSource::TaskContainer, Some(task)),
        )
        .await?;
        let result = run_terminal_command(
            &terminal_tool,
            TerminalExecutionMode::Default,
            None,
            command,
            timeout,
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

    /// Writes the script into the task workspace and syntax-checks it at its
    /// in-container path, so validation fails exactly where a run would.
    async fn validate_container(
        &self,
        profile: &profiles::Model,
        script: &TaskScript,
        timeout: TaskTimeout,
    ) -> Result<(), String> {
        let placement = TaskFilePlacement::container(self.container_task_workspace(profile).await?);
        let script_file = placement
            .write(
                temp_task_file_name(".may-task-validate-", crate::id::new().as_str(), ".sh"),
                script.content.as_bytes(),
            )
            .await?;
        let result = Box::pin(self.run_container_validation(
            profile,
            task_file_validation_command(&script_file.command_path),
            timeout,
        ))
        .await;
        self.cleanup_materialized_execution(vec![script_file.host_path])
            .await;
        result
    }

    async fn run_container_validation(
        &self,
        profile: &profiles::Model,
        command: String,
        timeout: TaskTimeout,
    ) -> Result<(), String> {
        let (terminal_tool, container_manager, container_id) =
            Box::pin(self.container_terminal_tool(
                profile,
                NetworkAccessSource::TaskValidationContainer,
                None,
            ))
            .await?;
        let result = run_terminal_command(
            &terminal_tool,
            TerminalExecutionMode::Default,
            None,
            command,
            timeout,
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

/// Maximum runtime of one scheduled run, validated against the largest
/// timeout a background terminal job accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskTimeout(std::time::Duration);

impl TaskTimeout {
    fn new(secs: u32) -> Result<Self, String> {
        let cap = aither_sandbox::DEFAULT_TIMEOUT_CAP;
        let timeout = std::time::Duration::from_secs(u64::from(secs));
        if secs == 0 || timeout > cap {
            return Err(format!(
                "task timeout_secs must be between 1 and {} seconds, got {secs}",
                cap.as_secs()
            ));
        }
        Ok(Self(timeout))
    }

    fn from_stored(secs: i64) -> Result<Self, String> {
        let secs = u32::try_from(secs)
            .map_err(|_| format!("stored task timeout_secs is out of range: {secs}"))?;
        Self::new(secs)
    }

    const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

/// Runs `script` as a detached terminal job bounded by `timeout` and waits
/// for its final result; the foreground window does not apply to it.
async fn run_terminal_command(
    terminal_tool: &TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured>,
    mode: TerminalExecutionMode,
    ssh_server_id: Option<String>,
    script: String,
    timeout: TaskTimeout,
) -> Result<String, String> {
    let started = terminal_tool
        .run_background(TerminalArgs {
            description: "run the scheduled task script".to_string(),
            script,
            language: aither_sandbox::ScriptLanguage::Shell,
            mode,
            ssh_server_id,
            expect: OutputFormat::Text,
            resolution: aither_sandbox::MediaResolution::Auto,
            max_lines: 800,
            raw: false,
            timeout: timeout.as_secs(),
            background: true,
        })
        .await
        .map_err(|error| prefixed_error("task terminal execution failed: ", &error))?;
    if started.is_error() {
        return Err(started
            .error_message()
            .unwrap_or("task terminal execution failed")
            .to_string());
    }
    let CommandOutcome::Running { task_id, .. } = started
        .command()
        .ok_or_else(|| "background terminal tool did not return a command result".to_string())?
        .outcome
        .clone()
    else {
        return Err("background terminal job started without a task id".to_string());
    };

    let receiver = terminal_tool.background_receiver();
    let completed = loop {
        let Some(completed) = receiver.recv().await else {
            return Err("background terminal channel closed before the task finished".to_string());
        };
        if completed.task_id == task_id {
            break completed;
        }
    };

    let killed = terminal_tool
        .job_registry()
        .get_by_task_id(&task_id)
        .await
        .is_some_and(|job| matches!(job.status, JobStatus::Killed { .. }));
    if killed {
        return Err(format!(
            "task timed out after {} seconds and was killed",
            timeout.as_secs()
        ));
    }
    let result = completed
        .result
        .map_err(|error| prefixed_error("task terminal execution failed: ", &error))?;

    parse_terminal_result(&result)
}

fn parse_terminal_result(result: &CompletedCommand) -> Result<String, String> {
    let output = combine_output(
        &crate::command_output::stream_text(&result.stdout),
        result
            .stderr
            .as_ref()
            .map(crate::command_output::stream_text),
    );
    if result.exit_code != 0 {
        return Err(task_script_failure(result.exit_code, output.as_str()));
    }

    Ok(output)
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

    use super::{
        SchedulerPermissionHandler, TaskFilePlacement, TaskRuntime, TaskScript, TaskTimeout,
        local_task_execution_mode, run_terminal_command, task_execution_command,
        task_file_validation_command,
    };
    use aither_sandbox::{Configured, TerminalExecutionMode, TerminalTool};
    use executor_core::DefaultExecutor;
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
        let local_profile_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");

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
                profile_id: local_profile_id.as_str(),
                session_id,
                name: task_id,
                trigger: &trigger,
                cron_expression: None,
                run_at: None,
                script_path: None,
                script_inline: Some("echo hi"),
                needs_network: false,
                timeout_secs: 60,
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
            may_dir.clone(),
            Arc::new(crate::tools::fixture_distribution(
                &may_dir.join("tool-fixtures"),
            )),
        )
        .await
        .expect("task runtime should load");

        let triggered = runtime
            .dispatch_event(
                "chat.turn.completed",
                Some(local_profile_id.as_str()),
                Some("session-a"),
            )
            .await
            .expect("event dispatch should succeed");

        assert_eq!(triggered, 1);
    }

    fn webhook_request(
        signature: Option<String>,
        body: &[u8],
    ) -> crate::task_trigger::TaskWebhookRequest {
        use base64::Engine as _;
        let mut headers = std::collections::BTreeMap::new();
        if let Some(signature) = signature {
            headers.insert("x-hub-signature-256".to_string(), vec![signature]);
        }
        crate::task_trigger::TaskWebhookRequest {
            method: "POST".to_string(),
            path: "/api/tasks/webhook/secret-token".to_string(),
            query: None,
            headers,
            body_base64: base64::engine::general_purpose::STANDARD.encode(body),
            body_text: None,
            body_json: None,
            remote_addr: None,
            body_size_bytes: body.len(),
        }
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("key");
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[tokio::test]
    async fn webhook_delivery_needs_the_token_and_any_required_signature() {
        use super::WebhookOutcome;
        use crate::task_trigger::WebhookSignature;

        let dir = tempdir().expect("temp dir should be created");
        let may_dir = dir.path().join(".may");
        std::fs::create_dir_all(&may_dir).expect("may dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize");
        let profile_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");

        let plain_token = crate::task_trigger::new_webhook_token();
        let signed_token = crate::task_trigger::new_webhook_token();
        for (id, token, signature) in [
            ("plain", plain_token.clone(), None),
            (
                "signed",
                signed_token.clone(),
                Some(WebhookSignature {
                    secret: "HOOK_SECRET".to_string(),
                    header: "x-hub-signature-256".to_string(),
                }),
            ),
        ] {
            let trigger = PersistedTaskTrigger::Webhook { token, signature };
            db.create_scheduled_task(CreateScheduledTaskParams {
                id,
                profile_id: profile_id.as_str(),
                session_id: None,
                name: id,
                trigger: &trigger,
                cron_expression: None,
                run_at: None,
                script_path: None,
                script_inline: Some("echo hi"),
                needs_network: false,
                timeout_secs: 60,
            })
            .await
            .expect("task should persist");
            db.approve_scheduled_task(id)
                .await
                .expect("task should be approved");
        }
        let runtime = TaskRuntime::load(
            db,
            may_dir.clone(),
            Arc::new(crate::tools::fixture_distribution(
                &may_dir.join("tool-fixtures"),
            )),
        )
        .await
        .expect("task runtime should load");
        let secrets =
            std::collections::BTreeMap::from([("HOOK_SECRET".to_string(), "s3cret".to_string())]);
        let body = br#"{"event":"push"}"#;
        let deliver = |token: String, signature: Option<String>| {
            let runtime = runtime.clone();
            let secrets = secrets.clone();
            async move {
                let tasks = runtime
                    .webhook_tasks_for_token(&token)
                    .await
                    .expect("lookup should succeed");
                runtime
                    .fire_webhook_tasks(tasks, &secrets, webhook_request(signature, body))
                    .await
                    .expect("dispatch should succeed")
            }
        };

        // A wrong token, including the old petname shape, runs nothing.
        assert_eq!(
            deliver(crate::task_trigger::new_webhook_token(), None).await,
            WebhookOutcome::NotFound
        );
        assert_eq!(
            deliver("quiet-brave-mint-otter".to_string(), None).await,
            WebhookOutcome::NotFound
        );
        // An unsigned task needs only its token.
        assert_eq!(
            deliver(plain_token, None).await,
            WebhookOutcome::Triggered(1)
        );
        // A signed task refuses a missing or wrong signature before running.
        assert_eq!(
            deliver(signed_token.clone(), None).await,
            WebhookOutcome::SignatureRefused
        );
        assert_eq!(
            deliver(signed_token.clone(), Some(sign("wrong", body))).await,
            WebhookOutcome::SignatureRefused
        );
        // A correctly signed delivery runs it.
        assert_eq!(
            deliver(signed_token.clone(), Some(sign("s3cret", body))).await,
            WebhookOutcome::Triggered(1)
        );
        // A signed task whose secret is not stored refuses everything.
        let tasks = runtime
            .webhook_tasks_for_token(&signed_token)
            .await
            .expect("lookup should succeed");
        assert_eq!(
            runtime
                .fire_webhook_tasks(
                    tasks,
                    &std::collections::BTreeMap::new(),
                    webhook_request(Some(sign("s3cret", body)), body)
                )
                .await
                .expect("dispatch should succeed"),
            WebhookOutcome::SignatureRefused
        );
    }

    #[tokio::test]
    async fn opening_the_database_replaces_weak_webhook_tokens() {
        let dir = tempdir().expect("temp dir should be created");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("db should initialize");
        let profile_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        let trigger = PersistedTaskTrigger::Webhook {
            token: "quiet-brave-mint-otter".to_string(),
            signature: None,
        };
        db.create_scheduled_task(CreateScheduledTaskParams {
            id: "old",
            profile_id: profile_id.as_str(),
            session_id: None,
            name: "old",
            trigger: &trigger,
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi"),
            needs_network: false,
            timeout_secs: 60,
        })
        .await
        .expect("task should persist");
        drop(db);

        let db = ChatDb::open(&path).await.expect("db should reopen");
        let task = db
            .get_scheduled_task("old")
            .await
            .expect("task should load")
            .expect("task should exist");
        assert!(crate::task_trigger::is_strong_webhook_token(
            &task.trigger_type
        ));
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
                None,
            )
            .await
            .expect_err("a distribution without native tools must fail before any exec");
        assert!(
            error.starts_with("failed to prepare baseline tools: "),
            "unexpected error: {error}"
        );
    }

    /// Builds a real local terminal over a temp workspace, prepared the same
    /// way scheduled runs prepare theirs.
    async fn scheduler_test_terminal(
        dir: &std::path::Path,
    ) -> TerminalTool<SchedulerPermissionHandler, DefaultExecutor, Configured> {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let may_dir = dir.join(".may");
        std::fs::create_dir_all(&may_dir).expect("may dir should be created");
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace should be created");
        let db = ChatDb::open(&dir.join("history.db"))
            .await
            .expect("db should initialize");
        let runtime = TaskRuntime::load(
            db,
            may_dir.clone(),
            Arc::new(crate::tools::fixture_distribution(
                &may_dir.join("tool-fixtures"),
            )),
        )
        .await
        .expect("task runtime should load");
        runtime
            .local_terminal_tool(
                "profile",
                NetworkAccessSource::TaskLocal,
                &TaskScript {
                    content: String::new(),
                    host_working_dir: workspace,
                },
                false,
                None,
            )
            .await
            .expect("terminal should initialize")
    }

    #[tokio::test]
    async fn task_runs_past_the_foreground_wait() {
        let dir = tempdir().expect("temp dir should be created");
        let terminal = scheduler_test_terminal(dir.path()).await;
        let runtime_secs = u32::try_from(aither_sandbox::DEFAULT_FOREGROUND_WAIT.as_secs())
            .expect("the foreground wait fits in u32 seconds")
            + 2;

        let output = run_terminal_command(
            &terminal,
            TerminalExecutionMode::Sandboxed,
            None,
            format!("sleep {runtime_secs}; echo finished"),
            TaskTimeout::new(runtime_secs + 60).expect("timeout is within the cap"),
        )
        .await
        .expect("a task longer than the foreground wait should complete");

        assert!(output.contains("finished"), "unexpected output: {output}");
    }

    #[tokio::test]
    async fn task_past_its_own_timeout_fails_with_timeout_error() {
        let dir = tempdir().expect("temp dir should be created");
        let terminal = scheduler_test_terminal(dir.path()).await;

        let error = run_terminal_command(
            &terminal,
            TerminalExecutionMode::Sandboxed,
            None,
            "sleep 60; echo finished".to_string(),
            TaskTimeout::new(2).expect("timeout is within the cap"),
        )
        .await
        .expect_err("a task past its timeout must fail");

        assert_eq!(error, "task timed out after 2 seconds and was killed");
    }

    #[test]
    fn host_placement_commands_use_host_paths() {
        let placement = TaskFilePlacement::local(std::path::PathBuf::from("/host/scripts"));
        let script = placement.command_path(".may-task-script-1.sh");
        let trigger = placement.command_path(".may-task-trigger-1.json");
        assert_eq!(script, "/host/scripts/.may-task-script-1.sh");
        assert_eq!(
            task_execution_command(&trigger, &script),
            "export MAY_TASK_TRIGGER_FILE=/host/scripts/.may-task-trigger-1.json\nexec bash /host/scripts/.may-task-script-1.sh"
        );
    }

    #[test]
    fn container_placement_commands_use_the_workspace_mount() {
        let placement = TaskFilePlacement::container(std::path::PathBuf::from(
            "/host/may/task-workspaces/work",
        ));
        let script = placement.command_path(".may-task-script-1.sh");
        let trigger = placement.command_path(".may-task-trigger-1.json");
        assert_eq!(
            script,
            crate::container::lifecycle::container_workspace_path(".may-task-script-1.sh")
        );
        let command = task_execution_command(&trigger, &script);
        assert!(
            command.contains("MAY_TASK_TRIGGER_FILE=/workspace/.may-task-trigger-1.json")
                && command.contains("exec bash /workspace/.may-task-script-1.sh"),
            "unexpected command: {command}"
        );
        assert!(
            !command.contains("/host/may"),
            "a container command must not mention host paths: {command}"
        );
        assert_eq!(
            task_file_validation_command(&script),
            "exec bash -n /workspace/.may-task-script-1.sh"
        );
    }

    #[tokio::test]
    async fn placement_writes_to_the_host_dir_and_reports_the_command_path() {
        let dir = tempdir().expect("temp dir should be created");
        let placement = TaskFilePlacement::container(dir.path().to_path_buf());

        let file = placement
            .write("task.sh".to_string(), b"echo ok")
            .await
            .expect("file should be written");

        assert_eq!(file.host_path, dir.path().join("task.sh"));
        assert_eq!(file.command_path, "/workspace/task.sh");
        assert_eq!(
            std::fs::read(&file.host_path).expect("host file should exist"),
            b"echo ok"
        );
    }

    #[test]
    fn task_timeout_is_bounded_by_the_background_cap() {
        assert!(TaskTimeout::new(0).is_err());
        assert!(TaskTimeout::new(1).is_ok());
        let cap = u32::try_from(aither_sandbox::DEFAULT_TIMEOUT_CAP.as_secs())
            .expect("the background cap fits in u32 seconds");
        assert!(TaskTimeout::new(cap).is_ok());
        assert!(TaskTimeout::new(cap + 1).is_err());
        assert!(TaskTimeout::from_stored(-1).is_err());
    }

    /// A daemon's `history.db` with a task runtime whose runs can `wake`.
    struct WakeHost {
        dir: tempfile::TempDir,
        runtime: TaskRuntime,
        runtime_service: Arc<crate::session::SessionRuntimeService>,
        wakes: async_channel::Receiver<String>,
        profile_id: String,
    }

    impl WakeHost {
        async fn new() -> Self {
            let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
            let dir = tempdir().expect("temp dir should be created");
            let may_dir = dir.path().join(".may");
            std::fs::create_dir_all(&may_dir).expect("may dir should be created");
            let db = ChatDb::open(&dir.path().join("history.db"))
                .await
                .expect("db should initialize");
            let profile_id = db
                .ensure_default_profiles()
                .await
                .expect("default profiles should exist");
            let runtime_service = Arc::new(crate::session::SessionRuntimeService::new(
                db.clone(),
                crate::ui_event_bus::UiEventBus::new(),
            ));
            let (wake_tx, wakes) = async_channel::unbounded();
            let runtime = TaskRuntime::load(
                db,
                may_dir.clone(),
                Arc::new(crate::tools::fixture_distribution(
                    &may_dir.join("tool-fixtures"),
                )),
            )
            .await
            .expect("task runtime should load")
            .with_session_prompts(crate::task_wake::SessionPrompts::new(
                runtime_service.clone(),
                wake_tx,
            ));
            Self {
                dir,
                runtime,
                runtime_service,
                wakes,
                profile_id,
            }
        }

        async fn task(
            &self,
            session_id: Option<&str>,
            trigger: &PersistedTaskTrigger,
            script: &str,
        ) -> crate::db::entity::scheduled_tasks::Model {
            // The script lives beside, not inside, the managed tool directory.
            let script_dir = self.dir.path().join("scripts");
            std::fs::create_dir_all(&script_dir).expect("script dir");
            let script_path = script_dir.join("task.sh");
            std::fs::write(&script_path, script).expect("script file");
            let db = self.runtime_service.db();
            if let Some(session_id) = session_id {
                db.create_session(session_id, Some("main"), None)
                    .await
                    .expect("session is created");
            }
            db.create_scheduled_task(CreateScheduledTaskParams {
                id: "task-1",
                profile_id: self.profile_id.as_str(),
                session_id,
                name: "Nightly check",
                trigger,
                cron_expression: None,
                run_at: None,
                script_path: Some(script_path.to_str().expect("utf-8 path")),
                script_inline: None,
                needs_network: false,
                timeout_secs: 60,
            })
            .await
            .expect("task should persist");
            db.get_scheduled_task("task-1")
                .await
                .expect("task reads")
                .expect("task exists")
        }

        /// The one prompt the task stored for `session_id`, claimed as the
        /// session's next run would.
        async fn stored_prompt(&self, session_id: &str) -> crate::session::PendingUserMessage {
            self.runtime_service
                .db()
                .claim_next_pending_message(
                    &crate::db::RunFence {
                        session_id: session_id.to_string(),
                        run_id: "run".to_string(),
                    },
                    session_id,
                )
                .await
                .expect("queue read")
                .expect("the prompt is stored")
        }
    }

    /// An inline script runs in a directory of the task's own, apart from the
    /// managed tools and the rest of the may home.
    #[tokio::test]
    async fn an_inline_local_task_runs_in_its_own_directory() {
        let host = WakeHost::new().await;
        let db = host.runtime_service.db();
        db.create_scheduled_task(CreateScheduledTaskParams {
            id: "inline-1",
            profile_id: host.profile_id.as_str(),
            session_id: None,
            name: "Inline",
            trigger: &TIME_ONCE,
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi; pwd -P"),
            needs_network: false,
            timeout_secs: 60,
        })
        .await
        .expect("task should persist");
        let task = db
            .get_scheduled_task("inline-1")
            .await
            .expect("task reads")
            .expect("task exists");

        let output = Box::pin(host.runtime.execute_task_inner(
            &task,
            crate::task_trigger::TaskInvocation::Time {
                trigger_type: crate::task_trigger::TimeTriggerType::Once,
            },
        ))
        .await
        .expect("the inline task run succeeds");

        let may_dir = host.dir.path().join(".may");
        let task_dir = crate::session::MayHomePaths::new(may_dir.clone())
            .inline_task_dir("inline-1")
            .canonicalize()
            .expect("the task directory was created");
        assert!(output.contains("hi"), "the output: {output}");
        assert!(
            output.contains(task_dir.to_str().expect("utf-8 path")),
            "the working directory is {task_dir:?}: {output}"
        );
        assert!(!task_dir.starts_with(may_dir.join("tools")));
        assert!(!may_dir.join("tools").starts_with(&task_dir));
    }

    /// Deleting a task removes its working directory, and deleting one that
    /// never ran is fine.
    #[tokio::test]
    async fn deleting_a_task_removes_its_inline_directory() {
        let host = WakeHost::new().await;
        let db = host.runtime_service.db();
        db.create_scheduled_task(CreateScheduledTaskParams {
            id: "inline-2",
            profile_id: host.profile_id.as_str(),
            session_id: None,
            name: "Inline",
            trigger: &TIME_ONCE,
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi"),
            needs_network: false,
            timeout_secs: 60,
        })
        .await
        .expect("task should persist");
        let task = db
            .get_scheduled_task("inline-2")
            .await
            .expect("task reads")
            .expect("task exists");
        Box::pin(host.runtime.execute_task_inner(
            &task,
            crate::task_trigger::TaskInvocation::Time {
                trigger_type: crate::task_trigger::TimeTriggerType::Once,
            },
        ))
        .await
        .expect("the inline task run succeeds");
        let dir = crate::session::MayHomePaths::new(host.dir.path().join(".may"))
            .inline_task_dir("inline-2");
        assert!(dir.is_dir(), "the run created {dir:?}");

        host.runtime.delete_task("inline-2").await.expect("delete");

        assert!(!dir.exists(), "{dir:?} is removed with the task");
        assert!(
            db.get_scheduled_task("inline-2")
                .await
                .expect("read")
                .is_none()
        );
        host.runtime
            .delete_task("inline-2")
            .await
            .expect("an absent directory is fine");
    }

    const TIME_ONCE: PersistedTaskTrigger = PersistedTaskTrigger::Time {
        trigger_type: crate::task_trigger::TimeTriggerType::Once,
    };

    /// A time-triggered task whose script runs `wake` stores a prompt in its
    /// idle session's queue, attributed to the task, and wakes the session.
    #[tokio::test]
    async fn a_time_task_wakes_its_idle_session_with_a_prompt() {
        let host = WakeHost::new().await;
        let task = host
            .task(
                Some("main"),
                &TIME_ONCE,
                "echo 'check the backups' > \"$MAY_TASK_TRIGGER_FILE.prompt\"\nwake \"$MAY_TASK_TRIGGER_FILE.prompt\"",
            )
            .await;

        Box::pin(host.runtime.execute_task_inner(
            &task,
            crate::task_trigger::TaskInvocation::Time {
                trigger_type: crate::task_trigger::TimeTriggerType::Once,
            },
        ))
        .await
        .expect("the task run succeeds");

        assert_eq!(host.wakes.recv().await.expect("a wake"), "main");
        let prompt = host.stored_prompt("main").await;
        assert!(prompt.content.is_empty(), "it is not the user's text");
        let xml = prompt.hidden_context_xml.expect("task delivery");
        assert!(xml.contains("task-1"), "the task id is named: {xml}");
        assert!(xml.contains("Nightly check"), "the task is named: {xml}");
        assert!(xml.contains("check the backups"), "the prompt: {xml}");
    }

    /// A busy session keeps the prompt queued for after its current turn
    /// rather than losing it or replacing a steering message.
    #[tokio::test]
    async fn a_busy_session_receives_the_prompt_after_its_turn() {
        let host = WakeHost::new().await;
        let task = host
            .task(
                Some("main"),
                &TIME_ONCE,
                "echo 'later' > \"$MAY_TASK_TRIGGER_FILE.prompt\"\nwake \"$MAY_TASK_TRIGGER_FILE.prompt\"",
            )
            .await;
        host.runtime_service
            .set_state("main", crate::session::SessionRuntimeState::Running)
            .await
            .expect("state persists");

        Box::pin(host.runtime.execute_task_inner(
            &task,
            crate::task_trigger::TaskInvocation::Time {
                trigger_type: crate::task_trigger::TimeTriggerType::Once,
            },
        ))
        .await
        .expect("the task run succeeds");

        let prompt = host.stored_prompt("main").await;
        assert_eq!(
            prompt.delivery_mode,
            crate::session::PendingUserMessageMode::Queue
        );
        assert!(
            prompt
                .hidden_context_xml
                .expect("delivery")
                .contains("later")
        );
    }

    /// A webhook task passes its trigger payload into the prompt.
    #[tokio::test]
    async fn a_webhook_task_passes_its_payload_into_the_prompt() {
        let host = WakeHost::new().await;
        let task = host
            .task(
                Some("main"),
                &PersistedTaskTrigger::Webhook {
                    token: "tok".to_string(),
                    signature: None,
                },
                "wake \"$MAY_TASK_TRIGGER_FILE\"",
            )
            .await;

        Box::pin(host.runtime.execute_task_inner(
            &task,
            crate::task_trigger::TaskInvocation::Webhook {
                request: crate::task_trigger::TaskWebhookRequest {
                    method: "POST".to_string(),
                    path: "/hook".to_string(),
                    query: None,
                    headers: std::collections::BTreeMap::new(),
                    body_base64: String::new(),
                    body_text: Some("deploy finished".to_string()),
                    body_json: None,
                    remote_addr: None,
                    body_size_bytes: 15,
                },
            },
        ))
        .await
        .expect("the task run succeeds");

        let xml = host
            .stored_prompt("main")
            .await
            .hidden_context_xml
            .expect("delivery");
        assert!(xml.contains("deploy finished"), "the payload: {xml}");
    }

    /// A task no session created has nobody to prompt: the run fails with a
    /// clear error and nothing is stored.
    #[tokio::test]
    async fn a_task_without_a_session_refuses_wake() {
        let host = WakeHost::new().await;
        let task = host
            .task(
                None,
                &TIME_ONCE,
                "echo hi > \"$MAY_TASK_TRIGGER_FILE.p\"\nwake \"$MAY_TASK_TRIGGER_FILE.p\"",
            )
            .await;

        let error = Box::pin(host.runtime.execute_task_inner(
            &task,
            crate::task_trigger::TaskInvocation::Time {
                trigger_type: crate::task_trigger::TimeTriggerType::Once,
            },
        ))
        .await
        .expect_err("wake must fail without a creating session");

        assert!(
            error.contains("was not created by a session"),
            "unexpected error: {error}"
        );
        assert!(host.wakes.is_empty());
    }
}
