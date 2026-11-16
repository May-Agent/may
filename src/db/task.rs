//! Scheduled-task CRUD and task-run lifecycle queries on [`ChatDb`].

use crate::scheduler::TaskRunStatus;
use crate::task_trigger::{PersistedTaskTrigger, TRIGGER_KIND_TIME, TRIGGER_KIND_WEBHOOK};

use super::common::{CreateScheduledTaskParams, UpdateScheduledTaskParams};
use super::entity::scheduled_tasks;
use super::{ChatDb, ChatDbError, query};

/// The session and word of a deleted task.
#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct NamedTaskRow {
    session_id: Option<String>,
    word: Option<String>,
}

impl ChatDb {
    /// Create a scheduled task. A task bound to a session is named for the
    /// model by a word of that session's pool, kept leased while the task
    /// exists.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the insert fails or the id collides, or
    /// every word of the session's pool is leased.
    pub async fn create_scheduled_task(
        &self,
        params: CreateScheduledTaskParams<'_>,
    ) -> Result<scheduled_tasks::Model, ChatDbError> {
        let named = match params.session_id {
            Some(session_id) => {
                let ids = self.session_ids(session_id).await?;
                let lease = ids.lease()?;
                Some((ids, lease))
            }
            None => None,
        };
        let word = named.as_ref().map(|(_, lease)| lease.to_string());
        let trigger_kind = params.trigger.kind().as_str();
        let trigger_type = params.trigger.storage_trigger_type();
        let now_at = jiff::Timestamp::now();
        let now = now_at.to_string();
        let next_fire_at = match params.trigger {
            PersistedTaskTrigger::Time { trigger_type } => Some(
                trigger_type
                    .next_fire(params.cron_expression, params.run_at, now_at)
                    .map_err(|error| {
                        ChatDbError::Storage(format!(
                            "invalid time trigger for a new task: {error}"
                        ))
                    })?
                    .to_string(),
            ),
            PersistedTaskTrigger::Event { .. } | PersistedTaskTrigger::Webhook { .. } => None,
        };
        let timeout_secs = i64::from(params.timeout_secs);
        let signature_secret = params
            .trigger
            .webhook_signature()
            .map(|signature| signature.secret.clone());
        let signature_header = params
            .trigger
            .webhook_signature()
            .map(|signature| signature.header.clone());
        query!(
            self.db,
            "INSERT INTO scheduled_tasks (id, profile_id, session_id, name, trigger_kind, trigger_type, cron_expression, run_at, script_path, script_inline, needs_network, timeout_secs, approved, enabled, last_run_at, last_run_status, last_run_output, created_at, updated_at, webhook_signature_secret, webhook_signature_header, word, next_fire_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            now.clone(),
            signature_secret.clone(),
            signature_header.clone(),
            word.as_deref(),
            next_fire_at.clone()
        )
        .execute()
        .await?;
        if let Some((ids, lease)) = named {
            ids.keep(lease);
        }
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
            webhook_signature_secret: signature_secret,
            webhook_signature_header: signature_header,
            word,
            next_fire_at,
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
        let timeout_secs = params.timeout_secs.map(i64::from);
        let reschedules =
            cron_enabled || run_at_enabled || params.enabled.is_some() || params.approved.is_some();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
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
        if reschedules {
            Self::refresh_next_fire_in_tx(&mut tx, id).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Recompute the next fire of task `id` from now, after a change to its
    /// trigger, `enabled` or `approved`: a fire that fell while the task was
    /// off or changing is not run late.
    async fn refresh_next_fire_in_tx(
        tx: &mut skyzen_services::DbTransaction,
        id: &str,
    ) -> Result<(), ChatDbError> {
        let Some(task) = query!(tx, "SELECT * FROM scheduled_tasks WHERE id = ?", id)
            .fetch_optional::<scheduled_tasks::Model>()
            .await?
        else {
            return Ok(());
        };
        let next = Self::next_fire_for_row(tx, &task, jiff::Timestamp::now()).await?;
        query!(
            tx,
            "UPDATE scheduled_tasks SET next_fire_at = ? WHERE id = ?",
            next.map(|next| next.to_string()),
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    async fn next_fire_for_row(
        tx: &mut skyzen_services::DbTransaction,
        task: &scheduled_tasks::Model,
        after: jiff::Timestamp,
    ) -> Result<Option<jiff::Timestamp>, ChatDbError> {
        let ran = query!(
            tx,
            "SELECT COUNT(*) AS value FROM scheduled_task_runs WHERE task_id = ? AND invocation_kind = ?",
            task.id.as_str(),
            TRIGGER_KIND_TIME
        )
        .fetch_all::<super::common::I64ValueRow>()
        .await?
        .into_iter()
        .any(|row| row.value > 0);
        task.next_fire_after(after, ran).map_err(|error| {
            ChatDbError::Storage(format!(
                "invalid time trigger stored in task {}: {error}",
                task.id
            ))
        })
    }

    /// Give every time-triggered task without a next fire time one: the rows
    /// a build without the column wrote, or a trigger edited around it. A
    /// one-time task that already ran keeps none. A task whose trigger cannot
    /// be read is reported and skipped.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn backfill_next_fire_times(&self) -> Result<(), ChatDbError> {
        let tasks = query!(
            self.db,
            "SELECT * FROM scheduled_tasks WHERE trigger_kind = ? AND next_fire_at IS NULL",
            TRIGGER_KIND_TIME
        )
        .fetch_all::<scheduled_tasks::Model>()
        .await?;
        for task in tasks {
            let mut tx = self.db.begin().await?;
            let next = match Self::next_fire_for_row(&mut tx, &task, jiff::Timestamp::now()).await {
                Ok(next) => next,
                Err(error) => {
                    tracing::error!(task_id = %task.id, %error, "task has no readable next fire time; skipped");
                    continue;
                }
            };
            query!(
                tx,
                "UPDATE scheduled_tasks SET next_fire_at = ? WHERE id = ? AND next_fire_at IS NULL",
                next.map(|next| next.to_string()),
                task.id.as_str()
            )
            .execute()
            .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// Start the run of a fire that was due at `due` (the task's stored
    /// `next_fire_at`) and move the task's next fire to `next`, in one
    /// transaction. Returns `false` and starts nothing when another writer
    /// already advanced the task past `due`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn start_time_run(
        &self,
        run_id: &str,
        task: &scheduled_tasks::Model,
        due: &str,
        next: Option<jiff::Timestamp>,
    ) -> Result<bool, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let advanced = query!(
            tx,
            "UPDATE scheduled_tasks SET next_fire_at = ? WHERE id = ? AND next_fire_at = ?",
            next.map(|next| next.to_string()),
            task.id.as_str(),
            due
        )
        .execute()
        .await?;
        if advanced.rows_written == 0 {
            return Ok(false);
        }
        Self::insert_task_run_in_tx(&mut tx, run_id, task, TRIGGER_KIND_TIME).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Delete a scheduled task.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        let deleted = query!(
            self.db,
            "DELETE FROM scheduled_tasks WHERE id = ? RETURNING session_id, word",
            id
        )
        .fetch_optional::<NamedTaskRow>()
        .await?;
        if let Some(NamedTaskRow {
            session_id: Some(session_id),
            word: Some(word),
        }) = deleted
        {
            self.release_session_word(&session_id, &word);
        }
        Ok(())
    }

    /// Store a task created by `session_id` approved and enabled when the
    /// session is in yolo mode; otherwise return it unchanged.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn grant_task_under_yolo(
        &self,
        session_id: &str,
        task: scheduled_tasks::Model,
    ) -> Result<scheduled_tasks::Model, ChatDbError> {
        if !self.session_yolo(session_id).await? {
            return Ok(task);
        }
        self.update_scheduled_task(
            &task.id,
            UpdateScheduledTaskParams {
                enabled: Some(true),
                approved: Some(true),
                ..Default::default()
            },
        )
        .await?;
        self.get_scheduled_task(&task.id)
            .await?
            .ok_or_else(|| ChatDbError::RowNotFound(format!("scheduled task {}", task.id)))
    }

    /// Approve a task for execution.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn approve_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "UPDATE scheduled_tasks SET approved = TRUE, updated_at = ? WHERE id = ?",
            now,
            id
        )
        .execute()
        .await?;
        Self::refresh_next_fire_in_tx(&mut tx, id).await?;
        tx.commit().await?;
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
        let mut tx = self.db.begin().await?;
        Self::insert_task_run_in_tx(&mut tx, id, task, invocation_kind).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert_task_run_in_tx(
        tx: &mut skyzen_services::DbTransaction,
        id: &str,
        task: &scheduled_tasks::Model,
        invocation_kind: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            tx,
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

    /// Runs started for `task_id`, for tests that count fires.
    #[cfg(test)]
    pub(crate) async fn task_run_count(&self, task_id: &str) -> Result<i64, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT COUNT(*) AS value FROM scheduled_task_runs WHERE task_id = ?",
            task_id
        )
        .fetch_all::<super::common::I64ValueRow>()
        .await?;
        Ok(rows.into_iter().map(|row| row.value).sum())
    }

    /// Status of one task run, for tests that check what recovery did.
    #[cfg(test)]
    pub(crate) async fn task_run_status(&self, id: &str) -> Result<Option<String>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT status AS value FROM scheduled_task_runs WHERE id = ?",
            id
        )
        .fetch_all::<super::common::StringValueRow>()
        .await?;
        Ok(rows.into_iter().next().map(|row| row.value))
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

    /// List every enabled, approved webhook task. The caller matches the
    /// presented token against each one in constant time, so the lookup never
    /// reveals how much of a token was right.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_webhook_tasks(&self) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM scheduled_tasks WHERE enabled = TRUE AND approved = TRUE AND trigger_kind = ?",
            TRIGGER_KIND_WEBHOOK
        )
        .fetch_all::<scheduled_tasks::Model>()
        .await?)
    }
}
