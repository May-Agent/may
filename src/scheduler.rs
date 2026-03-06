//! Scheduled task execution runtime.
//!
//! Background tokio task that ticks every 30 seconds, checks for due tasks
//! (cron or one-time), and executes them through profile-specific runtimes.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use aither_core::llm::Tool;
use aither_sandbox::{
    BashArgs, BashExecutionMode, BashMode, BashResult, BashTool, BollardContainerExec, Configured,
    ContainerLaunchSpec, OutputFormat, PermissionHandler, ShellRuntimeAvailability,
    ShellSessionRegistry, ToolRegistryBuilder, permission::PermissionError,
};
use executor_core::DefaultExecutor;
use sea_orm::DbErr;

use crate::container::lifecycle::ContainerManager;
use crate::db::ChatDb;
use crate::db::entity::{profiles, scheduled_tasks};
use crate::notification::NotificationManager;
use crate::profile::{ProfileEnvironment, ProfileInfo};
use crate::task_trigger::{
    TRIGGER_KIND_EVENT, TRIGGER_KIND_TIME, TRIGGER_TYPE_CRON, TRIGGER_TYPE_ONCE,
    normalize_event_name,
};

/// Task scheduler that runs in the background.
pub struct TaskScheduler {
    db: ChatDb,
    notifier: Option<Arc<NotificationManager>>,
    may_dir: PathBuf,
    container_manager: Option<Arc<ContainerManager>>,
}

#[derive(Debug, Clone)]
struct TaskScript {
    content: String,
    host_working_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct SchedulerPermissionHandler;

impl PermissionHandler for SchedulerPermissionHandler {
    async fn check(&self, _mode: BashMode, _script: &str) -> Result<bool, PermissionError> {
        Ok(true)
    }

    async fn check_domain(&self, _domain: &str, _port: u16) -> bool {
        true
    }
}

impl TaskScheduler {
    pub fn new(db: ChatDb, may_dir: PathBuf) -> Self {
        Self {
            db,
            notifier: None,
            may_dir,
            container_manager: None,
        }
    }

    /// Attach a notification manager for sending push notifications on task completion.
    pub fn with_notifier(mut self, notifier: Arc<NotificationManager>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Attach container manager for container-profile task execution.
    pub fn with_container_manager(mut self, container_manager: Arc<ContainerManager>) -> Self {
        self.container_manager = Some(container_manager);
        self
    }

    /// Spawn the scheduler as a background task.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = self.tick().await {
                    tracing::error!(error = %e, "Scheduler tick failed");
                }
            }
        })
    }

    /// Check for due tasks and execute them.
    async fn tick(&self) -> Result<(), crate::db::ChatDbError> {
        let tasks = self.db.list_due_tasks().await?;
        let now = jiff::Timestamp::now();

        for task in tasks {
            let is_due = self.is_due(&task, now)?;

            if is_due {
                self.execute_task(&task).await;
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
        let tasks = self.db.list_due_tasks().await?;
        let mut dispatched = 0usize;

        for task in tasks {
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
            self.execute_task_by_model(&task).await;
        }

        Ok(dispatched)
    }

    fn is_due(
        &self,
        task: &scheduled_tasks::Model,
        now: jiff::Timestamp,
    ) -> Result<bool, crate::db::ChatDbError> {
        match (task.trigger_kind.as_str(), task.trigger_type.as_str()) {
            (TRIGGER_KIND_TIME, TRIGGER_TYPE_CRON) => self.is_cron_due(task, now),
            (TRIGGER_KIND_TIME, TRIGGER_TYPE_ONCE) => self.is_once_due(task, now),
            (TRIGGER_KIND_TIME, other) => Err(crate::db::ChatDbError::Db(DbErr::Custom(format!(
                "unsupported time trigger_type '{other}' for task {}",
                task.id
            )))),
            (TRIGGER_KIND_EVENT, _) => Ok(false),
            (other_kind, _) => Err(crate::db::ChatDbError::Db(DbErr::Custom(format!(
                "unsupported trigger_kind '{other_kind}' for task {}",
                task.id
            )))),
        }
    }

    /// Check if a cron task is due.
    fn is_cron_due(
        &self,
        task: &scheduled_tasks::Model,
        now: jiff::Timestamp,
    ) -> Result<bool, crate::db::ChatDbError> {
        let Some(ref expr) = task.cron_expression else {
            return Err(crate::db::ChatDbError::Db(DbErr::Custom(format!(
                "cron task {} is missing cron_expression",
                task.id
            ))));
        };

        let schedule = cron::Schedule::from_str(expr).map_err(|e| {
            crate::db::ChatDbError::Db(DbErr::Custom(format!(
                "invalid cron expression stored in task {}: {e}",
                task.id
            )))
        })?;

        // Check if any scheduled time falls within the last 30 seconds.
        let now_chrono = timestamp_to_chrono(now);
        let window_start = now_chrono - chrono::Duration::seconds(30);

        // Skip if we already ran within this window.
        if let Some(ref last_run) = task.last_run_at {
            let last = jiff::Timestamp::from_str(last_run).map_err(|e| {
                crate::db::ChatDbError::Db(DbErr::Custom(format!(
                    "invalid last_run_at stored in task {}: {e}",
                    task.id
                )))
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
            return Err(crate::db::ChatDbError::Db(DbErr::Custom(format!(
                "one-time task {} is missing run_at",
                task.id
            ))));
        };

        // Skip if already executed.
        if task.last_run_at.is_some() {
            return Ok(false);
        }

        let target = jiff::Timestamp::from_str(run_at).map_err(|e| {
            crate::db::ChatDbError::Db(DbErr::Custom(format!(
                "invalid run_at stored in task {}: {e}",
                task.id
            )))
        })?;

        Ok(now >= target)
    }

    /// Execute a task (run its script). Can be called externally for manual triggers.
    pub async fn execute_task_by_model(&self, task: &scheduled_tasks::Model) {
        self.execute_task(task).await;
    }

    /// Execute a task (run its script).
    async fn execute_task(&self, task: &scheduled_tasks::Model) {
        tracing::info!(
            task_id = %task.id,
            name = %task.name,
            trigger = %task.trigger_type,
            profile_id = %task.profile_id,
            "Executing scheduled task"
        );

        let result = self.execute_task_inner(task).await;

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

                if let Some(ref notifier) = self.notifier {
                    let title = format!("Task: {}", task.name);
                    if let Err(e) = notifier.send(&title, "Completed successfully").await {
                        tracing::warn!(task_id = %task.id, error = %e, "Failed to send push notification");
                    }
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

                if let Some(ref notifier) = self.notifier {
                    let title = format!("Task: {}", task.name);
                    if let Err(e) = notifier.send(&title, "Execution failed").await {
                        tracing::warn!(task_id = %task.id, error = %e, "Failed to send push notification");
                    }
                }

                tracing::error!(task_id = %task.id, error = %error_message, "Scheduled task execution failed");
            }
        }
    }

    async fn execute_task_inner(&self, task: &scheduled_tasks::Model) -> Result<String, String> {
        let profile = self
            .db
            .get_profile(&task.profile_id)
            .await
            .map_err(|e| format!("failed to load profile {}: {e}", task.profile_id))?
            .ok_or_else(|| format!("profile {} not found", task.profile_id))?;
        let profile_info = ProfileInfo::from_db_model(profile.clone())
            .map_err(|e| format!("invalid profile {}: {e}", task.profile_id))?;

        let script = self.resolve_script(task).await?;

        match profile_info.environment {
            ProfileEnvironment::Local => self.execute_local(task, &script).await,
            ProfileEnvironment::Container { .. } => {
                self.execute_container(task, &profile, &script).await
            }
        }
    }

    async fn resolve_script(&self, task: &scheduled_tasks::Model) -> Result<TaskScript, String> {
        match (&task.script_inline, &task.script_path) {
            (Some(inline), None) => Ok(TaskScript {
                content: inline.clone(),
                host_working_dir: self.may_dir.clone(),
            }),
            (None, Some(path)) => {
                let canonical = tokio::fs::canonicalize(path)
                    .await
                    .map_err(|e| format!("failed to resolve script_path {path}: {e}"))?;
                let content = tokio::fs::read_to_string(&canonical).await.map_err(|e| {
                    format!("failed to read script_path {}: {e}", canonical.display())
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

    async fn execute_local(
        &self,
        _task: &scheduled_tasks::Model,
        script: &TaskScript,
    ) -> Result<String, String> {
        let shell_sessions = ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: true,
            container: false,
            ssh: false,
        });

        let registry =
            Arc::new(ToolRegistryBuilder::new().build(script.host_working_dir.join("outputs")));
        let bash_tool: BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured> =
            BashTool::new_exact(
                &script.host_working_dir,
                SchedulerPermissionHandler,
                DefaultExecutor,
            )
            .await
            .map_err(|e| format!("failed to initialize local bash tool: {e}"))?
            .with_shell_sessions(shell_sessions)
            .with_writable_paths([script.host_working_dir.clone()])
            .with_readable_paths([script.host_working_dir.clone()])
            .with_registry(registry);

        run_bash_script(
            &bash_tool,
            BashExecutionMode::Default,
            None,
            script.content.clone(),
        )
        .await
    }

    async fn execute_container(
        &self,
        _task: &scheduled_tasks::Model,
        profile: &profiles::Model,
        script: &TaskScript,
    ) -> Result<String, String> {
        let container_manager = self.container_manager.as_ref().ok_or_else(|| {
            "container profile task cannot run: container runtime unavailable".to_string()
        })?;

        let default_image_tag =
            crate::container::profile::default_image_tag(&self.may_dir, profile)
                .map_err(|e| format!("invalid container profile image tag: {e}"))?;
        let image_spec = crate::container::profile::image_spec_for_profile(
            &self.may_dir,
            profile,
            &default_image_tag,
        )
        .map_err(|e| format!("invalid container profile image configuration: {e}"))?;
        let extra_mounts = crate::container::profile::mounts_for_profile(profile)
            .map_err(|e| format!("invalid container profile mount configuration: {e}"))?;

        crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
            .await
            .map_err(|e| format!("container image check/build failed: {e}"))?;

        let container_name = format!("may-{}", profile.slug.as_deref().unwrap_or(&profile.id));

        let workspace_dir = self
            .may_dir
            .join("task-workspaces")
            .join(profile.slug.as_deref().unwrap_or(&profile.id));
        tokio::fs::create_dir_all(&workspace_dir)
            .await
            .map_err(|e| {
                format!(
                    "failed to prepare container workspace {}: {e}",
                    workspace_dir.display()
                )
            })?;

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
            .map_err(|e| format!("failed to start container: {e}"))?;
        container_manager.touch_activity(&container_id).await;

        let shell_sessions = ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: false,
            container: true,
            ssh: false,
        });
        shell_sessions.set_container_exec(Arc::new(BollardContainerExec::new(Arc::new(
            container_manager.runtime().client.clone(),
        ))));
        shell_sessions.set_container_id(container_id.clone());

        // Use host workspace dir for output storage while executing commands in
        // /workspace inside container.
        let registry = Arc::new(ToolRegistryBuilder::new().build(workspace_dir.join("outputs")));
        let bash_tool: BashTool<SchedulerPermissionHandler, DefaultExecutor, Configured> =
            BashTool::new_exact(&workspace_dir, SchedulerPermissionHandler, DefaultExecutor)
                .await
                .map_err(|e| format!("failed to initialize container bash tool: {e}"))?
                .with_shell_sessions(shell_sessions)
                .with_writable_paths([workspace_dir.clone()])
                .with_readable_paths([workspace_dir])
                .with_registry(registry);

        let result = run_bash_script(
            &bash_tool,
            BashExecutionMode::Default,
            None,
            script.content.clone(),
        )
        .await;
        container_manager.touch_activity(&container_id).await;
        result
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
        .map_err(|e| format!("task bash execution failed: {e}"))?;

    let payload = output
        .as_str()
        .ok_or_else(|| "task bash execution returned non-text output".to_string())?;

    parse_bash_result(payload)
}

fn parse_bash_result(payload: &str) -> Result<String, String> {
    let result: BashResult =
        serde_json::from_str(payload).map_err(|e| format!("failed to parse bash result: {e}"))?;

    if result.exit_code != 0 {
        return Err(format!(
            "task script failed with exit code {}\n{}",
            result.exit_code,
            combine_output(
                &result.stdout.to_string(),
                result.stderr.as_ref().map(ToString::to_string)
            )
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

/// Convert a jiff::Timestamp to chrono::DateTime<chrono::Utc> for cron compatibility.
fn timestamp_to_chrono(ts: jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
        .expect("jiff timestamp must fit within chrono DateTime range")
}
