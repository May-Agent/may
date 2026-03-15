//! Scheduled task execution runtime.
//!
//! Background tokio task that ticks every 30 seconds, checks for due tasks
//! (cron or one-time), and executes them through profile-specific runtimes.

use std::borrow::Cow;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use aither_core::llm::Tool;
use aither_sandbox::{
    BashArgs, BashExecutionMode, BashMode, BashResult, BashTool, BollardContainerExec, Configured,
    ContainerExecHandle, ContainerLaunchSpec, ContainerShellRuntime, OutputFormat,
    PermissionHandler, ShellRuntimeAvailability, ShellSessionRegistry, SshServer,
    ToolRegistryBuilder, permission::PermissionError,
};
use arc_swap::ArcSwap;
use askama::Template;
use base64::Engine;
use executor_core::DefaultExecutor;
use sea_orm::DbErr;
use serde::Serialize;
use shell_escape::escape as shell_escape;

use crate::agent::task_tool::{TaskCreateRequest, TaskCreateResult, TaskRuntimeKind};
use crate::config::SshServerConfig;
use crate::container::lifecycle::ContainerManager;
use crate::db::ChatDb;
use crate::db::NetworkAccessLogParams;
use crate::db::entity::{profiles, scheduled_tasks};
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyConfig};
use crate::notification::NotificationPayload;
use crate::notification_service::NotificationService;
use crate::profile::{ProfileEnvironment, ProfileInfo};
use crate::task_trigger::{
    TRIGGER_KIND_EVENT, TRIGGER_KIND_TIME, TRIGGER_KIND_WEBHOOK, TRIGGER_TYPE_CRON,
    TRIGGER_TYPE_ONCE, TaskInvocation, TaskWebhookRequest, normalize_event_name, webhook_path,
};

/// Shared task execution facility used by the scheduler, REST API, and agent tools.
#[derive(Clone)]
pub struct TaskRuntime {
    db: ChatDb,
    notifier: Option<Arc<NotificationService>>,
    may_dir: PathBuf,
    container_manager: Option<Arc<ContainerManager>>,
    ssh_servers: Arc<ArcSwap<Vec<SshServerConfig>>>,
    network_policy: Arc<ArcSwap<Option<CompiledNetworkPolicy>>>,
    active_tasks: Arc<ArcSwap<TaskCatalog>>,
}

/// Time-based wrapper that periodically asks [`TaskRuntime`] to run due tasks.
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
    trigger_kind: &'a str,
    script_file: &'a str,
}

#[derive(Debug, Default)]
struct TaskCatalog {
    tasks: Vec<scheduled_tasks::Model>,
}

impl TaskCatalog {
    fn from_tasks(tasks: Vec<scheduled_tasks::Model>) -> Self {
        Self { tasks }
    }
}

fn join_text(parts: &[&str]) -> String {
    let capacity = parts.iter().map(|part| part.len()).sum();
    let mut output = String::with_capacity(capacity);
    for part in parts {
        output.push_str(part);
    }
    output
}

fn prefixed_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&[prefix, error_text.as_str()])
}

fn profile_load_error(profile_id: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&[
        "failed to load profile ",
        profile_id,
        ": ",
        error_text.as_str(),
    ])
}

fn profile_not_found_error(profile_id: &str) -> String {
    join_text(&["profile ", profile_id, " not found"])
}

fn invalid_profile_error(profile_id: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&["invalid profile ", profile_id, ": ", error_text.as_str()])
}

fn session_load_error(session_id: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&[
        "failed to load session ",
        session_id,
        ": ",
        error_text.as_str(),
    ])
}

fn session_not_found_error(session_id: &str) -> String {
    join_text(&["session not found: ", session_id])
}

fn task_notification_title(task_name: &str) -> String {
    join_text(&["Task: ", task_name])
}

fn task_notification_callback_message(task_name: &str, status: &str) -> String {
    join_text(&[
        "Continue from scheduled task \"",
        task_name,
        "\" (",
        status,
        "). Review the hidden task result and choose the next action.",
    ])
}

const fn task_runtime_kind_name(kind: TaskRuntimeKind) -> &'static str {
    match kind {
        TaskRuntimeKind::Local => "local",
        TaskRuntimeKind::Container => "container",
        TaskRuntimeKind::Remote => "remote",
    }
}

fn task_script_failure(exit_code: i32, output: &str) -> String {
    let exit_code_text = exit_code.to_string();
    join_text(&[
        "task script failed with exit code ",
        exit_code_text.as_str(),
        "\n",
        output,
    ])
}

fn temp_task_file_name(prefix: &str, execution_id: &str, suffix: &str) -> String {
    join_text(&[prefix, execution_id, suffix])
}

fn path_error_text(prefix: &str, path: &std::path::Path, error: &impl std::fmt::Display) -> String {
    let path_text = path.display().to_string();
    let error_text = error.to_string();
    join_text(&[prefix, path_text.as_str(), ": ", error_text.as_str()])
}

fn profile_workspace_key(profile: &profiles::Model) -> &str {
    profile.slug.as_deref().unwrap_or(&profile.id)
}

fn container_profile_name(profile: &profiles::Model) -> String {
    join_text(&["may-", profile_workspace_key(profile)])
}

fn task_notification_callback_context(
    task: &scheduled_tasks::Model,
    status: &'static str,
    summary: &str,
) -> Option<crate::XmlFragment> {
    task.session_id.as_ref()?;
    let xml = crate::xml_fragment::serialize_tagged_xml(
        "system_alert",
        &TaskNotificationCallbackXml {
            alert_type: "task_callback",
            task_id: task.id.as_str(),
            task_name: task.name.as_str(),
            profile_id: task.profile_id.as_str(),
            trigger_kind: task.trigger_kind.as_str(),
            trigger_type: task.trigger_type.as_str(),
            status,
            summary,
        },
    )
    .ok()?;
    crate::XmlFragment::parse(xml).ok()
}

fn task_notification_callback_payload(
    task: &scheduled_tasks::Model,
    status: &'static str,
    summary: &str,
) -> (Option<crate::XmlFragment>, Option<String>) {
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

fn task_db_error(message: String) -> crate::db::ChatDbError {
    crate::db::ChatDbError::Db(DbErr::Custom(message))
}

fn task_field_error(task_id: &str, prefix: &str, field: &str) -> crate::db::ChatDbError {
    task_db_error(join_text(&[prefix, task_id, " is missing ", field]))
}

fn task_value_error(
    task_id: &str,
    prefix: &str,
    error: &impl std::fmt::Display,
) -> crate::db::ChatDbError {
    let error_text = error.to_string();
    task_db_error(join_text(&[prefix, task_id, ": ", error_text.as_str()]))
}

fn task_kind_error(kind: &str, task_id: &str) -> crate::db::ChatDbError {
    task_db_error(join_text(&[
        "unsupported trigger_kind '",
        kind,
        "' for task ",
        task_id,
    ]))
}

fn task_trigger_type_error(trigger_type: &str, task_id: &str) -> crate::db::ChatDbError {
    task_db_error(join_text(&[
        "unsupported time trigger_type '",
        trigger_type,
        "' for task ",
        task_id,
    ]))
}

fn workspace_prepare_error(path: &std::path::Path, error: &impl std::fmt::Display) -> String {
    path_error_text("failed to prepare workspace ", path, error)
}

fn init_bash_tool_error(prefix: &str, error: &impl std::fmt::Display) -> String {
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
    trigger_kind: String,
    trigger_type: String,
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
    fn new(task: &scheduled_tasks::Model, invocation: TaskInvocation) -> Self {
        Self {
            task: TaskExecutionTask {
                id: task.id.clone(),
                name: task.name.clone(),
                profile_id: task.profile_id.clone(),
                session_id: task.session_id.clone(),
                trigger_kind: task.trigger_kind.clone(),
                trigger_type: task.trigger_type.clone(),
            },
            invocation,
        }
    }
}

#[derive(Debug, Clone)]
struct SchedulerPermissionHandler {
    db: ChatDb,
    profile_id: String,
    source: String,
    allow_network: bool,
    network_policy: Option<CompiledNetworkPolicy>,
}

impl PermissionHandler for SchedulerPermissionHandler {
    async fn check(&self, _mode: BashMode, _script: &str) -> Result<bool, PermissionError> {
        Ok(true)
    }

    async fn check_domain(&self, domain: &str, port: u16) -> bool {
        let allowed = self.allow_network
            && self
                .network_policy
                .as_ref()
                .is_none_or(|policy| policy.allows_domain(domain));
        let reason = if !self.allow_network {
            Some("needs_network_disabled")
        } else if let Some(policy) = &self.network_policy {
            if !policy.allows_domain(domain) {
                Some(policy.mode().as_str())
            } else {
                None
            }
        } else {
            None
        };
        let _ = self
            .db
            .append_network_access_log(NetworkAccessLogParams {
                session_id: None,
                profile_id: Some(self.profile_id.as_str()),
                source: self.source.as_str(),
                mode: "network",
                domain,
                port,
                allowed,
                reason,
            })
            .await;
        allowed
    }
}

impl TaskRuntime {
    pub async fn load(db: ChatDb, may_dir: PathBuf) -> Result<Self, crate::db::ChatDbError> {
        let runtime = Self {
            db,
            notifier: None,
            may_dir,
            container_manager: None,
            ssh_servers: Arc::new(ArcSwap::from_pointee(Vec::new())),
            network_policy: Arc::new(ArcSwap::from_pointee(None)),
            active_tasks: Arc::new(ArcSwap::from_pointee(TaskCatalog::default())),
        };
        runtime.reload_active_tasks().await?;
        Ok(runtime)
    }

    /// Attach a notification manager for sending push notifications on task completion.
    pub fn with_notifier(mut self, notifier: Arc<NotificationService>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Attach container manager for container-profile task execution.
    pub fn with_container_manager(mut self, container_manager: Arc<ContainerManager>) -> Self {
        self.container_manager = Some(container_manager);
        self
    }

    pub fn with_ssh_servers(self, ssh_servers: Vec<SshServerConfig>) -> Self {
        self.set_ssh_servers(ssh_servers);
        self
    }

    pub fn set_ssh_servers(&self, ssh_servers: Vec<SshServerConfig>) {
        self.ssh_servers.store(Arc::new(ssh_servers));
    }

    pub fn with_network_policy(self, network_policy: NetworkPolicyConfig) -> Result<Self, String> {
        self.set_network_policy(network_policy)?;
        Ok(self)
    }

    pub fn set_network_policy(&self, network_policy: NetworkPolicyConfig) -> Result<(), String> {
        let compiled = network_policy.compile()?;
        self.network_policy.store(Arc::new(compiled));
        Ok(())
    }

    pub async fn reload_active_tasks(&self) -> Result<(), crate::db::ChatDbError> {
        let tasks = self.db.list_due_tasks().await?;
        self.active_tasks
            .store(Arc::new(TaskCatalog::from_tasks(tasks)));
        Ok(())
    }

    /// Validate that a task script can be parsed in the target profile runtime.
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
        self.validate_task_script_for_profile_model(&profile, script_path, script_inline)
            .await
    }

    /// Create a scheduled task requested by the in-chat `task` tool.
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
            join_text(&[
                "session ",
                normalized.session_id.as_str(),
                " is not attached to a profile",
            ])
        })?;
        let profile = self
            .db
            .get_profile(&profile_id)
            .await
            .map_err(|error| profile_load_error(profile_id.as_str(), &error))?
            .ok_or_else(|| profile_not_found_error(profile_id.as_str()))?;
        let profile_info = ProfileInfo::from_db_model(profile.clone())
            .map_err(|error| invalid_profile_error(profile_id.as_str(), &error))?;
        let actual_runtime = match profile_info.environment {
            ProfileEnvironment::Local => TaskRuntimeKind::Local,
            ProfileEnvironment::Container { .. } => TaskRuntimeKind::Container,
            ProfileEnvironment::Remote { .. } => TaskRuntimeKind::Remote,
        };
        if runtime_kind != actual_runtime {
            return Err(join_text(&[
                "task runtime mismatch: session profile uses ",
                task_runtime_kind_name(actual_runtime),
                ", request declared ",
                task_runtime_kind_name(runtime_kind),
            ]));
        }

        self.validate_task_script_for_profile_model(
            &profile,
            normalized.script.script_path.as_deref(),
            normalized.script.script_inline.as_deref(),
        )
        .await?;

        let task_id = uuid::Uuid::new_v4().to_string();
        let trigger_type = normalized.trigger.storage_trigger_type();
        let model = self
            .db
            .create_scheduled_task(crate::db::CreateScheduledTaskParams {
                id: &task_id,
                profile_id: &profile_id,
                session_id: Some(normalized.session_id.as_str()),
                name: &normalized.name,
                trigger_kind: &normalized.trigger_kind,
                trigger_type: &trigger_type,
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
        let webhook_path = if model.trigger_kind == TRIGGER_KIND_WEBHOOK {
            Some(webhook_path(model.trigger_type.as_str()))
        } else {
            None
        };

        Ok(TaskCreateResult {
            task_id: model.id,
            profile_id: model.profile_id,
            trigger_kind: model.trigger_kind,
            trigger_type: model.trigger_type,
            run_at: model.run_at,
            cron_expression: model.cron_expression,
            webhook_path,
            approved: model.approved,
            enabled: model.enabled,
        })
    }

    pub async fn run_manual_task(
        &self,
        task: scheduled_tasks::Model,
    ) -> Result<(), crate::db::ChatDbError> {
        self.spawn_task(task, TaskInvocation::Manual).await
    }

    /// Check for due tasks and execute them.
    pub async fn tick_due_tasks(&self) -> Result<(), crate::db::ChatDbError> {
        let tasks = self.active_tasks.load();
        let now = jiff::Timestamp::now();

        for task in &tasks.tasks {
            let is_due = self.is_due(task, now)?;

            if is_due {
                self.spawn_task(
                    task.clone(),
                    TaskInvocation::Time {
                        trigger_type: task.trigger_type.clone(),
                    },
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Dispatch event-triggered tasks.
    ///
    /// Returns the number of matching tasks that were started.
    pub async fn dispatch_event(
        &self,
        event_name: &str,
        profile_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<usize, crate::db::ChatDbError> {
        let event_name = normalize_event_name(event_name)
            .map_err(|e| crate::db::ChatDbError::Db(DbErr::Custom(e)))?;
        let tasks = self.active_tasks.load();
        let mut dispatched = 0usize;

        for task in &tasks.tasks {
            if task.trigger_kind != TRIGGER_KIND_EVENT || task.trigger_type != event_name {
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
        &self,
        task: &scheduled_tasks::Model,
        now: jiff::Timestamp,
    ) -> Result<bool, crate::db::ChatDbError> {
        match (task.trigger_kind.as_str(), task.trigger_type.as_str()) {
            (TRIGGER_KIND_TIME, TRIGGER_TYPE_CRON) => self.is_cron_due(task, now),
            (TRIGGER_KIND_TIME, TRIGGER_TYPE_ONCE) => self.is_once_due(task, now),
            (TRIGGER_KIND_TIME, other) => Err(task_trigger_type_error(other, task.id.as_str())),
            (TRIGGER_KIND_EVENT, _) => Ok(false),
            (TRIGGER_KIND_WEBHOOK, _) => Ok(false),
            (other_kind, _) => Err(task_kind_error(other_kind, task.id.as_str())),
        }
    }

    /// Check if a cron task is due.
    fn is_cron_due(
        &self,
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
        &self,
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
            runtime
                .execute_task(run_id.as_str(), &task, invocation)
                .await;
        });
        Ok(())
    }

    async fn validate_task_script_for_profile_model(
        &self,
        profile: &profiles::Model,
        script_path: Option<&str>,
        script_inline: Option<&str>,
    ) -> Result<(), String> {
        let profile_info = ProfileInfo::from_db_model(profile.clone())
            .map_err(|error| invalid_profile_error(profile.id.as_str(), &error))?;
        let script = self
            .resolve_script_source(script_path, script_inline)
            .await?;

        match profile_info.environment {
            ProfileEnvironment::Local => self.validate_local(&profile.id, &script).await,
            ProfileEnvironment::Container { .. } => self.validate_container(profile, &script).await,
            ProfileEnvironment::Remote { .. } => self.validate_remote(profile, &script).await,
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

        let result = self.execute_task_inner(task, invocation).await;

        match result {
            Ok(output) => {
                let truncated = truncate_output(&output);
                if let Err(e) = self
                    .db
                    .record_task_run(&task.id, "success", Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, error = %e, "Failed to record task run");
                }
                if let Err(error) = self
                    .db
                    .finish_task_run(run_id, "success", Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, run_id, error = %error, "Failed to record task run history");
                }

                if let Some(ref notifier) = self.notifier {
                    let (callback_context_xml, callback_message) =
                        task_notification_callback_payload(task, "completed", truncated.as_str());
                    let payload = NotificationPayload {
                        notification_id: None,
                        title: task_notification_title(task.name.as_str()),
                        body: "Completed successfully".to_string(),
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
                    .record_task_run(&task.id, "failed", Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, error = %e, "Failed to record task run");
                }
                if let Err(error) = self
                    .db
                    .finish_task_run(run_id, "failed", Some(&truncated))
                    .await
                {
                    tracing::error!(task_id = %task.id, run_id, error = %error, "Failed to record task run history");
                }

                if let Some(ref notifier) = self.notifier {
                    let (callback_context_xml, callback_message) =
                        task_notification_callback_payload(task, "failed", truncated.as_str());
                    let payload = NotificationPayload {
                        notification_id: None,
                        title: task_notification_title(task.name.as_str()),
                        body: "Execution failed".to_string(),
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
        let profile_info = ProfileInfo::from_db_model(profile.clone())
            .map_err(|error| invalid_profile_error(task.profile_id.as_str(), &error))?;

        let script = self.resolve_script(task).await?;
        let materialized = self
            .materialize_task_execution(task, &script, invocation)
            .await?;

        let result = match profile_info.environment {
            ProfileEnvironment::Local => {
                self.execute_local(task, &script, materialized.command.clone())
                    .await
            }
            ProfileEnvironment::Container { .. } => {
                self.execute_container(task, &profile, materialized.command.clone())
                    .await
            }
            ProfileEnvironment::Remote { .. } => {
                self.execute_remote(&profile, materialized.command.clone())
                    .await
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
        let trigger_kind =
            shell_escape(Cow::Borrowed(Self::invocation_kind(&invocation))).into_owned();
        let trigger_payload = serde_json::to_vec(&TaskExecutionEnvelope::new(task, invocation))
            .map_err(|error| prefixed_error("failed to encode task trigger payload: ", &error))?;

        tokio::fs::write(&script_path, script.content.as_bytes())
            .await
            .map_err(|error| {
                path_error_text("failed to write task script ", &script_path, &error)
            })?;
        tokio::fs::write(&trigger_path, trigger_payload)
            .await
            .map_err(|error| {
                let _ = std::fs::remove_file(&script_path);
                path_error_text(
                    "failed to write task trigger payload ",
                    &trigger_path,
                    &error,
                )
            })?;

        let trigger_file =
            shell_escape(Cow::Owned(trigger_path.display().to_string())).into_owned();
        let script_file = shell_escape(Cow::Owned(script_path.display().to_string())).into_owned();
        let command = TaskExecutionScriptTemplate {
            trigger_file: &trigger_file,
            trigger_kind: &trigger_kind,
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
            if let Err(error) = tokio::fs::remove_file(&path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), error = %error, "failed to remove materialized task file");
                }
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
                        path_error_text("failed to read script_path ", &canonical, &error)
                    })?;
                let canonical_display = canonical.display().to_string();
                let working_dir = canonical
                    .parent()
                    .ok_or_else(|| {
                        join_text(&[
                            "script_path ",
                            canonical_display.as_str(),
                            " has no parent directory",
                        ])
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
        let bash_tool = self
            .local_bash_tool(&task.profile_id, "task_local", script, task.needs_network)
            .await?;
        run_bash_script(
            &bash_tool,
            local_task_execution_mode(task.needs_network),
            None,
            command,
        )
        .await
    }

    async fn validate_local(&self, profile_id: &str, script: &TaskScript) -> Result<(), String> {
        let bash_tool = self
            .local_bash_tool(profile_id, "task_validation_local", script, false)
            .await?;

        run_bash_script(
            &bash_tool,
            local_task_execution_mode(false),
            None,
            build_validation_command(script),
        )
        .await
        .map(|_| ())
    }

    async fn local_bash_tool(
        &self,
        profile_id: &str,
        source: &str,
        script: &TaskScript,
        allow_network: bool,
    ) -> Result<BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured>, String> {
        let shell_sessions = ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: true,
            container: false,
            ssh: false,
        });
        let registry =
            Arc::new(ToolRegistryBuilder::new().build(script.host_working_dir.join("outputs")));
        let network_policy = self.network_policy.load().as_ref().clone();
        BashTool::new_exact(
            &script.host_working_dir,
            SchedulerPermissionHandler {
                db: self.db.clone(),
                profile_id: profile_id.to_string(),
                source: source.to_string(),
                allow_network,
                network_policy,
            },
            DefaultExecutor,
        )
        .await
        .map_err(|error| init_bash_tool_error("failed to initialize local bash tool: ", &error))
        .map(|tool| {
            tool.with_shell_sessions(shell_sessions)
                .with_writable_paths([script.host_working_dir.clone()])
                .with_readable_paths([script.host_working_dir.clone()])
                .with_registry(registry)
        })
    }

    fn resolve_remote_server(&self, profile: &profiles::Model) -> Result<SshServerConfig, String> {
        let configured_servers = self.ssh_servers.load();
        crate::remote_profile::resolve_remote_profile_server(profile, configured_servers.as_slice())
    }

    async fn remote_bash_tool(
        &self,
        profile: &profiles::Model,
        working_dir: &std::path::Path,
    ) -> Result<BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured>, String> {
        let ssh_server = self.resolve_remote_server(profile)?;
        let shell_sessions = ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: false,
            container: false,
            ssh: true,
        })
        .with_ssh_servers(vec![SshServer {
            name: ssh_server.name,
            target: ssh_server.target,
        }])
        .map_err(|error| prefixed_error("failed to configure remote ssh server: ", &error))?;
        let registry = Arc::new(ToolRegistryBuilder::new().build(working_dir.join("outputs")));
        let network_policy = self.network_policy.load().as_ref().clone();
        BashTool::new_exact(
            working_dir,
            SchedulerPermissionHandler {
                db: self.db.clone(),
                profile_id: profile.id.clone(),
                source: "task_remote".to_string(),
                allow_network: true,
                network_policy,
            },
            DefaultExecutor,
        )
        .await
        .map_err(|error| init_bash_tool_error("failed to initialize remote bash tool: ", &error))
        .map(|tool| {
            tool.with_shell_sessions(shell_sessions)
                .with_writable_paths([working_dir.to_path_buf()])
                .with_readable_paths([working_dir.to_path_buf()])
                .with_registry(registry)
        })
    }

    async fn execute_remote(
        &self,
        profile: &profiles::Model,
        command: String,
    ) -> Result<String, String> {
        let working_dir = self.may_dir.join("tasks").join("remote").join(&profile.id);
        std::fs::create_dir_all(working_dir.join("outputs"))
            .map_err(|error| workspace_prepare_error(&working_dir, &error))?;
        let bash_tool = self.remote_bash_tool(profile, &working_dir).await?;
        run_bash_script(&bash_tool, BashExecutionMode::Ssh, None, command).await
    }

    async fn validate_remote(
        &self,
        profile: &profiles::Model,
        script: &TaskScript,
    ) -> Result<(), String> {
        let bash_tool = self
            .remote_bash_tool(profile, script.host_working_dir.as_path())
            .await?;
        run_bash_script(
            &bash_tool,
            BashExecutionMode::Ssh,
            None,
            build_validation_command(script),
        )
        .await
        .map(|_| ())
    }

    async fn execute_container(
        &self,
        _task: &scheduled_tasks::Model,
        profile: &profiles::Model,
        command: String,
    ) -> Result<String, String> {
        let container_manager = self.container_manager.as_ref().ok_or_else(|| {
            "container profile task cannot run: container runtime unavailable".to_string()
        })?;

        let default_image_tag =
            crate::container::profile::default_image_tag(&self.may_dir, profile).map_err(
                |error| {
                    invalid_profile_component_error("invalid container profile image tag: ", &error)
                },
            )?;
        let image_spec = crate::container::profile::image_spec_for_profile(
            &self.may_dir,
            profile,
            &default_image_tag,
        )
        .map_err(|error| {
            invalid_profile_component_error(
                "invalid container profile image configuration: ",
                &error,
            )
        })?;
        let extra_mounts =
            crate::container::profile::mounts_for_profile(profile).map_err(|error| {
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

        let ipc_socket = workspace_dir.join("leash-ipc.sock");
        let mut launch_spec = ContainerLaunchSpec::new(
            container_name,
            image_spec.tag.clone(),
            workspace_dir.clone(),
        )
        .with_ipc_socket(ipc_socket);
        for mount in extra_mounts {
            launch_spec = launch_spec.with_mount(mount);
        }
        let container_id = container_manager
            .ensure_running(&launch_spec)
            .await
            .map_err(|error| prefixed_error("failed to start container: ", &error))?;
        container_manager.touch_activity(&container_id).await;

        let container_runtime = ContainerShellRuntime::new(
            container_id.clone(),
            container_manager.runtime().host_gateway().to_string(),
            ContainerExecHandle::new(Arc::new(BollardContainerExec::new(Arc::new(
                container_manager.runtime().client.clone(),
            )))),
        );
        let shell_sessions = ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: false,
            container: true,
            ssh: false,
        })
        .with_container_runtime(container_runtime);

        // Use host workspace dir for output storage while executing commands in
        // /workspace inside container.
        let registry = Arc::new(ToolRegistryBuilder::new().build(workspace_dir.join("outputs")));
        let network_policy = self.network_policy.load().as_ref().clone();
        let bash_tool: BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured> =
            BashTool::new_exact(
                &workspace_dir,
                SchedulerPermissionHandler {
                    db: self.db.clone(),
                    profile_id: profile.id.clone(),
                    source: "task_container".to_string(),
                    allow_network: true,
                    network_policy,
                },
                DefaultExecutor,
            )
            .await
            .map_err(|error| {
                init_bash_tool_error("failed to initialize container bash tool: ", &error)
            })?
            .with_shell_sessions(shell_sessions)
            .with_writable_paths([workspace_dir.clone()])
            .with_readable_paths([workspace_dir])
            .with_registry(registry);

        let result = run_bash_script(&bash_tool, BashExecutionMode::Default, None, command).await;
        container_manager.touch_activity(&container_id).await;
        result
    }

    #[must_use]
    fn invocation_kind(invocation: &TaskInvocation) -> &'static str {
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
        let container_manager = self.container_manager.as_ref().ok_or_else(|| {
            "container profile task cannot run: container runtime unavailable".to_string()
        })?;

        let default_image_tag =
            crate::container::profile::default_image_tag(&self.may_dir, profile).map_err(
                |error| {
                    invalid_profile_component_error("invalid container profile image tag: ", &error)
                },
            )?;
        let image_spec = crate::container::profile::image_spec_for_profile(
            &self.may_dir,
            profile,
            &default_image_tag,
        )
        .map_err(|error| {
            invalid_profile_component_error(
                "invalid container profile image configuration: ",
                &error,
            )
        })?;
        let extra_mounts =
            crate::container::profile::mounts_for_profile(profile).map_err(|error| {
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

        let ipc_socket = workspace_dir.join("leash-ipc.sock");
        let mut launch_spec = ContainerLaunchSpec::new(
            container_name,
            image_spec.tag.clone(),
            workspace_dir.clone(),
        )
        .with_ipc_socket(ipc_socket);
        for mount in extra_mounts {
            launch_spec = launch_spec.with_mount(mount);
        }
        let container_id = container_manager
            .ensure_running(&launch_spec)
            .await
            .map_err(|error| prefixed_error("failed to start container: ", &error))?;
        container_manager.touch_activity(&container_id).await;

        let container_runtime = ContainerShellRuntime::new(
            container_id.clone(),
            container_manager.runtime().host_gateway().to_string(),
            ContainerExecHandle::new(Arc::new(BollardContainerExec::new(Arc::new(
                container_manager.runtime().client.clone(),
            )))),
        );
        let shell_sessions = ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: false,
            container: true,
            ssh: false,
        })
        .with_container_runtime(container_runtime);

        let registry = Arc::new(ToolRegistryBuilder::new().build(workspace_dir.join("outputs")));
        let network_policy = self.network_policy.load().as_ref().clone();
        let bash_tool: BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured> =
            BashTool::new_exact(
                &workspace_dir,
                SchedulerPermissionHandler {
                    db: self.db.clone(),
                    profile_id: profile.id.clone(),
                    source: "task_validation_container".to_string(),
                    allow_network: true,
                    network_policy,
                },
                DefaultExecutor,
            )
            .await
            .map_err(|error| {
                init_bash_tool_error("failed to initialize container bash tool: ", &error)
            })?
            .with_shell_sessions(shell_sessions)
            .with_writable_paths([workspace_dir.clone()])
            .with_readable_paths([workspace_dir])
            .with_registry(registry);

        let result = run_bash_script(
            &bash_tool,
            BashExecutionMode::Default,
            None,
            build_validation_command(script),
        )
        .await
        .map(|_| ());
        container_manager.touch_activity(&container_id).await;
        result
    }
}

const fn local_task_execution_mode(needs_network: bool) -> BashExecutionMode {
    if needs_network {
        BashExecutionMode::Default
    } else {
        BashExecutionMode::Sandboxed
    }
}

impl TaskScheduler {
    pub fn new(runtime: Arc<TaskRuntime>) -> Self {
        Self { runtime }
    }

    /// Spawn the scheduler as a background task.
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

async fn run_bash_script(
    bash_tool: &BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured>,
    mode: BashExecutionMode,
    ssh_server_id: Option<String>,
    script: String,
) -> Result<String, String> {
    let output = bash_tool
        .call(BashArgs {
            script,
            mode,
            ssh_server_id,
            expect: OutputFormat::Text,
            timeout: 300,
            max_lines: 800,
            raw: false,
        })
        .await
        .map_err(|error| prefixed_error("task bash execution failed: ", &error))?;

    let payload = output
        .as_str()
        .ok_or_else(|| "task bash execution returned non-text output".to_string())?;

    parse_bash_result(payload)
}

fn parse_bash_result(payload: &str) -> Result<String, String> {
    let result: BashResult = serde_json::from_str(payload)
        .map_err(|error| prefixed_error("failed to parse bash result: ", &error))?;

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
            join_text(&[stdout, "\n--- stderr ---\n", stderr_text.as_str()])
        }
        _ => stdout.to_string(),
    }
}

fn truncate_output(output: &str) -> String {
    if output.len() > 10240 {
        join_text(&[&output[..10240], "...(truncated)"])
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

/// Convert a jiff::Timestamp to chrono::DateTime<chrono::Utc> for cron compatibility.
fn timestamp_to_chrono(ts: jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
        .expect("jiff timestamp must fit within chrono DateTime range")
}

#[cfg(test)]
mod tests {
    use super::local_task_execution_mode;
    use aither_sandbox::BashExecutionMode;

    #[test]
    fn local_tasks_respect_needs_network_flag() {
        assert_eq!(local_task_execution_mode(true), BashExecutionMode::Default);
        assert_eq!(
            local_task_execution_mode(false),
            BashExecutionMode::Sandboxed
        );
    }
}
