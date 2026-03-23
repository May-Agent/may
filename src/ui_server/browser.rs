//! Browser live view API endpoints.
//!
//! - `GET  /api/browser/status` — Browser status
//! - `POST /api/browser/start`  — Start the browser
//! - `POST /api/browser/stop`   — Stop the browser
//! - `POST /api/browser/offer`  — WebRTC SDP offer → answer (WHEP-style signaling)

use std::sync::Arc;

use skyzen::utils::{Json, State};
use skyzen::{Result as SkyResult, StatusCode};

use super::state::UiServerState;
use super::types::BrowserTakeoverResponse;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowserStatus {
    running: bool,
    cdp_endpoint: Option<String>,
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

pub(super) async fn browser_status(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BrowserStatus>> {
    let takeover_session_id = state
        .runtime_service
        .find_browser_takeover_session()
        .await
        .map_err(|error| {
            skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    let Some(ref bm) = state.browser_manager else {
        return Ok(Json(BrowserStatus {
            running: false,
            cdp_endpoint: None,
            takeover_session_id,
        }));
    };
    let running = bm.is_running().await;
    Ok(Json(BrowserStatus {
        running,
        cdp_endpoint: if running {
            Some(bm.cdp_endpoint())
        } else {
            None
        },
        takeover_session_id,
    }))
}

pub(super) async fn browser_start(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BrowserStatus>> {
    let Some(ref bm) = state.browser_manager else {
        return Err(skyzen::Error::msg("Browser automation is not enabled")
            .set_status(StatusCode::NOT_FOUND));
    };

    bm.start().await.map_err(|e| {
        skyzen::Error::msg(e.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    Ok(Json(BrowserStatus {
        running: true,
        cdp_endpoint: Some(bm.cdp_endpoint()),
        takeover_session_id: state
            .runtime_service
            .find_browser_takeover_session()
            .await
            .map_err(|error| {
                skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
            })?,
    }))
}

pub(super) async fn browser_stop(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<BrowserStatus>> {
    let Some(ref bm) = state.browser_manager else {
        return Err(skyzen::Error::msg("Browser automation is not enabled")
            .set_status(StatusCode::NOT_FOUND));
    };

    bm.stop().await.map_err(|e| {
        skyzen::Error::msg(e.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    Ok(Json(BrowserStatus {
        running: false,
        cdp_endpoint: None,
        takeover_session_id: state
            .runtime_service
            .find_browser_takeover_session()
            .await
            .map_err(|error| {
                skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
            })?,
    }))
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
    let Some(ref bm) = state.browser_manager else {
        return Err(skyzen::Error::msg("Browser automation is not enabled")
            .set_status(StatusCode::NOT_FOUND));
    };

    let answer_sdp = bm.handle_offer(req.sdp).await.map_err(|e| {
        skyzen::Error::msg(e.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    Ok(Json(WebRtcAnswerResponse { sdp: answer_sdp }))
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
    state
        .runtime_service
        .clear_browser_takeover_owner(session_id.as_str())
        .await
        .map_err(|error| {
            skyzen::Error::msg(error.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    Ok(Json(BrowserTakeoverResponse {
        ok: true,
        session_id,
    }))
}
