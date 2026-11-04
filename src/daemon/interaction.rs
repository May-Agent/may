use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use super::error::{bad_request_error, internal_error};
use super::state::{AskUserOutcome, AskUserRespondError, AskUserRespondRequest, DaemonState};
use super::types::{PermissionRespondRequest, SessionRuntimeResponse, WorkspaceRespondRequest};
use crate::session::AnswerError;

/// Resume a session whose run is gone with the decision it was waiting for.
/// A session whose run is live (in this or another process) is left alone:
/// that run is answered through its own prompt, and the resume takes the
/// run lease before writing anything.
async fn resume_after_decision(
    state: &Arc<DaemonState>,
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
    State(state): State<Arc<DaemonState>>,
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

/// The status of a question that is no longer open.
///
/// Withdrawn or already answered; clients tell it apart from a failure to
/// store the answer, a `5xx`, and from an answer that does not fit the
/// question, a `400`.
pub const ASK_USER_NOT_OPEN: skyzen::StatusCode = skyzen::StatusCode::NOT_FOUND;

fn ask_user_not_open_error() -> skyzen::Error {
    skyzen::Error::msg("ask_user request not found or withdrawn").set_status(ASK_USER_NOT_OPEN)
}

pub(super) async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let resolved = state
        .ask_user_approver
        .respond(&req.id, req.answers)
        .await
        .map_err(|error| match error {
            AskUserRespondError::Input(error) => bad_request_error(error.to_string()),
            AskUserRespondError::Answer(AnswerError::Mismatch(error)) => {
                bad_request_error(error.to_string())
            }
            error @ (AskUserRespondError::Answer(AnswerError::Storage(_))
            | AskUserRespondError::Lookup(_)
            | AskUserRespondError::Store { .. }) => internal_error(error),
        })?
        .ok_or_else(ask_user_not_open_error)?;
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
    State(state): State<Arc<DaemonState>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_that_is_not_open_is_a_404() {
        assert_eq!(
            ask_user_not_open_error().status(),
            skyzen::StatusCode::NOT_FOUND
        );
    }
}
