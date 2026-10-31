use std::str::FromStr;
use std::sync::Arc;

use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::db::ProfileEnvironmentKind;
use crate::session::{
    MayHomePaths, SandboxResourceLayout, SessionId, SessionStore, list_summaries_from_db,
};
use crate::ui_event_bus::UiEventTopics;
use aither_core::llm::{Message as LlmMessage, Role as LlmRole};

use super::error::{bad_request_error, internal_error, internal_error_message};
use super::message_mapping::map_db_messages;
use super::model_resolution::{
    persist_last_model, preferred_advanced_model, resolve_default_model,
};
use super::state::DaemonState;
use super::types::{
    ApiHealth, ApiSession, ContextDebugMessage, CreateSessionRequest, NullableStringPatch,
    SessionContextDebugResponse, SessionContextWindowMetricsResponse,
    SessionContextWindowSnapshotResponse, SessionPlanStateRequest, SessionPlanStateResponse,
    SessionTasksPayload, UpdateSessionRequest,
};

fn session_not_found_error(session_id: &str) -> SkyError {
    SkyError::msg(format!("Session {session_id} not found")).set_status(StatusCode::NOT_FOUND)
}

fn context_debug_not_found_error(session_id: &str) -> SkyError {
    SkyError::msg(format!(
        "No context debug snapshot for session {session_id}"
    ))
    .set_status(StatusCode::NOT_FOUND)
}

fn resolve_session_model(
    state: &DaemonState,
    session_id: &str,
    model: Option<String>,
) -> Result<String, skyzen::Error> {
    let config = state.config_snapshot();
    resolve_default_model(&config, model).ok_or_else(|| {
        internal_error_message(format!("no model resolved for session {session_id}"))
    })
}

pub(super) async fn list_sessions(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<ApiSession>>> {
    Ok(Json(list_api_sessions(state.as_ref()).await?))
}

pub(super) async fn list_api_sessions(
    state: &DaemonState,
) -> Result<Vec<ApiSession>, skyzen::Error> {
    let db = state.db();
    let summaries = list_summaries_from_db(&db).await.map_err(internal_error)?;

    let config = state.config_snapshot();
    let mut sessions = Vec::with_capacity(summaries.len());
    for s in summaries {
        let model = resolve_default_model(&config, s.model.clone()).ok_or_else(|| {
            internal_error_message(format!("no model resolved for session {}", s.id.as_str()))
        })?;
        sessions.push(ApiSession::from_summary(s, model, Vec::new()));
    }

    Ok(sessions)
}

pub(super) async fn get_session(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    let db_session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;

    let mut summary = crate::session::SessionSummary::from_db_model(db_session);

    // Fetch runtime and messages in parallel — they are independent queries
    let (runtime_result, messages_result) =
        tokio::join!(db.get_session_runtime(&id), db.get_messages(&id),);
    summary.runtime = runtime_result.map_err(internal_error)?.unwrap_or_default();
    let model = resolve_session_model(&state, &id, summary.model.clone())?;
    let db_messages = messages_result.map_err(internal_error)?;
    let messages = map_db_messages(&db_messages).map_err(internal_error)?;

    Ok(Json(ApiSession::from_summary(summary, model, messages)))
}

pub(super) async fn get_session_context_debug(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionContextDebugResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();
    let row = db
        .get_context_debug_snapshot(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| context_debug_not_found_error(&id))?;
    let window_snapshot = db
        .get_session_context_snapshot(&id)
        .await
        .map_err(internal_error)?
        .map(build_window_snapshot);

    Ok(Json(SessionContextDebugResponse {
        session_id: row.session_id,
        timestamp: row.timestamp,
        model: row.model,
        provider_id: row.provider_id,
        prompt: row.prompt,
        prompt_attachments: row.prompt_attachments,
        assembled_messages: row
            .assembled_messages
            .into_iter()
            .map(|message| ContextDebugMessage {
                role: message.role,
                content: message.content,
                attachments: message.attachments,
                tool_call_id: message.tool_call_id,
                tool_call_names: message.tool_call_names,
            })
            .collect(),
        window_snapshot,
    }))
}

pub(super) async fn get_session_tasks(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionTasksPayload>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let paths = super::helpers::load_session_paths(&state, &id).await?;
    let runtime = state
        .runtime_service
        .get(&id)
        .await
        .map_err(internal_error)?;
    let content = paths
        .tasks_content()
        .await
        .map_err(internal_error)?
        .unwrap_or_default();
    Ok(Json(SessionTasksPayload {
        content,
        plan_state: runtime.plan_state,
    }))
}

/// Atomically set the plan state for a session without touching task content.
///
/// This is what the UI uses to toggle plan mode (enter/approve/reopen); it
/// avoids the fetch-then-save race of the generic `save_session_tasks` path.
pub(super) async fn set_session_plan_state(
    params: Params,
    Json(payload): Json<SessionPlanStateRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionPlanStateResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let runtime = state
        .runtime_service
        .set_plan_state(&id, payload.plan_state)
        .await
        .map_err(internal_error)?;
    Ok(Json(SessionPlanStateResponse {
        plan_state: runtime.plan_state,
    }))
}

pub(super) async fn save_session_tasks(
    params: Params,
    Json(payload): Json<SessionTasksPayload>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionTasksPayload>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let paths = super::helpers::load_session_paths(&state, &id).await?;
    async_fs::write(paths.tasks_path(), &payload.content)
        .await
        .map_err(internal_error)?;
    let runtime = state
        .runtime_service
        .set_plan_state(&id, payload.plan_state)
        .await
        .map_err(internal_error)?;
    Ok(Json(SessionTasksPayload {
        content: payload.content,
        plan_state: runtime.plan_state,
    }))
}

fn build_window_snapshot(
    snapshot: crate::db::StoredSessionContextSnapshot,
) -> SessionContextWindowSnapshotResponse {
    let window = snapshot.checkpoint.context_window;

    SessionContextWindowSnapshotResponse {
        phase: window.phase,
        metrics: SessionContextWindowMetricsResponse::from(window.metrics),
        assembled_messages: window
            .messages
            .iter()
            .map(map_context_window_message)
            .collect(),
    }
}

fn map_context_window_message(message: &LlmMessage) -> ContextDebugMessage {
    ContextDebugMessage {
        role: map_llm_role(message.role()).to_string(),
        content: message.content().to_string(),
        attachments: message
            .attachments()
            .iter()
            .map(|attachment| attachment.url().to_string())
            .collect(),
        tool_call_id: message.tool_call_id().map(ToString::to_string),
        tool_call_names: message
            .tool_calls()
            .iter()
            .map(|call| call.name.clone())
            .collect(),
    }
}

const fn map_llm_role(role: LlmRole) -> &'static str {
    match role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
        LlmRole::Tool => "tool",
    }
}

/// Creates the durable DB row and session directory layout for a profile.
///
/// Shared by the regular create-session endpoint and notification callbacks,
/// which open a fresh conversation in the originating profile.
pub(super) async fn create_session_for_profile(
    state: &Arc<DaemonState>,
    profile_id: Option<&str>,
    model: &str,
) -> SkyResult<String> {
    let id = SessionId::new().to_string();
    let db = state.db();
    let layout = match profile_id {
        Some(profile_id) => {
            let profile = db
                .get_profile(profile_id)
                .await
                .map_err(internal_error)?
                .ok_or_else(|| bad_request_error("profile not found"))?;
            let environment = ProfileEnvironmentKind::from_str(profile.environment.as_str())
                .map_err(internal_error)?;
            if environment == ProfileEnvironmentKind::Container {
                crate::container::profile::ensure_container_runtime_assets(&state.may_dir)
                    .await
                    .map_err(internal_error)?;
                SandboxResourceLayout::ContainerMountPoints
            } else {
                SandboxResourceLayout::Symlinked
            }
        }
        None => SandboxResourceLayout::Symlinked,
    };
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .create_session_dirs(&id, profile_id, layout)
        .await
        .map_err(internal_error)?;
    db.create_session_in_profile(&id, None, Some(model), profile_id)
        .await
        .map_err(internal_error)?;
    super::chat::open_session_for_agents(state, &id, profile_id)
        .await
        .map_err(internal_error)?;
    Ok(id)
}

pub(super) async fn create_session(
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let requested_profile_id = req.profile_id.clone();
    let model = {
        let config = state.config_snapshot();
        req.model
            .or_else(|| config.last_model.as_ref().map(ToString::to_string))
            .or_else(|| preferred_advanced_model(&config))
            .ok_or_else(|| bad_request_error("No model configured"))?
    };

    let resolved_profile_id = requested_profile_id
        .clone()
        .or_else(|| Some(state.default_profile_id.clone()));
    let id = create_session_for_profile(&state, resolved_profile_id.as_deref(), &model).await?;
    if let Some(cwd) = req
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    {
        state
            .db()
            .set_session_cwd(&id, cwd)
            .await
            .map_err(internal_error)?;
        if let Some(worktree) = crate::repo_search::worktree_root(std::path::Path::new(cwd)) {
            let _ = state.db().touch_repo_usage(&worktree).await;
        }
    }

    persist_last_model(&state, &model)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);

    Ok(Json(ApiSession::new_empty(id, model, resolved_profile_id)))
}

pub(super) async fn update_session(
    params: Params,
    Json(req): Json<UpdateSessionRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;

    let mut model_to_persist: Option<String> = None;
    let requested_title = req.title.clone();
    let requested_model = req.model.clone();
    let requested_effort_patch = req.reasoning_effort.clone();
    let next_title = requested_title.clone().or_else(|| session.title.clone());
    let next_model = requested_model.clone().or_else(|| session.model.clone());
    let next_effort = match requested_effort_patch.as_ref() {
        Some(NullableStringPatch::Null) => None,
        Some(NullableStringPatch::Set(value)) => Some(value.clone()),
        None => session.reasoning_effort.clone(),
    };

    if let Some(model) = next_model.clone()
        && session.model.as_deref() != Some(model.as_str())
    {
        model_to_persist = Some(model);
    }

    let effort_patch_ref = requested_effort_patch.as_ref().map(|inner| match inner {
        NullableStringPatch::Null => None,
        NullableStringPatch::Set(value) => Some(value.as_str()),
    });

    db.update_session(
        &id,
        requested_title.as_deref(),
        requested_model.as_deref(),
        effort_patch_ref,
    )
    .await
    .map_err(internal_error)?;

    if let Some(model) = model_to_persist {
        persist_last_model(&state, &model)
            .await
            .map_err(internal_error)?;
    }

    let model = resolve_session_model(&state, session.id.as_str(), next_model.clone())?;
    let runtime = state
        .runtime_service
        .get(&id)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);

    Ok(Json(ApiSession {
        id,
        title: next_title.unwrap_or_else(|| "New Chat".to_string()),
        last_modified: jiff::Timestamp::now().to_string(),
        preview: session
            .preview
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "New conversation started".to_string()),
        messages: Vec::new(),
        profile_id: session.profile_id,
        model,
        reasoning_effort: next_effort,
        runtime: runtime.into(),
    }))
}

pub(super) async fn delete_session(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let profile_id = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .and_then(|session| session.profile_id);
    db.delete_session(&id).await.map_err(internal_error)?;
    state.parked_agents.evict(&id);
    state.agent_listeners.remove(&id);
    #[cfg(target_os = "macos")]
    state.session_displays.lock().await.release(&id);
    state
        .tool_environments
        .evict(&crate::session::SessionId::from_string(id.as_str()));
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .delete_session(&id, profile_id.as_deref())
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn delete_all_sessions(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Delete all from DB
    let db = state.db();
    db.delete_all_sessions().await.map_err(internal_error)?;
    state.tool_environments.evict_all();
    state.agent_listeners.clear();
    #[cfg(target_os = "macos")]
    state.session_displays.lock().await.clear();

    // Clean up filesystem session directories
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .delete_all_session_dirs()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);
    Ok(Json(ApiHealth { ok: true }))
}
