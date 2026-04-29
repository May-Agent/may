use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use crate::ask_user::QuestionAnswer;
use crate::session::SessionId;

use super::error::{bad_request_error, internal_error};
use super::state::{AskUserRespondRequest, UiServerState};
use super::types::{PermissionRespondRequest, SessionRuntimeResponse, WorkspaceRespondRequest};

pub(super) async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let pending = state
        .db()
        .find_pending_permission_request(&req.id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("permission request not found or already resolved"))?;
    let session_id = pending.session_id;

    // Delete the specific pending row BEFORE waking the agent. If we deleted
    // session-wide afterwards, the agent — resumed by `respond` below — could
    // emit and persist a brand-new permission request before our DELETE ran,
    // wiping the new row and leaving the next Approve click with a 400.
    let mut runtime = state
        .runtime_service
        .clear_waiting_permission_request(&session_id, &req.id)
        .await
        .map_err(internal_error)?;

    let responded = state.permission_approver.respond(&req.id, req.approved);
    if !responded {
        tracing::warn!(request_id = %req.id, session_id = %session_id, "permission response arrived without a live waiter");
    }

    let session_id = SessionId::from(session_id);
    let _ = state
        .db()
        .append_permission_response_message(
            session_id.as_str(),
            req.id.as_str(),
            req.approved,
            None,
        )
        .await;
    if !responded {
        state
            .runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Idle,
            )
            .await
            .map_err(internal_error)?;
        runtime = super::chat::schedule_waiting_turn_resume(
            &state,
            session_id.as_str(),
            super::chat::ResumeRequestKind::Permission,
            if req.approved {
                super::chat::ResumeResolution::Approved
            } else {
                super::chat::ResumeResolution::Denied
            },
        )
        .await
        .map_err(internal_error)?;
    }

    Ok(Json(runtime.into()))
}

pub(super) async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let pending = state
        .db()
        .find_pending_ask_user_request(&req.id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("ask_user request not found or already resolved"))?;
    let session_id = pending.session_id;

    let answers = req
        .answers
        .into_iter()
        .map(|answer| {
            Ok(QuestionAnswer {
                section: answer.section,
                answer: answer.answer.try_into().map_err(bad_request_error)?,
            })
        })
        .collect::<SkyResult<Vec<_>>>()?;

    let responded = state.ask_user_approver.respond(&req.id, answers.clone());

    let mut runtime = state
        .runtime_service
        .set_waiting_ask_user(&session_id, None)
        .await
        .map_err(internal_error)?;
    if !responded {
        tracing::warn!(request_id = %req.id, session_id = %session_id, "ask-user response arrived without a live waiter");
    }

    let session_id = SessionId::from(session_id);
    let _ = state
        .db()
        .append_ask_user_response_message(session_id.as_str(), req.id.as_str(), &answers, None)
        .await;
    if !responded {
        state
            .runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Idle,
            )
            .await
            .map_err(internal_error)?;
        runtime = super::chat::schedule_waiting_turn_resume(
            &state,
            session_id.as_str(),
            super::chat::ResumeRequestKind::AskUser,
            super::chat::ResumeResolution::Answered,
        )
        .await
        .map_err(internal_error)?;
    }

    Ok(Json(runtime.into()))
}

pub(super) async fn respond_workspace_request(
    Json(req): Json<WorkspaceRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let pending = state
        .db()
        .find_pending_workspace_request(&req.id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("workspace request not found or already resolved"))?;
    let session_id = pending.session_id;

    let responded = state.workspace_approver.respond(&req.id, req.approved);

    let mut runtime = state
        .runtime_service
        .set_waiting_workspace(&session_id, None)
        .await
        .map_err(internal_error)?;
    if !responded {
        tracing::warn!(request_id = %req.id, session_id = %session_id, "workspace response arrived without a live waiter");
    }

    let session_id = SessionId::from(session_id);
    let _ = state
        .db()
        .append_workspace_response_message(session_id.as_str(), req.id.as_str(), req.approved, None)
        .await;
    if !responded {
        state
            .runtime_service
            .set_state(
                session_id.as_str(),
                crate::session::SessionRuntimeState::Idle,
            )
            .await
            .map_err(internal_error)?;
        runtime = super::chat::schedule_waiting_turn_resume(
            &state,
            session_id.as_str(),
            super::chat::ResumeRequestKind::Workspace,
            if req.approved {
                super::chat::ResumeResolution::Approved
            } else {
                super::chat::ResumeResolution::Denied
            },
        )
        .await
        .map_err(internal_error)?;
    }

    Ok(Json(runtime.into()))
}
