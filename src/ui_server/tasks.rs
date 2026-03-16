//! Scheduled task REST API.

use std::collections::BTreeMap;
use std::mem;
use std::sync::Arc;

use base64::Engine;
use skyzen::extract::PeerAddr;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Body, Request, Result as SkyResult, StatusCode};

use crate::db::{CreateScheduledTaskParams, UpdateScheduledTaskParams};
use crate::task_trigger::{
    NormalizedTaskTrigger, TRIGGER_KIND_WEBHOOK, TaskTriggerInput, TaskTriggerKind,
    TaskWebhookRequest, normalize_trigger, webhook_path,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ScheduledTaskResponse {
    fn from_model(m: crate::db::entity::scheduled_tasks::Model) -> Self {
        let webhook_path = if m.trigger_kind == TRIGGER_KIND_WEBHOOK {
            Some(webhook_path(m.trigger_type.as_str()))
        } else {
            None
        };
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
            webhook_path,
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
    #[serde(default)]
    pub trigger_type: Option<String>,
    #[serde(default)]
    pub event_name: Option<String>,
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

#[derive(Debug, Clone)]
pub(super) struct WebhookInvocationRequest(TaskWebhookRequest);

#[skyzen::error(status = StatusCode::BAD_REQUEST)]
pub(super) enum WebhookInvocationRequestError {
    #[error("failed to read webhook request body")]
    InvalidBody,
    #[error("webhook request contains a non-UTF8 header value")]
    InvalidHeaderValue,
}

impl skyzen::extract::Extractor for WebhookInvocationRequest {
    type Error = WebhookInvocationRequestError;

    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let method = request.method().as_str().to_string();
        let path = request.uri().path().to_string();
        let query = request.uri().query().map(ToOwned::to_owned);
        let remote_addr = request
            .extensions()
            .get::<PeerAddr>()
            .map(|addr| addr.to_string());

        let mut headers = BTreeMap::new();
        for (name, value) in request.headers() {
            let value = value
                .to_str()
                .map_err(|_| WebhookInvocationRequestError::InvalidHeaderValue)?
                .to_string();
            headers
                .entry(name.as_str().to_string())
                .or_insert_with(Vec::new)
                .push(value);
        }

        let body = mem::replace(request.body_mut(), Body::empty());
        let body = body
            .into_bytes()
            .await
            .map_err(|_| WebhookInvocationRequestError::InvalidBody)?;
        let body_size_bytes = body.len();
        let body_text = std::str::from_utf8(body.as_ref())
            .ok()
            .map(ToOwned::to_owned);
        let body_json = headers
            .get(skyzen::header::CONTENT_TYPE.as_str())
            .and_then(|values| values.first())
            .filter(|value| value.starts_with("application/json"))
            .and_then(|_| serde_json::from_slice(body.as_ref()).ok());

        Ok(Self(TaskWebhookRequest {
            method,
            path,
            query,
            headers,
            body_base64: base64::engine::general_purpose::STANDARD.encode(body.as_ref()),
            body_text,
            body_json,
            remote_addr,
            body_size_bytes,
        }))
    }
}

fn normalize_create_trigger(request: &CreateTaskRequest) -> SkyResult<NormalizedTaskTrigger> {
    normalize_trigger(TaskTriggerInput {
        kind: &request.trigger_kind,
        trigger_type: request.trigger_type.as_deref(),
        run_at_utc: request.run_at.as_deref(),
        cron_utc: request.cron_expression.as_deref(),
        event_name: request.event_name.as_deref(),
    })
    .map_err(bad_request_error)
}

fn validate_existing_trigger(
    current: &crate::db::entity::scheduled_tasks::Model,
    next_cron: Option<&str>,
    next_run_at: Option<&str>,
) -> SkyResult<TaskTriggerKind> {
    let current_kind = current
        .trigger_kind
        .parse::<TaskTriggerKind>()
        .map_err(bad_request_error)?;
    normalize_trigger(TaskTriggerInput {
        kind: current_kind.as_str(),
        trigger_type: if current_kind == TaskTriggerKind::Time {
            Some(current.trigger_type.as_str())
        } else {
            None
        },
        run_at_utc: next_run_at,
        cron_utc: next_cron,
        event_name: if current_kind == TaskTriggerKind::Event {
            Some(current.trigger_type.as_str())
        } else {
            None
        },
    })
    .map_err(bad_request_error)?;
    Ok(current_kind)
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
    let tasks = state
        .db()
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
    let id = uuid::Uuid::new_v4().to_string();
    let trigger = normalize_create_trigger(&req)?;
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
    validate_script_fields(normalized_script_path, normalized_script_inline)?;
    state
        .task_runtime
        .validate_task_script_for_profile(
            &req.profile_id,
            normalized_script_path,
            normalized_script_inline,
        )
        .await
        .map_err(bad_request_error)?;

    let trigger_type = trigger.storage_trigger_type();
    let model = state
        .db()
        .create_scheduled_task(CreateScheduledTaskParams {
            id: &id,
            profile_id: &req.profile_id,
            session_id: req.session_id.as_deref(),
            name: &req.name,
            trigger_kind: trigger.kind_str(),
            trigger_type: &trigger_type,
            cron_expression: trigger.cron_expression(),
            run_at: trigger.run_at(),
            script_path: normalized_script_path,
            script_inline: normalized_script_inline,
            needs_network: req.needs_network,
        })
        .await
        .map_err(internal_error)?;
    state
        .task_runtime
        .reload_active_tasks()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();

    Ok(Json(ScheduledTaskResponse::from_model(model)))
}

pub(super) async fn get_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ScheduledTaskResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let model = state
        .db()
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

    let db = state.db();
    let current = db
        .get_scheduled_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Task not found"))?;

    let normalized_run_at = req
        .run_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let next_cron = req
        .cron_expression
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| current.cron_expression.clone());
    let next_run_at = normalized_run_at.clone().or_else(|| current.run_at.clone());
    let current_kind =
        validate_existing_trigger(&current, next_cron.as_deref(), next_run_at.as_deref())?;

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
    state
        .task_runtime
        .validate_task_script_for_profile(
            &current.profile_id,
            next_script_path.as_deref(),
            next_script_inline.as_deref(),
        )
        .await
        .map_err(bad_request_error)?;

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
    let normalized_cron_update = if current_kind == TaskTriggerKind::Time {
        req.cron_expression
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Some)
    } else {
        None
    };
    let normalized_run_at_update = match current_kind {
        TaskTriggerKind::Time => normalized_run_at.as_deref().map(Some),
        TaskTriggerKind::Event => None,
        TaskTriggerKind::Webhook => None,
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
    state
        .task_runtime
        .reload_active_tasks()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();

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

    state
        .db()
        .delete_scheduled_task(&id)
        .await
        .map_err(internal_error)?;
    state
        .task_runtime
        .reload_active_tasks()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();
    Ok(Json(super::types::API_OK))
}

pub(super) async fn approve_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    state
        .db()
        .approve_scheduled_task(&id)
        .await
        .map_err(internal_error)?;
    state
        .task_runtime
        .reload_active_tasks()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();
    Ok(Json(super::types::API_OK))
}

pub(super) async fn run_task(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::TaskTriggerResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let task = state
        .db()
        .get_scheduled_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Task not found"))?;

    state
        .task_runtime
        .run_manual_task(task)
        .await
        .map_err(internal_error)?;

    Ok(Json(super::types::TaskTriggerResponse {
        ok: true,
        status: "triggered",
        triggered: 1,
    }))
}

pub(super) async fn invoke_webhook_task(
    params: Params,
    webhook_request: WebhookInvocationRequest,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::TaskTriggerResponse>> {
    let token = params.get("token").map_err(bad_request_error)?.to_string();
    let triggered = state
        .task_runtime
        .dispatch_webhook(token, webhook_request.0)
        .await
        .map_err(internal_error)?;
    if triggered == 0 {
        return Err(bad_request_error("Webhook task not found"));
    }

    Ok(Json(super::types::TaskTriggerResponse {
        ok: true,
        status: "triggered",
        triggered,
    }))
}
