//! Session desktop stream endpoints — the real computer-use surface.
//!
//! - `GET  /api/sessions/{id}/computer/status` — whether the desktop's stream is live
//! - `POST /api/sessions/{id}/computer/start`  — create the display (first time) + start streaming
//! - `POST /api/sessions/{id}/computer/stop`   — put the stream to sleep; the display keeps its windows
//! - `POST /api/sessions/{id}/computer/offer`  — WebRTC SDP offer → answer

use std::sync::Arc;

use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Result as SkyResult, StatusCode};

use super::chat::spawn_follow_up_runs;
use super::error::{bad_request_error, internal_error_message};
use super::state::DaemonState;
use super::types::ApiHealth;
use crate::ui_event_bus::UiEventTopics;

#[derive(serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComputerStatus {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    needs_permissions: Option<ComputerPermissions>,
}

/// macOS TCC grants the desktop stack is missing; `null` when granted.
#[derive(serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComputerPermissions {
    pub screen_recording: bool,
    pub accessibility: bool,
}

#[derive(serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComputerOfferRequest {
    sdp: String,
}

#[derive(serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComputerTakeoverResponse {
    ok: bool,
    takeover: bool,
}

#[derive(serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComputerAnswerResponse {
    sdp: String,
}

pub(super) async fn computer_status(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ComputerStatus>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    Ok(Json(ComputerStatus {
        running: is_running(&state, &id).await,
        needs_permissions: needs_permissions(&state).await?,
    }))
}

pub(super) async fn computer_start(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ComputerStatus>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    if let Some(permissions) = needs_permissions(&state).await? {
        // An explicit start is the consent point: raise the TCC prompts,
        // then respawn the helper so the next call sees a fresh grant.
        let computer = state.computer.clone();
        tokio::task::spawn_blocking(move || {
            computer.request_permissions()?;
            computer.restart()
        })
        .await
        .map_err(internal_error_message)?
        .map_err(internal_error_message)?;
        return Ok(Json(ComputerStatus {
            running: false,
            needs_permissions: Some(permissions),
        }));
    }
    let tokio_handle = state.tokio_handle.clone();
    let running = tokio_handle
        .spawn({
            let state = state.clone();
            async move { run_computer_start(state, id).await }
        })
        .await
        .map_err(|_| {
            internal_error_message("computer start task panicked before producing a response")
        })??;
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    Ok(Json(ComputerStatus {
        running,
        needs_permissions: needs_permissions(&state).await?,
    }))
}

pub(super) async fn computer_stop(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    stop_stream(&state, &id).await;
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    Ok(Json(super::types::API_OK))
}

pub(super) async fn computer_offer(
    params: Params,
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ComputerOfferRequest>,
) -> SkyResult<Json<ComputerAnswerResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let sdp = answer_offer(&state, &id, req.sdp).await?;
    Ok(Json(ComputerAnswerResponse { sdp }))
}

/// The session's user takes the keyboard and mouse; an in-flight agent
/// run is interrupted so it does not fight the human.
pub(super) async fn computer_takeover_start(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ComputerTakeoverResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    set_takeover(&state, &id, true).await;
    let _ = state
        .runtime_service
        .runs()
        .cancel(id.as_str(), crate::session::CancelReason::User);
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    Ok(Json(ComputerTakeoverResponse {
        ok: true,
        takeover: true,
    }))
}

/// Hands the desktop back to the agent and lets queued follow-ups run.
pub(super) async fn computer_takeover_stop(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ComputerTakeoverResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    set_takeover(&state, &id, false).await;
    // Anything the model queued while the human drove runs now; the
    // dispatch is a no-op when the session has no pending work.
    spawn_follow_up_runs(state.clone(), id.clone());
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    Ok(Json(ComputerTakeoverResponse {
        ok: true,
        takeover: false,
    }))
}

use crate::computer::proto::{AgentRequest, AgentResponse};

/// The TCC grants the helper is still missing; `None` when all granted.
/// Read-only — status polls must never raise prompts or kill the helper.
/// An unreachable helper is an error, never a permission state.
async fn needs_permissions(state: &DaemonState) -> SkyResult<Option<ComputerPermissions>> {
    match rpc(state, AgentRequest::Preflight).await? {
        AgentResponse::Permissions {
            screen_recording,
            accessibility,
        } => Ok(
            (!screen_recording || !accessibility).then_some(ComputerPermissions {
                screen_recording,
                accessibility,
            }),
        ),
        AgentResponse::Error { error } => {
            Err(skyzen::Error::msg(error).set_status(StatusCode::INTERNAL_SERVER_ERROR))
        }
        _ => Err(skyzen::Error::msg("unexpected reply from computer-agent")
            .set_status(StatusCode::INTERNAL_SERVER_ERROR)),
    }
}

async fn rpc(state: &DaemonState, request: AgentRequest) -> SkyResult<AgentResponse> {
    state
        .computer
        .request(request)
        .await
        .map_err(|error| skyzen::Error::msg(error).set_status(StatusCode::INTERNAL_SERVER_ERROR))
}

async fn run_computer_start(state: Arc<DaemonState>, id: String) -> SkyResult<bool> {
    let codec = state.config_snapshot().stream.codec;
    match rpc(
        &state,
        AgentRequest::StartStream {
            session_id: id,
            codec,
        },
    )
    .await?
    {
        AgentResponse::Ok => Ok(true),
        AgentResponse::Error { error } => {
            Err(skyzen::Error::msg(error).set_status(StatusCode::INTERNAL_SERVER_ERROR))
        }
        _ => Err(skyzen::Error::msg("unexpected reply from computer-agent")
            .set_status(StatusCode::INTERNAL_SERVER_ERROR)),
    }
}

async fn is_running(state: &DaemonState, id: &str) -> bool {
    matches!(
        state
            .computer
            .request(AgentRequest::StreamRunning {
                session_id: id.to_owned(),
            })
            .await,
        Ok(AgentResponse::Flag { value: true })
    )
}

async fn stop_stream(state: &DaemonState, id: &str) {
    let _ = state
        .computer
        .request(AgentRequest::StopStream {
            session_id: id.to_owned(),
        })
        .await;
}

async fn answer_offer(state: &DaemonState, id: &str, offer_sdp: String) -> SkyResult<String> {
    match rpc(
        state,
        AgentRequest::Offer {
            session_id: id.to_owned(),
            sdp: offer_sdp,
        },
    )
    .await?
    {
        AgentResponse::Sdp { sdp } => Ok(sdp),
        // The viewer's offer is the problem, as on the browser stream.
        AgentResponse::OfferRefused { error } => {
            Err(skyzen::Error::msg(error).set_status(StatusCode::UNPROCESSABLE_ENTITY))
        }
        AgentResponse::Error { error } => Err(skyzen::Error::msg(error)),
        _ => Err(skyzen::Error::msg("unexpected reply from computer-agent")
            .set_status(StatusCode::INTERNAL_SERVER_ERROR)),
    }
}

async fn set_takeover(state: &DaemonState, id: &str, active: bool) {
    let _ = state
        .computer
        .request(AgentRequest::SetTakeover {
            session_id: id.to_owned(),
            active,
        })
        .await;
}
