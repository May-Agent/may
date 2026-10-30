use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use crate::ask_user::QuestionAnswer;

use super::error::{bad_request_error, internal_error};
use super::state::{AskUserOutcome, AskUserRespondRequest, UiServerState};
use super::types::{PermissionRespondRequest, SessionRuntimeResponse, WorkspaceRespondRequest};
use crate::session::AnswerError;

/// Resume a session whose run is gone with the decision it was waiting for.
/// A session whose run is live (in this or another process) is left alone:
/// that run is answered through its own prompt, and the resume takes the
/// run lease before writing anything.
async fn resume_after_decision(
    state: &Arc<UiServerState>,
    session_id: &str,
    request_kind: super::chat::ResumeRequestKind,
    approved: bool,
) -> SkyResult<crate::session::SessionRuntimeSummary> {
    super::chat::schedule_waiting_turn_resume(
        state,
        session_id,
        request_kind,
        if approved {
            super::chat::ResumeResolution::Approved
        } else {
            super::chat::ResumeResolution::Denied
        },
    )
    .await
    .map_err(internal_error)
}

/// Every response commits first (the request cleared by its id, the answer
/// in the history); only then is the waiting run woken or the session
/// resumed. A commit that fails is returned and wakes nothing.
pub(super) async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let resolved = state
        .permission_approver
        .resolve(&req.id, req.approved)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("permission request not found or already resolved"))?;
    let runtime = if resolved.woke {
        state
            .runtime_service
            .get(&resolved.session_id)
            .await
            .map_err(internal_error)?
    } else {
        tracing::warn!(request_id = %req.id, session_id = %resolved.session_id, "permission response arrived without a live waiter");
        resume_after_decision(
            &state,
            &resolved.session_id,
            super::chat::ResumeRequestKind::Permission,
            req.approved,
        )
        .await?
    };

    Ok(Json(runtime.into()))
}

pub(super) async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
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

    let resolved = state
        .ask_user_approver
        .resolve(&req.id, answers)
        .await
        .map_err(|error| match error {
            AnswerError::Mismatch(error) => bad_request_error(error.to_string()),
            AnswerError::Storage(error) => internal_error(error),
        })?
        .ok_or_else(|| bad_request_error("ask_user request not found or withdrawn"))?;
    let runtime = match resolved.outcome {
        AskUserOutcome::Woke | AskUserOutcome::AlreadyAnswered => state
            .runtime_service
            .get(&resolved.session_id)
            .await
            .map_err(internal_error)?,
        AskUserOutcome::Queued => {
            let runtime = state
                .runtime_service
                .get(&resolved.session_id)
                .await
                .map_err(internal_error)?;
            super::chat::spawn_follow_up_runs(state.clone(), resolved.session_id);
            runtime
        }
        AskUserOutcome::Resume { approved } => {
            tracing::warn!(request_id = %req.id, session_id = %resolved.session_id, "authorization answer arrived without a live waiter");
            resume_after_decision(
                &state,
                &resolved.session_id,
                super::chat::ResumeRequestKind::Authorization,
                approved,
            )
            .await?
        }
    };

    Ok(Json(runtime.into()))
}

pub(super) async fn respond_workspace_request(
    Json(req): Json<WorkspaceRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let resolved = state
        .workspace_approver
        .resolve(&req.id, req.approved)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("workspace request not found or already resolved"))?;
    let runtime = if resolved.woke {
        state
            .runtime_service
            .get(&resolved.session_id)
            .await
            .map_err(internal_error)?
    } else {
        tracing::warn!(request_id = %req.id, session_id = %resolved.session_id, "workspace response arrived without a live waiter");
        resume_after_decision(
            &state,
            &resolved.session_id,
            super::chat::ResumeRequestKind::Workspace,
            req.approved,
        )
        .await?
    };

    Ok(Json(runtime.into()))
}
