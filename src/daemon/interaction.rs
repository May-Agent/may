use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use super::error::{bad_request_error, internal_error};
use super::state::{AskUserOutcome, AskUserRespondError, DaemonState};
use super::types::SessionRuntimeResponse;
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

/// `POST /api/interactions/{id}`: `id` names the pending request; `kind`
/// names which approver answers it. Every response commits first (the
/// request cleared by its id, the answer in the history); only then is the
/// waiting run woken or the session resumed. A commit that fails is
/// returned and wakes nothing.
pub(super) async fn respond_interaction(
    params: Params,
    Json(req): Json<super::types::InteractionRespondRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    match req {
        super::types::InteractionRespondRequest::Permission { approved } => {
            resolve_permission(&state, &id, approved).await
        }
        super::types::InteractionRespondRequest::Workspace { approved } => {
            resolve_workspace(&state, &id, approved).await
        }
        super::types::InteractionRespondRequest::AskUser { answers } => {
            resolve_ask_user(&state, &id, answers).await
        }
    }
}

/// Every response commits first (the request cleared by its id, the answer
/// in the history); only then is the waiting run woken or the session
/// resumed. A commit that fails is returned and wakes nothing.
async fn resolve_permission(
    state: &Arc<DaemonState>,
    id: &str,
    approved: bool,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let resolved = state
        .permission_approver
        .resolve(id, approved)
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
        tracing::warn!(request_id = %id, session_id = %resolved.session_id, "permission response arrived without a live waiter");
        resume_after_decision(
            state,
            &resolved.session_id,
            super::chat::ResumeRequestKind::Permission,
            approved,
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

/// The HTTP error of a refused answer. A secret the store refuses is
/// reported with its message: a `5xx` body is redacted, and the person
/// answering is the one who can fix the keychain.
fn ask_user_respond_error(error: AskUserRespondError) -> skyzen::Error {
    match error {
        AskUserRespondError::Input(error) => bad_request_error(error.to_string()),
        AskUserRespondError::Answer(AnswerError::Mismatch(error)) => {
            bad_request_error(error.to_string())
        }
        error @ AskUserRespondError::Store { .. } => skyzen::Error::msg(error.to_string())
            .set_status(skyzen::StatusCode::UNPROCESSABLE_ENTITY),
        error @ (AskUserRespondError::Answer(AnswerError::Storage(_))
        | AskUserRespondError::Lookup(_)) => internal_error(error),
    }
}

async fn resolve_ask_user(
    state: &Arc<DaemonState>,
    id: &str,
    answers: Vec<crate::daemon::state::UiQuestionAnswer>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let resolved = state
        .ask_user_approver
        .respond(id, answers)
        .await
        .map_err(ask_user_respond_error)?
        .ok_or_else(ask_user_not_open_error)?;
    super::chat::cards::announce_answer(state, &resolved.session_id, id).await;
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
            tracing::warn!(request_id = %id, session_id = %resolved.session_id, "authorization answer arrived without a live waiter");
            resume_after_decision(
                state,
                &resolved.session_id,
                super::chat::ResumeRequestKind::Authorization,
                approved,
            )
            .await?
        }
    };

    Ok(Json(runtime.into()))
}

async fn resolve_workspace(
    state: &Arc<DaemonState>,
    id: &str,
    approved: bool,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let resolved = state
        .workspace_approver
        .resolve(id, approved)
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
        tracing::warn!(request_id = %id, session_id = %resolved.session_id, "workspace response arrived without a live waiter");
        resume_after_decision(
            state,
            &resolved.session_id,
            super::chat::ResumeRequestKind::Workspace,
            approved,
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

    #[test]
    fn a_secret_the_store_refuses_is_reported_with_its_message() {
        let error = ask_user_respond_error(AskUserRespondError::Store {
            name: "deploy_token".to_string(),
            source: crate::secrets::SecretError::InvalidName("1x".to_string()),
        });
        assert_eq!(error.status(), skyzen::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            error
                .to_string()
                .contains("failed to store secret deploy_token"),
            "{error}"
        );
    }
}
