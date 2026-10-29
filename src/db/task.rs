//! Scheduled-task CRUD and task-run lifecycle queries on [`ChatDb`].

use crate::scheduler::TaskRunStatus;
use crate::task_trigger::TRIGGER_KIND_WEBHOOK;

use super::common::{CreateScheduledTaskParams, UpdateScheduledTaskParams};
use super::entity::scheduled_tasks;
use super::{ChatDb, ChatDbError, query};

impl ChatDb {
    /// Create a scheduled task.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the insert fails or the id collides.
    pub async fn create_scheduled_task(
        &self,
        params: CreateScheduledTaskParams<'_>,
    ) -> Result<scheduled_tasks::Model, ChatDbError> {
        let trigger_kind = params.trigger.kind().as_str();
        let trigger_type = params.trigger.storage_trigger_type();
        let now = jiff::Timestamp::now().to_string();
        let timeout_secs = i64::try_from(params.timeout_secs)
            .expect("task timeout is validated against the background cap before it is stored");
        query!(
            self.db,
            "INSERT INTO scheduled_tasks (id, profile_id, session_id, name, trigger_kind, trigger_type, cron_expression, run_at, script_path, script_inline, needs_network, timeout_secs, approved, enabled, last_run_at, last_run_status, last_run_output, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params.id,
            params.profile_id,
            params.session_id,
            params.name,
            trigger_kind,
            trigger_type,
            params.cron_expression,
            params.run_at,
            params.script_path,
            params.script_inline,
            params.needs_network,
            timeout_secs,
            false,
            true,
            Option::<String>::None,
            Option::<String>::None,
            Option::<String>::None,
            now.clone(),
            now.clone()
        )
        .execute()
        .await?;
        Ok(scheduled_tasks::Model {
            id: params.id.to_string(),
            profile_id: params.profile_id.to_string(),
            session_id: params.session_id.map(ToOwned::to_owned),
            name: params.name.to_string(),
            trigger_kind: trigger_kind.to_string(),
            trigger_type: trigger_type.to_string(),
            cron_expression: params.cron_expression.map(ToOwned::to_owned),
            run_at: params.run_at.map(ToOwned::to_owned),
            script_path: params.script_path.map(ToOwned::to_owned),
            script_inline: params.script_inline.map(ToOwned::to_owned),
            needs_network: params.needs_network,
            timeout_secs,
            approved: false,
            enabled: true,
            last_run_at: None,
            last_run_status: None,
            last_run_output: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Get a scheduled task by ID.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RowNotFound`] when no task with `id` exists, or [`ChatDbError`] on storage failure.
    pub async fn get_scheduled_task(
        &self,
        id: &str,
    ) -> Result<Option<scheduled_tasks::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM scheduled_tasks WHERE id = ?", id)
                .fetch_optional::<scheduled_tasks::Model>()
                .await?,
        )
    }

    /// List all scheduled tasks, optionally filtered by profile.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_scheduled_tasks(
        &self,
        profile_id: Option<&str>,
    ) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        let sql = if profile_id.is_some() {
            "SELECT * FROM scheduled_tasks WHERE profile_id = ? ORDER BY created_at DESC"
        } else {
            "SELECT * FROM scheduled_tasks ORDER BY created_at DESC"
        };
        let rows = match profile_id {
            Some(profile_id) => {
                query!(self.db, sql, profile_id)
                    .fetch_all::<scheduled_tasks::Model>()
                    .await?
            }
            None => {
                query!(self.db, sql)
                    .fetch_all::<scheduled_tasks::Model>()
                    .await?
            }
        };
        Ok(rows)
    }

    /// Update a scheduled task's mutable fields.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RowNotFound`] when no task with `id` exists, or [`ChatDbError`] on storage failure.
    pub async fn update_scheduled_task(
        &self,
        id: &str,
        params: UpdateScheduledTaskParams<'_>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let name_enabled = params.name.is_some();
        let enabled_enabled = params.enabled.is_some();
        let approved_enabled = params.approved.is_some();
        let cron_enabled = params.cron_expression.is_some();
        let run_at_enabled = params.run_at.is_some();
        let script_path_enabled = params.script_path.is_some();
        let script_inline_enabled = params.script_inline.is_some();
        let timeout_enabled = params.timeout_secs.is_some();
        let timeout_secs = params.timeout_secs.map(|secs| {
            i64::try_from(secs)
                .expect("task timeout is validated against the background cap before it is stored")
        });
        query!(
            self.db,
            "UPDATE scheduled_tasks SET updated_at = ?, name = CASE WHEN ? THEN ? ELSE name END, enabled = CASE WHEN ? THEN ? ELSE enabled END, approved = CASE WHEN ? THEN ? ELSE approved END, cron_expression = CASE WHEN ? THEN ? ELSE cron_expression END, run_at = CASE WHEN ? THEN ? ELSE run_at END, script_path = CASE WHEN ? THEN ? ELSE script_path END, script_inline = CASE WHEN ? THEN ? ELSE script_inline END, timeout_secs = CASE WHEN ? THEN ? ELSE timeout_secs END WHERE id = ?",
            now,
            name_enabled,
            params.name,
            enabled_enabled,
            params.enabled.unwrap_or(false),
            approved_enabled,
            params.approved.unwrap_or(false),
            cron_enabled,
            params.cron_expression.flatten(),
            run_at_enabled,
            params.run_at.flatten(),
            script_path_enabled,
            params.script_path.flatten(),
            script_inline_enabled,
            params.script_inline.flatten(),
            timeout_enabled,
            timeout_secs.unwrap_or(0),
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Delete a scheduled task.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "DELETE FROM scheduled_tasks WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Approve a task for execution.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn approve_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_tasks SET approved = TRUE, updated_at = ? WHERE id = ?",
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Record a task execution result.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn record_task_run(
        &self,
        id: &str,
        status: TaskRunStatus,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_tasks SET last_run_at = ?, last_run_status = ?, last_run_output = ?, updated_at = ? WHERE id = ?",
            now.clone(),
            status.as_str(),
            output,
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Mark a task run as started and persist it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn start_task_run(
        &self,
        id: &str,
        task: &scheduled_tasks::Model,
        invocation_kind: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO scheduled_task_runs (id, task_id, profile_id, session_id, invocation_kind, status, output, started_at, finished_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            task.id.as_str(),
            task.profile_id.as_str(),
            task.session_id.as_deref(),
            invocation_kind,
            TaskRunStatus::Running.as_str(),
            Option::<String>::None,
            now.clone(),
            Option::<String>::None,
            now
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Record the outcome of a task run and mark it finished.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn finish_task_run(
        &self,
        id: &str,
        status: TaskRunStatus,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE scheduled_task_runs SET status = ?, output = ?, finished_at = ?, updated_at = ? WHERE id = ?",
            status.as_str(),
            output,
            Some(now.clone()),
            now,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List enabled and approved tasks.
    ///
    /// Due-ness is resolved by the scheduler runtime because it depends on
    /// trigger-kind-specific rules.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_due_tasks(&self) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM scheduled_tasks WHERE enabled = TRUE AND approved = TRUE"
        )
        .fetch_all::<scheduled_tasks::Model>()
        .await?)
    }

    /// List tasks whose webhook trigger token matches `token`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_tasks_for_webhook(
        &self,
        token: &str,
    ) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM scheduled_tasks WHERE enabled = TRUE AND approved = TRUE AND trigger_kind = ? AND trigger_type = ?",
            TRIGGER_KIND_WEBHOOK,
            token
        )
        .fetch_all::<scheduled_tasks::Model>()
        .await?)
    }
}
