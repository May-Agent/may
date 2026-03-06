use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::utils::State;
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

use super::helpers::send_event;
use super::state::{AskUserPollResponse, UiServerState};
use super::types::{
    ConfigEventPayload, PermissionPollItem, PermissionPollResponse, RequestLogsResponse,
    SseHeartbeat,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct EventsQuery {
    #[serde(default)]
    session_id: Option<String>,
}

pub(super) async fn stream_events(
    Query(query): Query<EventsQuery>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Sse> {
    let session_id = query
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let (sender, sse) = Sse::channel();
    let state_cloned = state.clone();

    tokio::spawn(async move {
        let mut last_permission_sig = String::new();
        let mut last_ask_user_sig = String::new();
        let mut last_config_sig = String::new();
        let mut last_request_logs_sig = String::new();
        let mut last_container_prepare_sig = String::new();
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Ignore the first immediate tick; we emit initial state immediately below.
        let _ = heartbeat.tick().await;

        if !emit_permission_if_changed(
            &state_cloned,
            &sender,
            session_id.as_deref(),
            &mut last_permission_sig,
        )
        .await
        {
            return;
        }
        if !emit_ask_user_if_changed(
            &state_cloned,
            &sender,
            session_id.as_deref(),
            &mut last_ask_user_sig,
        )
        .await
        {
            return;
        }
        if !emit_config_if_changed(&state_cloned, &sender, &mut last_config_sig).await {
            return;
        }
        if !emit_request_logs_if_changed(&state_cloned, &sender, &mut last_request_logs_sig).await {
            return;
        }
        if !emit_container_prepare_if_changed(
            &state_cloned,
            &sender,
            &mut last_container_prepare_sig,
        )
        .await
        {
            return;
        }

        loop {
            tokio::select! {
                _ = state_cloned.permission_approver.listen() => {
                    if !emit_permission_if_changed(
                        &state_cloned,
                        &sender,
                        session_id.as_deref(),
                        &mut last_permission_sig,
                    ).await {
                        break;
                    }
                }
                _ = state_cloned.ask_user_approver.listen() => {
                    if !emit_ask_user_if_changed(
                        &state_cloned,
                        &sender,
                        session_id.as_deref(),
                        &mut last_ask_user_sig,
                    ).await {
                        break;
                    }
                }
                _ = wait_for_notify(&state_cloned.ui_events_notify) => {
                    if !emit_config_if_changed(
                        &state_cloned,
                        &sender,
                        &mut last_config_sig,
                    ).await {
                        break;
                    }
                    if !emit_request_logs_if_changed(
                        &state_cloned,
                        &sender,
                        &mut last_request_logs_sig,
                    )
                    .await
                    {
                        break;
                    }
                    if !emit_container_prepare_if_changed(
                        &state_cloned,
                        &sender,
                        &mut last_container_prepare_sig,
                    )
                    .await
                    {
                        break;
                    }
                }
                _ = heartbeat.tick() => {
                    if !send_event(&sender, "heartbeat", &SseHeartbeat { ok: true }).await {
                        break;
                    }
                }
            }
        }
    });

    Ok(sse)
}

async fn wait_for_notify(notify: &Arc<Notify>) {
    notify.notified().await;
}

async fn emit_permission_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    session_filter: Option<&str>,
    last_permission_sig: &mut String,
) -> bool {
    let permission = state
        .permission_approver
        .snapshot_session_runtime(session_filter)
        .map(|(id, request)| PermissionPollItem { id, request });
    let permission_payload = PermissionPollResponse {
        state: permission.as_ref().map(|_| "waiting_permission"),
        request: permission,
    };
    if let Ok(sig) = serde_json::to_string(&permission_payload)
        && sig != *last_permission_sig
    {
        if !send_event(sender, "permission", &permission_payload).await {
            return false;
        }
        *last_permission_sig = sig;
    }
    true
}

async fn emit_ask_user_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    session_filter: Option<&str>,
    last_ask_user_sig: &mut String,
) -> bool {
    let ask_user_payload = AskUserPollResponse {
        request: state.ask_user_approver.snapshot_pending(session_filter),
    };
    if let Ok(sig) = serde_json::to_string(&ask_user_payload)
        && sig != *last_ask_user_sig
    {
        if !send_event(sender, "ask_user", &ask_user_payload).await {
            return false;
        }
        *last_ask_user_sig = sig;
    }
    true
}

async fn emit_config_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_config_sig: &mut String,
) -> bool {
    let config_payload = {
        let config = state.config.read().await;
        ConfigEventPayload {
            ui_font: config.appearance.font.clone(),
            ui_theme: config.appearance.theme.clone(),
            ssh_servers: config.ssh_servers.clone(),
            max_iterations: config.max_iterations,
        }
    };
    if let Ok(sig) = serde_json::to_string(&config_payload)
        && sig != *last_config_sig
    {
        if !send_event(sender, "config", &config_payload).await {
            return false;
        }
        *last_config_sig = sig;
    }
    true
}

async fn emit_request_logs_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_logs_sig: &mut String,
) -> bool {
    let payload = {
        let logs = state.request_logs.lock().await;
        RequestLogsResponse {
            logs: logs.iter().cloned().collect(),
        }
    };

    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_logs_sig
    {
        if !send_event(sender, "request_logs", &payload).await {
            return false;
        }
        *last_logs_sig = sig;
    }
    true
}

async fn emit_container_prepare_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_sig: &mut String,
) -> bool {
    let payload = state.snapshot_container_prepare_statuses().await;
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        if !send_event(sender, "container_prepare", &payload).await {
            return false;
        }
        *last_sig = sig;
    }
    true
}
