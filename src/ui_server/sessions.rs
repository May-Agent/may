use std::str::FromStr;
use std::sync::Arc;

use async_fs;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::db::ProfileEnvironmentKind;
use crate::session::{
    MayHomePaths, SandboxResourceLayout, SessionId, SessionStore, list_summaries_from_db,
};
use aither_core::llm::{Message as LlmMessage, Role as LlmRole};

use super::error::{bad_request_error, internal_error};
use super::helpers::load_session_paths;
use super::message_mapping::map_db_messages;
use super::model_resolution::{
    persist_last_model, preferred_advanced_model, resolve_default_model,
};
use super::state::UiServerState;
use super::types::{
    ApiHealth, ApiSession, ContextDebugMessage, CreateSessionRequest, SessionContextDebugResponse,
    SessionContextWindowMetricsResponse, SessionContextWindowSnapshotResponse, SessionTasksPayload,
    UpdateSessionRequest,
};

fn session_not_found_error(session_id: &str) -> SkyError {
    let mut message = String::from("Session ");
    message.push_str(session_id);
    message.push_str(" not found");
    SkyError::msg(message).set_status(StatusCode::NOT_FOUND)
}

fn context_debug_not_found_error(session_id: &str) -> SkyError {
    let mut message = String::from("No context debug snapshot for session ");
    message.push_str(session_id);
    SkyError::msg(message).set_status(StatusCode::NOT_FOUND)
}

fn resolve_session_model(
    state: &UiServerState,
    session_id: &str,
    model: Option<String>,
) -> Result<String, skyzen::Error> {
    let config = state.config_snapshot();
    resolve_default_model(&config, model).ok_or_else(|| {
        let mut message = String::from("no model resolved for session ");
        message.push_str(session_id);
        internal_error(message)
    })
}

fn parse_json_field<T>(raw: &str, label: &str) -> Result<T, skyzen::Error>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(raw).map_err(|error| {
        let mut message = String::from("invalid ");
        message.push_str(label);
        message.push_str(": ");
        message.push_str(error.to_string().as_str());
        internal_error(message)
    })
}

pub(super) async fn list_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<ApiSession>>> {
    let db = state.db();
    let summaries = list_summaries_from_db(&db).await.map_err(internal_error)?;

    let config = state.config_snapshot();
    let mut sessions = Vec::with_capacity(summaries.len());
    for s in summaries {
        let model = resolve_default_model(&config, s.model.clone()).ok_or_else(|| {
            let mut message = String::from("no model resolved for session ");
            message.push_str(s.id.as_str());
            internal_error(message)
        })?;
        sessions.push(ApiSession::from_summary(s, model, Vec::new()));
    }

    Ok(Json(sessions))
}

pub(super) async fn get_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    let db_session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;

    let mut summary = crate::session::SessionSummary::from_db_model(db_session);
    summary.runtime = db
        .get_session_runtime(&id)
        .await
        .map_err(internal_error)?
        .unwrap_or_default();

    let model = resolve_session_model(&state, &id, summary.model.clone())?;

    let db_messages = db.get_messages(&id).await.map_err(internal_error)?;
    let messages = map_db_messages(&db_messages);

    Ok(Json(ApiSession::from_summary(summary, model, messages)))
}

pub(super) async fn get_session_context_debug(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionContextDebugResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();
    let row = db
        .get_context_debug_snapshot(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| context_debug_not_found_error(&id))?;

    let prompt_attachments: Vec<String> =
        parse_json_field(&row.prompt_attachments_json, "prompt_attachments_json")?;
    let assembled_messages: Vec<ContextDebugMessage> =
        parse_json_field(&row.assembled_messages_json, "assembled_messages_json")?;
    let window_snapshot = db
        .get_session_context_snapshot(&id)
        .await
        .map_err(internal_error)?
        .map(|snapshot| build_window_snapshot(snapshot))
        .transpose()?;

    Ok(Json(SessionContextDebugResponse {
        session_id: row.session_id,
        timestamp: row.timestamp,
        model: row.model,
        provider_id: row.provider_id,
        prompt: row.prompt,
        prompt_attachments,
        assembled_messages,
        window_snapshot,
    }))
}

pub(super) async fn get_session_tasks(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionTasksPayload>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let path = load_session_paths(&state, &id).await?.tasks_path();
    let runtime = state
        .runtime_service
        .get(&id)
        .await
        .map_err(internal_error)?;
    let content = match async_fs::read_to_string(&path).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(internal_error(error)),
    };
    Ok(Json(SessionTasksPayload {
        content,
        plan_state: runtime.plan_state,
    }))
}

pub(super) async fn save_session_tasks(
    params: Params,
    Json(payload): Json<SessionTasksPayload>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionTasksPayload>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let paths = load_session_paths(&state, &id).await?;
    async_fs::create_dir_all(paths.root())
        .await
        .map_err(internal_error)?;
    async_fs::write(paths.tasks_path(), payload.content)
        .await
        .map_err(internal_error)?;
    let runtime = state
        .runtime_service
        .set_plan_state(&id, payload.plan_state)
        .await
        .map_err(internal_error)?;
    Ok(Json(SessionTasksPayload {
        content: async_fs::read_to_string(paths.tasks_path())
            .await
            .map_err(internal_error)?,
        plan_state: runtime.plan_state,
    }))
}

fn build_window_snapshot(
    snapshot: crate::db::entity::session_context_snapshots::Model,
) -> Result<SessionContextWindowSnapshotResponse, skyzen::Error> {
    let metadata_json = snapshot
        .metadata_json
        .ok_or_else(|| internal_error("session context snapshot is missing metadata_json"))?;
    let stored: crate::session::StoredContextWindowMetadata =
        parse_json_field(&metadata_json, "session context metadata_json")?;
    let assembled_messages_json = snapshot.assembled_messages_json.ok_or_else(|| {
        internal_error("session context snapshot is missing assembled_messages_json")
    })?;
    let assembled_messages: Vec<LlmMessage> = parse_json_field(
        &assembled_messages_json,
        "session context assembled_messages_json",
    )?;

    Ok(SessionContextWindowSnapshotResponse {
        phase: stored.phase,
        metrics: SessionContextWindowMetricsResponse::from(stored.metrics),
        assembled_messages: assembled_messages
            .iter()
            .map(map_context_window_message)
            .collect(),
    })
}

fn map_context_window_message(message: &LlmMessage) -> ContextDebugMessage {
    ContextDebugMessage {
        role: map_llm_role(message.role()).to_string(),
        content: message.content().to_string(),
        attachments: message
            .attachments()
            .iter()
            .map(url::Url::to_string)
            .collect(),
        tool_call_id: message.tool_call_id().map(ToString::to_string),
        tool_call_names: message
            .tool_calls()
            .iter()
            .map(|call| call.name.clone())
            .collect(),
    }
}

fn map_llm_role(role: LlmRole) -> &'static str {
    match role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
        LlmRole::Tool => "tool",
    }
}

pub(super) async fn create_session(
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let requested_profile_id = req.profile_id.clone();
    let model = {
        let config = state.config_snapshot();
        req.model
            .or_else(|| config.last_model.clone())
            .or_else(|| preferred_advanced_model(&config))
            .ok_or_else(|| bad_request_error("No model configured"))?
    };

    let resolved_profile_id = requested_profile_id
        .clone()
        .or_else(|| Some(state.default_profile_id.clone()));
    let id = SessionId::new().to_string();
    let db = state.db();
    let layout = match resolved_profile_id.as_deref() {
        Some(profile_id) => {
            let profile = db
                .get_profile(profile_id)
                .await
                .map_err(internal_error)?
                .ok_or_else(|| bad_request_error("profile not found"))?;
            let environment = ProfileEnvironmentKind::from_str(profile.environment.as_str())
                .map_err(internal_error)?;
            if environment == ProfileEnvironmentKind::Container {
                SandboxResourceLayout::ContainerMountPoints
            } else {
                SandboxResourceLayout::Symlinked
            }
        }
        None => SandboxResourceLayout::Symlinked,
    };
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .create_session_dirs(&id, resolved_profile_id.as_deref(), layout)
        .await
        .map_err(internal_error)?;
    db.create_session_in_profile(
        &id,
        None,
        Some(model.as_str()),
        resolved_profile_id.as_deref(),
    )
    .await
    .map_err(internal_error)?;

    persist_last_model(&state, &model)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();

    Ok(Json(ApiSession::new_empty(id, model, resolved_profile_id)))
}

pub(super) async fn update_session(
    params: Params,
    Json(req): Json<UpdateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
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
    let next_title = requested_title.clone().or_else(|| session.title.clone());
    let next_model = requested_model.clone().or_else(|| session.model.clone());

    if let Some(model) = next_model.clone() {
        if session.model.as_deref() != Some(model.as_str()) {
            model_to_persist = Some(model);
        }
    }

    db.update_session(&id, requested_title.as_deref(), requested_model.as_deref())
        .await
        .map_err(internal_error)?;

    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    let mut history = store
        .read_history(&id, session.profile_id.as_deref())
        .await
        .map_err(internal_error)?;
    history.metadata.title = next_title.clone();
    history.metadata.model = next_model.clone();
    store
        .write_history(&id, session.profile_id.as_deref(), &history)
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
    state.notify_ui_events();

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
        reasoning_effort: None,
        runtime: runtime.clone().into(),
    }))
}

pub(super) async fn delete_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let profile_id = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .and_then(|session| session.profile_id);
    db.delete_session(&id).await.map_err(internal_error)?;
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .delete_session(&id, profile_id.as_deref())
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn delete_all_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Delete all from DB
    let db = state.db();
    db.delete_all_sessions().await.map_err(internal_error)?;

    // Clean up filesystem session directories
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .delete_all_session_dirs()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events();
    Ok(Json(ApiHealth { ok: true }))
}
