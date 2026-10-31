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
    }))
}

pub(super) async fn computer_start(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ComputerStatus>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
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
    Ok(Json(ComputerStatus { running }))
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
    let _ = state.runtime_service.runs().cancel(id.as_str());
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

#[cfg_attr(not(target_os = "macos"), allow(clippy::unused_async))]
async fn run_computer_start(state: Arc<DaemonState>, id: String) -> SkyResult<bool> {
    #[cfg(target_os = "macos")]
    {
        let (stream, display) = {
            let stream = state.session_streams.lock().await.stream(&id);
            let display = state
                .session_displays
                .lock()
                .await
                .acquire(&id)
                .map_err(|error| {
                    skyzen::Error::msg(error.to_string())
                        .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                })?;
            (stream, display)
        };
        stream.start(display).await.map_err(|error| {
            skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, id);
        Err(
            skyzen::Error::msg("session desktops are macOS-only for now")
                .set_status(StatusCode::NOT_FOUND),
        )
    }
}

#[cfg(target_os = "macos")]
async fn is_running(state: &DaemonState, id: &str) -> bool {
    state
        .session_streams
        .lock()
        .await
        .stream(id)
        .is_running()
        .await
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unused_async)]
async fn is_running(_state: &DaemonState, _id: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
async fn stop_stream(state: &DaemonState, id: &str) {
    state.session_streams.lock().await.stream(id).stop().await;
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unused_async)]
async fn stop_stream(_state: &DaemonState, _id: &str) {}

#[cfg(target_os = "macos")]
async fn answer_offer(state: &DaemonState, id: &str, offer_sdp: String) -> SkyResult<String> {
    state
        .session_streams
        .lock()
        .await
        .stream(id)
        .handle_offer(offer_sdp)
        .await
        .map_err(|error| skyzen::Error::msg(error.to_string()))
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unused_async)]
async fn answer_offer(_state: &DaemonState, _id: &str, _offer_sdp: String) -> SkyResult<String> {
    Err(
        skyzen::Error::msg("session desktops are macOS-only for now")
            .set_status(StatusCode::NOT_FOUND),
    )
}

#[cfg(target_os = "macos")]
async fn set_takeover(state: &DaemonState, id: &str, active: bool) {
    state
        .session_streams
        .lock()
        .await
        .stream(id)
        .set_takeover(active);
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unused_async)]
async fn set_takeover(_state: &DaemonState, _id: &str, _active: bool) {}
