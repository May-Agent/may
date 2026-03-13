use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use crate::ask_user::QuestionAnswer;
use crate::session::history::{AskUserResponseData, PermissionResponseData};
use crate::session::{ChatMessage, SessionId};

use super::error::{bad_request_error, internal_error};
use super::helpers::{append_history_message, parse_ui_answer_value};
use super::state::{AskUserRespondRequest, UiServerState};
use super::types::{PermissionRespondRequest, SessionRuntimeResponse};

pub(super) async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let pending = state
        .db()
        .await
        .find_pending_permission_request(&req.id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("permission request not found or already resolved"))?;
    let session_id = pending.session_id;

    let responded = state.permission_approver.respond(&req.id, req.approved);

    let runtime = state
        .runtime_service
        .set_waiting_permission(&session_id, None)
        .await
        .map_err(internal_error)?;
    if !responded {
        tracing::warn!(request_id = %req.id, session_id = %session_id, "permission response arrived without a live waiter");
    }

    let data = PermissionResponseData {
        id: req.id.clone(),
        approved: req.approved,
    };
    let session_id = SessionId::from(session_id);
    let _ =
        append_history_message(&state, &session_id, ChatMessage::permission_response(&data)).await;
    if !responded {
        let resumed_state = state.clone();
        let resumed_session_id = session_id.as_str().to_string();
        tokio::spawn(async move {
            if let Err(error) = super::chat::resume_waiting_turn(
                resumed_state,
                resumed_session_id.clone(),
                "permission",
                if req.approved { "approved" } else { "denied" },
            )
            .await
            {
                tracing::error!(
                    session_id = %resumed_session_id,
                    error = %error,
                    "failed to resume waiting permission turn after restart"
                );
            }
        });
    }

    Ok(Json(runtime.into()))
}

pub(super) async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let pending = state
        .db()
        .await
        .find_pending_ask_user_request(&req.id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("ask_user request not found or already resolved"))?;
    let session_id = pending.session_id;

    let answers = req
        .answers
        .into_iter()
        .map(|answer| QuestionAnswer {
            section: answer.section,
            answer: parse_ui_answer_value(answer.answer),
        })
        .collect::<Vec<_>>();

    let responded = state.ask_user_approver.respond(&req.id, answers.clone());

    let runtime = state
        .runtime_service
        .set_waiting_ask_user(&session_id, None)
        .await
        .map_err(internal_error)?;
    if !responded {
        tracing::warn!(request_id = %req.id, session_id = %session_id, "ask-user response arrived without a live waiter");
    }

    let answers_json = serde_json::to_value(&answers).map_err(internal_error)?;
    let data = AskUserResponseData {
        id: req.id.clone(),
        answers: answers_json,
    };
    let session_id = SessionId::from(session_id);
    let _ =
        append_history_message(&state, &session_id, ChatMessage::ask_user_response(&data)).await;
    if !responded {
        let resumed_state = state.clone();
        let resumed_session_id = session_id.as_str().to_string();
        tokio::spawn(async move {
            if let Err(error) = super::chat::resume_waiting_turn(
                resumed_state,
                resumed_session_id.clone(),
                "ask_user",
                "answered",
            )
            .await
            {
                tracing::error!(
                    session_id = %resumed_session_id,
                    error = %error,
                    "failed to resume waiting ask-user turn after restart"
                );
            }
        });
    }

    Ok(Json(runtime.into()))
}
