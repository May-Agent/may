//! Scheduled task REST API.

use std::str::FromStr;
use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::db::{CreateScheduledTaskParams, UpdateScheduledTaskParams};
use crate::task_trigger::{
    TRIGGER_TYPE_CRON, TRIGGER_TYPE_ONCE, TaskTriggerKind, normalize_event_name,
};

use super::error::{bad_request_error, internal_error};
use super::state::UiServerState;

// ── Response type ──────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ScheduledTaskResponse {
    pub id: String,
    pub profile_id: String,
    pub session_id: Option<String>,
    pub name: String,
    pub trigger_kind: String,
    pub trigger_type: String,
    pub cron_expression: Option<String>,
    pub run_at: Option<String>,
    pub script_path: Option<String>,
    pub script_inline: Option<String>,
    pub needs_network: bool,
    pub approved: bool,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub last_run_status: Option<String>,
    pub last_run_output: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ScheduledTaskResponse {
    fn from_model(m: crate::db::entity::scheduled_tasks::Model) -> Self {
        Self {
            id: m.id,
            profile_id: m.profile_id,
            session_id: m.session_id,
            name: m.name,
            trigger_kind: m.trigger_kind,
            trigger_type: m.trigger_type,
            cron_expression: m.cron_expression,
            run_at: m.run_at,
            script_path: m.script_path,
            script_inline: m.script_inline,
            needs_network: m.needs_network,
            approved: m.approved,
            enabled: m.enabled,
            last_run_at: m.last_run_at,
            last_run_status: m.last_run_status,
            last_run_output: m.last_run_output,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

// ── Request types ──────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateTaskRequest {
    pub profile_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub name: String,
    pub trigger_kind: String,
    pub trigger_type: String,
    #[serde(default)]
    pub cron_expression: Option<String>,
    #[serde(default)]
    pub run_at: Option<String>,
    #[serde(default)]
    pub script_path: Option<String>,
    #[serde(default)]
    pub script_inline: Option<String>,
    #[serde(default)]
    pub needs_network: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateTaskRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub cron_expression: Option<String>,
    #[serde(default)]
    pub run_at: Option<String>,
    #[serde(default)]
    pub script_path: Option<String>,
    #[serde(default)]
    pub script_inline: Option<String>,
}

fn parse_trigger_kind(value: &str) -> SkyResult<TaskTriggerKind> {
    value
        .trim()
        .parse::<TaskTriggerKind>()
        .map_err(bad_request_error)
}

fn normalize_trigger_type(kind: TaskTriggerKind, value: &str) -> SkyResult<String> {
    let trigger_type = value.trim().to_lowercase();
    if trigger_type.is_empty() {
        return Err(bad_request_error("trigger_type must not be empty"));
    }
    match kind {
        TaskTriggerKind::Time => match trigger_type.as_str() {
            TRIGGER_TYPE_CRON | TRIGGER_TYPE_ONCE => Ok(trigger_type),
            other => Err(bad_request_error(format!(
                "Unsupported trigger_type '{other}' for time trigger, expected one of: {TRIGGER_TYPE_CRON}, {TRIGGER_TYPE_ONCE}"
            ))),
        },
        TaskTriggerKind::Event => normalize_event_name(&trigger_type).map_err(bad_request_error),
    }
}

fn parse_utc_run_at(run_at: &str) -> SkyResult<String> {
    let value = run_at.trim();
    if !value.ends_with('Z') {
        return Err(bad_request_error(
            "run_at must be an ISO 8601 UTC timestamp ending with 'Z'",
        ));
    }
    let parsed = jiff::Timestamp::from_str(value)
        .map_err(|e| bad_request_error(format!("Invalid UTC timestamp for run_at: {e}")))?;
    Ok(parsed.to_string())
}

fn validate_trigger_fields(
    trigger_kind: TaskTriggerKind,
    trigger_type: &str,
    cron_expression: Option<&str>,
    run_at: Option<&str>,
) -> SkyResult<()> {
    match trigger_kind {
        TaskTriggerKind::Time => match trigger_type {
            TRIGGER_TYPE_CRON => {
                let expr = cron_expression
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad_request_error("cron trigger requires cron_expression"))?;
                cron::Schedule::from_str(expr)
                    .map_err(|e| bad_request_error(format!("Invalid cron expression: {e}")))?;
                if run_at.is_some() {
                    return Err(bad_request_error(
                        "cron trigger must not include run_at; use cron_expression only",
                    ));
                }
            }
            TRIGGER_TYPE_ONCE => {
                let ts = run_at
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad_request_error("once trigger requires run_at in UTC"))?;
                let _ = parse_utc_run_at(ts)?;
                if cron_expression.is_some() {
                    return Err(bad_request_error(
                        "once trigger must not include cron_expression",
                    ));
                }
            }
            other => {
                return Err(bad_request_error(format!(
                    "Unsupported time trigger_type '{other}'"
                )));
            }
        },
        TaskTriggerKind::Event => {
            if run_at.is_some() || cron_expression.is_some() {
                return Err(bad_request_error(
                    "event trigger must not include run_at or cron_expression",
                ));
            }
        }
    }
    Ok(())
}

fn validate_script_fields(script_path: Option<&str>, script_inline: Option<&str>) -> SkyResult<()> {
    let has_path = script_path
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_inline = script_inline
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    match (has_path, has_inline) {
        (true, true) => Err(bad_request_error(
            "Provide either script_path or script_inline, not both",
        )),
        (false, false) => Err(bad_request_error(
            "Either script_path or script_inline is required",
        )),
        _ => Ok(()),
    }
}

// ── Endpoints ──────────────────────────────────────────────

pub(super) async fn list_tasks(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<ScheduledTaskResponse>>> {
    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    let tasks = db
        .list_scheduled_tasks(None)
        .await
        .map_err(internal_error)?;
    Ok(Json(
        tasks
            .into_iter()
            .map(ScheduledTaskResponse::from_model)
            .collect(),
    ))
}

pub(super) async fn create_task(
    Json(req): Json<CreateTaskRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ScheduledTaskResponse>> {
    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    let id = uuid::Uuid::new_v4().to_string();
    let trigger_kind = parse_trigger_kind(&req.trigger_kind)?;
    let trigger_type = normalize_trigger_type(trigger_kind, &req.trigger_type)?;
    let normalized_run_at = req.run_at.as_deref().map(parse_utc_run_at).transpose()?;
    let normalized_cron = req
        .cron_expression
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let normalized_script_path = req
        .script_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let normalized_script_inline = req
        .script_inline
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    validate_trigger_fields(
        trigger_kind,
        &trigger_type,
        normalized_cron,
        normalized_run_at.as_deref(),
    )?;
    validate_script_fields(normalized_script_path, normalized_script_inline)?;

    let model = db
        .create_scheduled_task(CreateScheduledTaskParams {
            id: &id,
            profile_id: &req.profile_id,
            session_id: req.session_id.as_deref(),
            name: &req.name,
            trigger_kind: trigger_kind.as_str(),
            trigger_type: &trigger_type,
            cron_expression: normalized_cron,
            run_at: normalized_run_at.as_deref(),
            script_path: normalized_script_path,
            script_inline: normalized_script_inline,
            needs_network: req.needs_network,
        })
        .await
        .map_err(internal_error)?;

    Ok(Json(ScheduledTaskResponse::from_model(model)))
}

pub(super) async fn get_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ScheduledTaskResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    let model = db
        .get_scheduled_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Task not found"))?;

    Ok(Json(ScheduledTaskResponse::from_model(model)))
}

pub(super) async fn update_task(
    params: Params,
    Json(req): Json<UpdateTaskRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ScheduledTaskResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    let current = db
        .get_scheduled_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Task not found"))?;

    let current_kind = current
        .trigger_kind
        .parse::<TaskTriggerKind>()
        .map_err(|e| bad_request_error(format!("invalid trigger_kind stored in task: {e}")))?;

    let normalized_run_at = req.run_at.as_deref().map(parse_utc_run_at).transpose()?;
    let next_cron = req
        .cron_expression
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| current.cron_expression.clone());
    let next_run_at = normalized_run_at.clone().or_else(|| current.run_at.clone());
    validate_trigger_fields(
        current_kind,
        &current.trigger_type,
        next_cron.as_deref(),
        next_run_at.as_deref(),
    )?;

    let next_script_path = req
        .script_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| current.script_path.clone());
    let next_script_inline = req
        .script_inline
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| current.script_inline.clone());
    validate_script_fields(next_script_path.as_deref(), next_script_inline.as_deref())?;

    let normalized_script_path_update = req
        .script_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let normalized_script_inline_update = req
        .script_inline
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let normalized_cron_update = match current_kind {
        TaskTriggerKind::Time => req
            .cron_expression
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Some),
        TaskTriggerKind::Event => {
            if req.cron_expression.is_some() || req.run_at.is_some() {
                return Err(bad_request_error(
                    "event trigger fields (run_at/cron_expression) cannot be updated",
                ));
            }
            None
        }
    };
    let normalized_run_at_update = match current_kind {
        TaskTriggerKind::Time => normalized_run_at.as_deref().map(Some),
        TaskTriggerKind::Event => None,
    };

    db.update_scheduled_task(
        &id,
        UpdateScheduledTaskParams {
            name: req.name.as_deref(),
            enabled: req.enabled,
            approved: None,
            cron_expression: normalized_cron_update,
            run_at: normalized_run_at_update,
            script_path: normalized_script_path_update.map(Some),
            script_inline: normalized_script_inline_update.map(Some),
        },
    )
    .await
    .map_err(internal_error)?;

    let model = db
        .get_scheduled_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Task not found"))?;

    Ok(Json(ScheduledTaskResponse::from_model(model)))
}

pub(super) async fn delete_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    db.delete_scheduled_task(&id)
        .await
        .map_err(internal_error)?;
    Ok(Json(super::types::API_OK))
}

pub(super) async fn approve_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    db.approve_scheduled_task(&id)
        .await
        .map_err(internal_error)?;
    Ok(Json(super::types::API_OK))
}

pub(super) async fn run_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::TaskTriggerResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    let task = db
        .get_scheduled_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Task not found"))?;

    let may_dir = state.may_dir.clone();
    let state_for_task = state.clone();
    let notification_manager = state.notification_manager.clone();

    // Execute inline
    tokio::spawn(async move {
        let mut scheduler =
            crate::scheduler::TaskScheduler::new(db, may_dir).with_notifier(notification_manager);
        if let Ok(manager) = state_for_task.resolve_container_manager(false).await {
            scheduler = scheduler.with_container_manager(manager);
        }
        scheduler.execute_task_by_model(&task).await;
    });

    Ok(Json(super::types::TaskTriggerResponse {
        ok: true,
        status: "triggered",
    }))
}
