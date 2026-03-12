//! Web Push notification API endpoints.

use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use super::error::internal_error;
use super::state::UiServerState;

// ── Request/response types ──────────────────────────────

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VapidKeyResponse {
    pub public_key: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubscribeRequest {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UnsubscribeRequest {
    pub endpoint: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TestNotificationRequest {
    #[serde(default = "default_test_title")]
    pub title: String,
    #[serde(default = "default_test_body")]
    pub body: String,
}

fn default_test_title() -> String {
    "May".to_string()
}

fn default_test_body() -> String {
    "Test notification from May".to_string()
}

// ── Endpoints ──────────────────────────────────────────

/// GET /api/push/vapid-key — return the VAPID public key for browser subscription.
pub(super) async fn get_vapid_key(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<VapidKeyResponse>> {
    Ok(Json(VapidKeyResponse {
        public_key: state.notification_service.public_key().to_string(),
    }))
}

/// POST /api/push/subscribe — register a browser push subscription.
pub(super) async fn subscribe(
    Json(req): Json<SubscribeRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    state
        .notification_service
        .subscribe(&req.endpoint, &req.p256dh, &req.auth)
        .await
        .map_err(internal_error)?;

    Ok(Json(super::types::API_OK))
}

/// POST /api/push/unsubscribe — remove a browser push subscription.
pub(super) async fn unsubscribe(
    Json(req): Json<UnsubscribeRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    state
        .notification_service
        .unsubscribe(&req.endpoint)
        .await
        .map_err(internal_error)?;

    Ok(Json(super::types::API_OK))
}

/// POST /api/push/test — send a test notification to all subscribers.
pub(super) async fn test_notification(
    Json(req): Json<TestNotificationRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::NotificationSentResponse>> {
    let sent = state
        .notification_service
        .send(&req.title, &req.body)
        .await
        .map_err(internal_error)?;

    Ok(Json(super::types::NotificationSentResponse {
        ok: true,
        sent,
    }))
}

pub(super) async fn list_notification_mailbox(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::NotificationMailboxResponse>> {
    let items = state
        .db()
        .await
        .list_notification_mailbox_entries(100)
        .await
        .map_err(internal_error)?
        .into_iter()
        .map(super::types::NotificationMailboxEntryResponse::from)
        .collect();
    Ok(Json(super::types::NotificationMailboxResponse { items }))
}
