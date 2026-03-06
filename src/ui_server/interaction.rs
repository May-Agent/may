use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use crate::ask_user::QuestionAnswer;
use crate::session::history::{AskUserResponseData, PermissionResponseData};
use crate::session::{ChatMessage, SessionId};

use super::error::{bad_request_error, internal_error};
use super::helpers::{append_history_message, parse_ui_answer_value};
use super::state::{AskUserRespondRequest, UiServerState};
use super::types::{ApiHealth, PermissionRespondRequest};

pub(super) async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Get session_id from the target pending request before responding.
    let session_id = state.permission_approver.session_id_for_request(&req.id);

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

pub(super) async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Capture session_id from the pending request before responding
    let session_id = state.ask_user_approver.get_session_id(&req.id);

    let answers = req
        .answers
        .into_iter()
        .map(|answer| QuestionAnswer {
            section: answer.section,
            answer: parse_ui_answer_value(answer.answer),
        })
        .collect::<Vec<_>>();

    let responded = state.ask_user_approver.respond(&req.id, answers.clone());
    if !responded {
        return Err(bad_request_error(
            "ask_user request not found or already resolved",
        ));
    }

    // Persist the ask-user response to chat history
    if let Some(sid) = session_id {
        let answers_json = serde_json::to_value(&answers).map_err(internal_error)?;
        let data = AskUserResponseData {
            id: req.id.clone(),
            answers: answers_json,
        };
        let session_id = SessionId::from(sid);
        let _ = append_history_message(&state, &session_id, ChatMessage::ask_user_response(&data))
            .await;
    }

    Ok(Json(ApiHealth { ok: true }))
}
