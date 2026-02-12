use std::sync::Arc;

use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::session::SessionId;

use super::helpers::{
    bad_request_error, internal_error, map_db_messages, map_history_messages,
    persist_last_model, preferred_advanced_model, resolve_default_model, truncate_preview,
};
use super::state::UiServerState;
use super::types::{
    ApiHealth, ApiSession, CreateSessionRequest, UpdateSessionRequest,
};

pub(super) async fn list_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<ApiSession>>> {
    // Grab DB handle and session summaries, then release the manager lock
    let (summaries, db) = {
        let manager = state.session_manager.lock().await;
        let summaries = manager.list_sessions().await.map_err(internal_error)?;
        let db = manager.db().clone();
        (summaries, db)
    };

    let config = state.config.read().await;
    let mut sessions = Vec::with_capacity(summaries.len());

    for s in summaries {
        let session_id = s.id.as_str().to_string();
        let model = resolve_default_model(&config, s.model).unwrap_or_default();

        // Load messages from DB for this session
        let db_messages = db.get_messages(&session_id).await.map_err(internal_error)?;
        let messages = map_db_messages(&db_messages);

        let last_modified = s
            .updated_at
            .map(|t| t.to_string())
            .unwrap_or_else(|| jiff::Timestamp::now().to_string());

        let preview = if s.content_preview.is_empty() {
            "New conversation started".to_string()
        } else {
            s.content_preview
        };

        sessions.push(ApiSession {
            id: session_id,
            title: s.title.unwrap_or_else(|| "New Chat".to_string()),
            last_modified,
            preview,
            messages,
            model,
            reasoning_effort: None,
        });
    }

    Ok(Json(sessions))
}

pub(super) async fn create_session(
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let model = {
        let config = state.config.read().await;
        req.model
            .or_else(|| config.last_model.clone())
            .or_else(|| preferred_advanced_model(&config))
            .ok_or_else(|| bad_request_error("No model configured"))?
    };

    let mut manager = state.session_manager.lock().await;

    let session = manager
        .create_session_with_model(Some(model.clone()))
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

    Ok(Json(ApiSession {
        id,
        title: "New Chat".to_string(),
        last_modified: jiff::Timestamp::now().to_string(),
        preview: "New conversation started".to_string(),
        messages: Vec::new(),
        model,
        reasoning_effort: None,
    }))
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
        persist_last_model(&state, &model)
            .await
            .map_err(internal_error)?;
    }

    let current = manager.current().ok_or_else(|| {
        SkyError::msg("No active session").set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    Ok(Json(ApiSession {
        id,
        title: current
            .title
            .clone()
            .unwrap_or_else(|| "New Chat".to_string()),
        last_modified: jiff::Timestamp::now().to_string(),
        preview: current
            .history
            .messages()
            .last()
            .map(|m| truncate_preview(&m.content))
            .unwrap_or_else(|| "New conversation started".to_string()),
        messages: map_history_messages(current.history.messages()),
        model: {
            let cfg = state.config.read().await;
            resolve_default_model(&cfg, current.model.clone()).unwrap_or_default()
        },
        reasoning_effort: None,
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
    let ids = store.list_sessions().await.unwrap_or_default();
    for id in ids {
        let _ = store.delete_session(&id).await;
    }

    let _ = manager.create_session();
    Ok(Json(ApiHealth { ok: true }))
}
