//! Browser live view API endpoints.
//!
//! - `GET  /api/browser/status` — Browser status
//! - `POST /api/browser/start`  — Start the browser
//! - `POST /api/browser/stop`   — Stop the browser
//! - `POST /api/browser/offer`  — WebRTC SDP offer → answer (WHEP-style signaling)

use std::sync::Arc;

use skyzen::utils::{Json, State};
use skyzen::{Result as SkyResult, StatusCode};

use super::chat::spawn_follow_up_runs;
use super::state::UiServerState;
use super::types::BrowserTakeoverResponse;
use crate::text::error_chain_text;
use crate::ui_event_bus::UiEventTopics;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowserStatus {
    enabled: bool,
    running: bool,
    cdp_endpoint: Option<String>,
    owner_session_id: Option<String>,
    takeover_session_id: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebRtcOfferRequest {
    sdp: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebRtcAnswerResponse {
    sdp: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowserTakeoverRequest {
    session_id: String,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum EnsureBrowserRuntimeError {
    #[error("browser automation is not enabled")]
    Unavailable,
    #[error("failed to prepare browser runtime")]
    Prepare {
        #[source]
        source: crate::container::browser::BrowserError,
    },
    #[error("failed to start browser runtime")]
    Start {
        #[source]
        source: crate::container::browser::BrowserError,
    },
}

pub(super) async fn ensure_browser_runtime_started(
    state: &UiServerState,
) -> Result<Arc<crate::container::browser::BrowserManager>, EnsureBrowserRuntimeError> {
    let browser_manager = state
        .browser_manager()
        .ok_or(EnsureBrowserRuntimeError::Unavailable)?;
    if browser_manager.is_running().await {
        return Ok(browser_manager);
    }

    let config = state.config_snapshot();
    crate::container::browser::prepare_browser_runtime(&config.browser, state.may_dir.as_path())
        .await
        .map_err(|source| EnsureBrowserRuntimeError::Prepare { source })?;
    browser_manager
        .start()
        .await
        .map_err(|source| EnsureBrowserRuntimeError::Start { source })?;
    Ok(browser_manager)
}

pub(super) async fn browser_status(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BrowserStatus>> {
    let enabled = state.config_snapshot().browser.enabled;
    let owner_session_id = state.browser_owner_session_id();
    let takeover_session_id = state
        .runtime_service
        .find_browser_takeover_session()
        .await
        .map_err(|error| {
            skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    let Some(bm) = state.browser_manager() else {
        return Ok(Json(BrowserStatus {
            enabled,
            running: false,
            cdp_endpoint: None,
            owner_session_id,
            takeover_session_id,
        }));
    };
    let running = bm.is_running().await;
    Ok(Json(BrowserStatus {
        enabled,
        running,
        cdp_endpoint: if running { bm.cdp_endpoint() } else { None },
        owner_session_id,
        takeover_session_id,
    }))
}

pub(super) async fn browser_start(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BrowserStatus>> {
    let tokio_handle = state.tokio_handle.clone();
    let response = tokio_handle
        .spawn({
            let state = state.clone();
            async move { run_browser_start(state).await }
        })
        .await
        .map_err(|_| {
            skyzen::Error::msg("browser start task panicked before producing a response")
                .set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })??;
    Ok(Json(response))
}

pub(super) async fn browser_stop(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BrowserStatus>> {
    let tokio_handle = state.tokio_handle.clone();
    let response = tokio_handle
        .spawn({
            let state = state.clone();
            async move { run_browser_stop(state).await }
        })
        .await
        .map_err(|_| {
            skyzen::Error::msg("browser stop task panicked before producing a response")
                .set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })??;
    Ok(Json(response))
}

/// WebRTC SDP offer/answer signaling endpoint.
///
/// Client sends an SDP offer (recvonly video + `DataChannel`).
/// Server responds with SDP answer containing ICE candidates.
/// After this single round-trip, WebRTC media/data flows directly.
pub(super) async fn browser_offer(
    State(state): State<Arc<UiServerState>>,
    Json(req): Json<WebRtcOfferRequest>,
) -> SkyResult<Json<WebRtcAnswerResponse>> {
    let tokio_handle = state.tokio_handle.clone();
    let response = tokio_handle
        .spawn({
            let state = state.clone();
            async move { run_browser_offer(state, req).await }
        })
        .await
        .map_err(|_| {
            skyzen::Error::msg("browser offer task panicked before producing a response")
                .set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })??;
    Ok(Json(response))
}

async fn run_browser_start(state: Arc<UiServerState>) -> SkyResult<BrowserStatus> {
    let bm = ensure_browser_runtime_started(state.as_ref())
        .await
        .map_err(|error| {
            let status = match error {
                EnsureBrowserRuntimeError::Unavailable => StatusCode::NOT_FOUND,
                EnsureBrowserRuntimeError::Prepare { .. }
                | EnsureBrowserRuntimeError::Start { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            };
            skyzen::Error::msg(error_chain_text(&error)).set_status(status)
        })?;
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);

    Ok(BrowserStatus {
        enabled: true,
        running: true,
        cdp_endpoint: bm.cdp_endpoint(),
        owner_session_id: state.browser_owner_session_id(),
        takeover_session_id: state
            .runtime_service
            .find_browser_takeover_session()
            .await
            .map_err(|error| {
                skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
            })?,
    })
}

async fn run_browser_stop(state: Arc<UiServerState>) -> SkyResult<BrowserStatus> {
    let Some(bm) = state.browser_manager() else {
        return Err(skyzen::Error::msg("Browser automation is not enabled")
            .set_status(StatusCode::NOT_FOUND));
    };

    bm.stop().await.map_err(|error| {
        skyzen::Error::msg(error_chain_text(&error)).set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    state.set_browser_owner_session_id(None);
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);

    Ok(BrowserStatus {
        enabled: true,
        running: false,
        cdp_endpoint: None,
        owner_session_id: None,
        takeover_session_id: state
            .runtime_service
            .find_browser_takeover_session()
            .await
            .map_err(|error| {
                skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
            })?,
    })
}

async fn run_browser_offer(
    state: Arc<UiServerState>,
    req: WebRtcOfferRequest,
) -> SkyResult<WebRtcAnswerResponse> {
    let Some(bm) = state.browser_manager() else {
        return Err(skyzen::Error::msg("Browser automation is not enabled")
            .set_status(StatusCode::NOT_FOUND));
    };

    let answer_sdp = bm.handle_offer(req.sdp).await.map_err(|error| {
        skyzen::Error::msg(error_chain_text(&error)).set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    Ok(WebRtcAnswerResponse { sdp: answer_sdp })
}

/// Mark that user has taken over browser control for a session.
pub(super) async fn browser_takeover_start(
    State(state): State<Arc<UiServerState>>,
    Json(req): Json<BrowserTakeoverRequest>,
) -> SkyResult<Json<BrowserTakeoverResponse>> {
    let session_id = req.session_id.trim();
    if session_id.is_empty() {
        return Err(
            skyzen::Error::msg("session_id is required").set_status(StatusCode::BAD_REQUEST)
        );
    }

    let session_id = session_id.to_string();
    state
        .runtime_service
        .set_browser_takeover_owner(session_id.as_str())
        .await
        .map_err(|error| {
            skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    Ok(Json(BrowserTakeoverResponse {
        ok: true,
        session_id,
    }))
}

/// Release browser takeover for a session and enqueue post-takeover alert.
pub(super) async fn browser_takeover_stop(
    State(state): State<Arc<UiServerState>>,
    Json(req): Json<BrowserTakeoverRequest>,
) -> SkyResult<Json<BrowserTakeoverResponse>> {
    let session_id = req.session_id.trim();
    if session_id.is_empty() {
        return Err(
            skyzen::Error::msg("session_id is required").set_status(StatusCode::BAD_REQUEST)
        );
    }

    let session_id = session_id.to_string();
    let runtime = state
        .runtime_service
        .clear_browser_takeover_owner(session_id.as_str())
        .await
        .map_err(|error| {
            skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    if runtime.can_dispatch_follow_up() {
        spawn_follow_up_runs(state.clone(), session_id.clone());
    }
    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    Ok(Json(BrowserTakeoverResponse {
        ok: true,
        session_id,
    }))
}
