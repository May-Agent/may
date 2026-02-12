use std::sync::Arc;
use std::time::Duration;

use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::utils::{Json, State};

use crate::ask_user::QuestionAnswer;
use crate::session::history::{
    AskUserRequestData, AskUserResponseData, PermissionRequestData, PermissionResponseData,
};
use crate::session::{ChatMessage, SessionId};

use super::helpers::{append_history_message, bad_request_error, parse_ui_answer_value};
use super::state::{AskUserPollResponse, AskUserRespondRequest, UiServerState};
use super::types::{
    ApiHealth, PermissionPollQuery, PermissionPollResponse, PermissionRespondRequest,
};

pub(super) async fn poll_permission_request(
    Query(query): Query<PermissionPollQuery>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<PermissionPollResponse>> {
    let session_id = query.session_id.as_str();
    if session_id.is_empty() {
        return Err(bad_request_error("session_id is required"));
    }

    let request = match tokio::time::timeout(
        Duration::from_secs(20),
        state
            .permission_approver
            .next_request_for_session(Some(session_id)),
    )
    .await
    {
        Ok(req) => req,
        Err(_) => None,
    };

    let (request, state_label) = if let Some(req) = request {
        // Persist the permission request to chat history
        if let Some(sid) = &req.session_id {
            let data = PermissionRequestData {
                id: req.id.clone(),
                mode: format!("{:?}", req.mode).to_lowercase(),
                script: req.script.clone(),
            };
            let session_id = SessionId::from_string(sid.clone());
            let _ =
                append_history_message(&state, &session_id, ChatMessage::permission_request(&data))
                    .await;
        }
        (Some(req), Some("waiting_permission"))
    } else if state
        .permission_approver
        .snapshot_session_runtime(Some(session_id))
        .is_some()
    {
        (
            state
                .permission_approver
                .snapshot_session_runtime(Some(session_id)),
            Some("waiting_permission"),
        )
    } else {
        (None, None)
    };

    Ok(Json(PermissionPollResponse {
        request,
        state: state_label,
    }))
}

pub(super) async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Get session_id from the pending request before responding (which removes it)
    let session_id = state
        .permission_approver
        .snapshot_session_runtime(None)
        .and_then(|r| {
            if r.id == req.id {
                r.session_id.clone()
            } else {
                None
            }
        });

    let responded = state.permission_approver.respond(&req.id, req.approved);
    if !responded {
        return Err(bad_request_error(
            "permission request not found or already resolved",
        ));
    }

    // Persist the permission response to chat history
    if let Some(sid) = session_id {
        let data = PermissionResponseData {
            id: req.id.clone(),
            approved: req.approved,
        };
        let session_id = SessionId::from(sid);
        let _ =
            append_history_message(&state, &session_id, ChatMessage::permission_response(&data))
                .await;
    }

    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn poll_ask_user_request(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<AskUserPollResponse>> {
    let request = match tokio::time::timeout(
        Duration::from_secs(20),
        state.ask_user_approver.next_request(),
    )
    .await
    {
        Ok(req) => req,
        Err(_) => None,
    };

    // Persist the ask-user request to chat history
    if let Some(ref payload) = request
        && let Some(sid) = &payload.session_id
    {
        let data = AskUserRequestData {
            id: payload.id.clone(),
            questions: serde_json::to_value(&payload.questions).unwrap_or_default(),
        };
        let session_id = SessionId::from_string(sid.clone());
        let _ =
            append_history_message(&state, &session_id, ChatMessage::ask_user_request(&data)).await;
    }

    Ok(Json(AskUserPollResponse { request }))
}

pub(super) async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Capture session_id from the pending request before responding
    let session_id = state.ask_user_approver.get_session_id(&req.id).await;

    let answers = req
        .answers
        .into_iter()
        .map(|answer| QuestionAnswer {
            section: answer.section,
            answer: parse_ui_answer_value(answer.answer),
        })
        .collect::<Vec<_>>();

    let responded = state
        .ask_user_approver
        .respond(&req.id, answers.clone())
        .await;
    if !responded {
        return Err(bad_request_error(
            "ask_user request not found or already resolved",
        ));
    }

    // Persist the ask-user response to chat history
    if let Some(sid) = session_id {
        let data = AskUserResponseData {
            id: req.id.clone(),
            answers: serde_json::to_value(&answers).unwrap_or_default(),
        };
        let session_id = SessionId::from(sid);
        let _ = append_history_message(&state, &session_id, ChatMessage::ask_user_response(&data))
            .await;
    }

    Ok(Json(ApiHealth { ok: true }))
}
