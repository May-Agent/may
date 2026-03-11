use std::sync::Arc;

use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::session::SessionId;

use super::error::{bad_request_error, internal_error};
use super::helpers::truncate_preview;
use super::message_mapping::map_db_messages;
use super::model_resolution::{
    persist_last_model, preferred_advanced_model, resolve_default_model,
};
use super::state::UiServerState;
use super::types::{
    ApiHealth, ApiSession, ContextDebugMessage, CreateSessionRequest, SessionContextDebugResponse,
    UpdateSessionRequest,
};

pub(super) async fn list_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<ApiSession>>> {
    let summaries = {
        let manager = state.session_manager.lock().await;
        manager.list_sessions().await.map_err(internal_error)?
    };

    let config = state.config.read().await;
    let mut sessions = Vec::with_capacity(summaries.len());
    for s in summaries {
        let model = resolve_default_model(&config, s.model.clone())
            .ok_or_else(|| internal_error(format!("no model resolved for session {}", s.id)))?;
        sessions.push(ApiSession::from_summary(s, model, Vec::new()));
    }

    Ok(Json(sessions))
}

pub(super) async fn get_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };

    let db_session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            SkyError::msg(format!("Session {id} not found")).set_status(StatusCode::NOT_FOUND)
        })?;

    let summary = crate::session::SessionSummary::from_db_model(db_session);

    let config = state.config.read().await;
    let model = resolve_default_model(&config, summary.model.clone())
        .ok_or_else(|| internal_error(format!("no model resolved for session {id}")))?;

    let db_messages = db.get_messages(&id).await.map_err(internal_error)?;
    let messages = map_db_messages(&db_messages);

    Ok(Json(ApiSession::from_summary(summary, model, messages)))
}

pub(super) async fn get_session_context_debug(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionContextDebugResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };
    let row = db
        .get_context_debug_snapshot(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            SkyError::msg(format!("No context debug snapshot for session {id}"))
                .set_status(StatusCode::NOT_FOUND)
        })?;

    let prompt_attachments: Vec<String> = serde_json::from_str(&row.prompt_attachments_json)
        .map_err(|e| internal_error(format!("invalid prompt_attachments_json: {e}")))?;
    let assembled_messages: Vec<ContextDebugMessage> =
        serde_json::from_str(&row.assembled_messages_json)
            .map_err(|e| internal_error(format!("invalid assembled_messages_json: {e}")))?;

    Ok(Json(SessionContextDebugResponse {
        session_id: row.session_id,
        timestamp: row.timestamp,
        model: row.model,
        provider_id: row.provider_id,
        prompt: row.prompt,
        prompt_attachments,
        assembled_messages,
    }))
}

pub(super) async fn create_session(
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let requested_profile_id = req.profile_id.clone();
    let model = {
        let config = state.config.read().await;
        req.model
            .or_else(|| config.last_model.clone())
            .or_else(|| preferred_advanced_model(&config))
            .ok_or_else(|| bad_request_error("No model configured"))?
    };

    let mut manager = state.session_manager.lock().await;
    let resolved_profile_id =
        requested_profile_id.or_else(|| manager.current_profile_id().map(ToOwned::to_owned));

    let session = manager
        .create_session_in_profile(Some(model.clone()), resolved_profile_id)
        .map_err(internal_error)?;
    let id = session.id.as_str().to_string();

    manager.ensure_persisted().await.map_err(internal_error)?;
    manager
        .set_current_model(model.clone())
        .await
        .map_err(internal_error)?;
    drop(manager);

    persist_last_model(&state, &model)
        .await
        .map_err(internal_error)?;

    Ok(Json(ApiSession::new_empty(id, model)))
}

pub(super) async fn update_session(
    params: Params,
    Json(req): Json<UpdateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let session_id = SessionId::from_string(id.clone());

    let mut manager = state.session_manager.lock().await;
    manager
        .load_session(&session_id)
        .await
        .map_err(internal_error)?;

    let mut model_to_persist: Option<String> = None;

    if let Some(title) = req.title {
        manager
            .set_current_title(title)
            .await
            .map_err(internal_error)?;
    }
    if let Some(model) = req.model {
        manager
            .set_current_model(model.clone())
            .await
            .map_err(internal_error)?;
        model_to_persist = Some(model);
    }

    let current = manager.current().ok_or_else(|| {
        SkyError::msg("No active session").set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    let title = current
        .title
        .clone()
        .unwrap_or_else(|| "New Chat".to_string());
    let preview = current
        .history
        .messages()
        .last()
        .map(|m| truncate_preview(&m.content))
        .unwrap_or_else(|| "New conversation started".to_string());
    let current_model = current.model.clone();
    let current_id = current.id.as_str().to_string();
    drop(manager);

    if let Some(model) = model_to_persist {
        persist_last_model(&state, &model)
            .await
            .map_err(internal_error)?;
    }

    let model = {
        let cfg = state.config.read().await;
        resolve_default_model(&cfg, current_model)
            .ok_or_else(|| internal_error(format!("no model resolved for session {current_id}")))?
    };

    Ok(Json(ApiSession {
        id,
        title,
        last_modified: jiff::Timestamp::now().to_string(),
        preview,
        messages: Vec::new(),
        model,
        reasoning_effort: None,
        runtime_state: crate::session::SessionRuntimeState::Idle,
        queued_count: 0,
    }))
}

pub(super) async fn delete_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let mut manager = state.session_manager.lock().await;
    manager.delete_session(&id).await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn delete_all_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let mut manager = state.session_manager.lock().await;

    // Delete all from DB
    let db = manager.db().clone();
    db.delete_all_sessions().await.map_err(internal_error)?;

    // Clean up filesystem session directories
    let store = manager.store().clone();
    let ids = store.list_sessions().await.map_err(internal_error)?;
    for id in ids {
        store.delete_session(&id).await.map_err(internal_error)?;
    }

    let _ = manager.create_session();
    Ok(Json(ApiHealth { ok: true }))
}
