use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::utils::State;
use tokio::time::MissedTickBehavior;

use crate::profile::query_profile_sessions;

use super::helpers::send_event;
use super::state::UiServerState;
use super::types::{
    ConfigEventPayload, NotificationMailboxEntryResponse, NotificationMailboxResponse,
    ProfileSessionsEventPayload, RequestLogsResponse, SessionRuntimeEventPayload,
    SessionRuntimeSnapshotItem, SessionRuntimesEventPayload, SseHeartbeat,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct EventsQuery {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
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
    let profile_id = query
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let (sender, sse) = Sse::channel();
    let state_cloned = state.clone();

    tokio::spawn(async move {
        let mut last_config_sig = String::new();
        let mut last_request_logs_sig = String::new();
        let mut last_notification_mailbox_sig = String::new();
        let mut last_container_prepare_sig = String::new();
        let mut last_runtime_sig = String::new();
        let mut last_profile_runtime_sig = String::new();
        let mut last_profile_sessions_sig = String::new();
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Ignore the first immediate tick; we emit initial state immediately below.
        let _ = heartbeat.tick().await;

        if !emit_config_if_changed(&state_cloned, &sender, &mut last_config_sig).await {
            return;
        }
        if !emit_request_logs_if_changed(&state_cloned, &sender, &mut last_request_logs_sig).await {
            return;
        }
        if !emit_notification_mailbox_if_changed(
            &state_cloned,
            &sender,
            &mut last_notification_mailbox_sig,
        )
        .await
        {
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
        if !emit_session_runtime_if_changed(
            &state_cloned,
            &sender,
            session_id.as_deref(),
            &mut last_runtime_sig,
        )
        .await
        {
            return;
        }
        if !emit_session_runtimes_if_changed(
            &state_cloned,
            &sender,
            profile_id.as_deref(),
            &mut last_profile_runtime_sig,
        )
        .await
        {
            return;
        }
        if !emit_profile_sessions_if_changed(
            &state_cloned,
            &sender,
            profile_id.as_deref(),
            &mut last_profile_sessions_sig,
        )
        .await
        {
            return;
        }

        loop {
            tokio::select! {
                _ = state_cloned.ui_events_notify.notified() => {
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
                    if !emit_notification_mailbox_if_changed(
                        &state_cloned,
                        &sender,
                        &mut last_notification_mailbox_sig,
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
                    if !emit_session_runtime_if_changed(
                        &state_cloned,
                        &sender,
                        session_id.as_deref(),
                        &mut last_runtime_sig,
                    )
                    .await
                    {
                        break;
                    }
                    if !emit_session_runtimes_if_changed(
                        &state_cloned,
                        &sender,
                        profile_id.as_deref(),
                        &mut last_profile_runtime_sig,
                    )
                    .await
                    {
                        break;
                    }
                    if !emit_profile_sessions_if_changed(
                        &state_cloned,
                        &sender,
                        profile_id.as_deref(),
                        &mut last_profile_sessions_sig,
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
    let logs = {
        let logs = state.request_logs.lock().await;
        logs.iter().cloned().collect()
    };
    let cache_summary = match state.db().await.assistant_cache_usage_summary().await {
        Ok(summary) => super::types::RequestLogCacheSummary::from(summary),
        Err(error) => {
            tracing::warn!(error = %error, "failed to build request log cache summary");
            return true;
        }
    };
    let payload = RequestLogsResponse {
        logs,
        cache_summary,
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

async fn emit_notification_mailbox_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_sig: &mut String,
) -> bool {
    let items = match state
        .db()
        .await
        .list_notification_mailbox_entries(100)
        .await
    {
        Ok(items) => items
            .into_iter()
            .map(NotificationMailboxEntryResponse::from)
            .collect(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to load notification mailbox");
            return true;
        }
    };
    let payload = NotificationMailboxResponse { items };

    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        *last_sig = sig;
        if !send_event(sender, "notification_mailbox", &payload).await {
            return false;
        }
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

async fn emit_session_runtime_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    session_filter: Option<&str>,
    last_sig: &mut String,
) -> bool {
    let Some(session_id) = session_filter else {
        return true;
    };
    let runtime = match state.runtime_service.get(session_id).await {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(session_id, error = %error, "failed to load session runtime");
            return true;
        }
    };
    let payload = SessionRuntimeEventPayload {
        session_id: session_id.to_string(),
        runtime: runtime.into(),
    };
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        if !send_event(sender, "session_runtime", &payload).await {
            return false;
        }
        *last_sig = sig;
    }
    true
}

async fn emit_session_runtimes_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    profile_filter: Option<&str>,
    last_sig: &mut String,
) -> bool {
    let Some(profile_id) = profile_filter else {
        return true;
    };
    let sessions = match state.runtime_service.list_for_profile(profile_id).await {
        Ok(runtimes) => runtimes
            .into_iter()
            .map(|(session_id, runtime)| SessionRuntimeSnapshotItem {
                session_id,
                runtime: runtime.into(),
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(profile_id, error = %error, "failed to load profile runtime snapshot");
            return true;
        }
    };
    let payload = SessionRuntimesEventPayload { sessions };
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        if !send_event(sender, "session_runtimes", &payload).await {
            return false;
        }
        *last_sig = sig;
    }
    true
}

async fn emit_profile_sessions_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    profile_filter: Option<&str>,
    last_sig: &mut String,
) -> bool {
    let Some(profile_id) = profile_filter else {
        return true;
    };
    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };
    let profile_sessions = match query_profile_sessions(&db, profile_id).await {
        Ok(profile_sessions) => profile_sessions,
        Err(error) => {
            tracing::warn!(profile_id, error = %error, "failed to load profile sessions snapshot");
            return true;
        }
    };
    let payload = ProfileSessionsEventPayload { profile_sessions };
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        if !send_event(sender, "profile_sessions", &payload).await {
            return false;
        }
        *last_sig = sig;
    }
    true
}
