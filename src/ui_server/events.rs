use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::utils::State;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::MissedTickBehavior;

use crate::profile::query_profile_sessions;
use crate::ui_event_bus::UiEventTopics;

use super::config::build_ui_config_response;
use super::error::bad_request_error;
use super::helpers::send_event;
use super::state::UiServerState;
use super::types::{
    BrowserLifecycleEvent, BrowserStatusEventPayload, NetworkAccessLogEntryResponse,
    NetworkAccessLogsResponse, NotificationMailboxEntryResponse, NotificationMailboxResponse,
    ProfileSessionsEventPayload, RemoteAccessEventPayload, RequestLogsResponse,
    SessionRuntimeEventPayload, SessionRuntimeSnapshotItem, SessionRuntimesEventPayload,
    SseHeartbeat,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize, Default, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct EventsQuery {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    topics: Option<String>,
}

#[derive(Default)]
struct EventSignatures {
    config: String,
    request_logs: String,
    notification_mailbox: String,
    network_access_logs: String,
    container_prepare: String,
    browser_status: String,
    session_runtime: String,
    session_runtimes: String,
    profile_sessions: String,
    remote_access: String,
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
    let subscribed_topics = query
        .topics
        .as_deref()
        .map(UiEventTopics::from_csv)
        .transpose()
        .map_err(|error| bad_request_error(error.to_string()))?
        .unwrap_or(UiEventTopics::ALL);

    let (sender, sse) = Sse::channel();
    let state_cloned = state.clone();
    let mut topic_rx = state.ui_event_bus.subscribe();
    // Lifecycle events are sequential transients, not snapshots, so they
    // ride a dedicated broadcast — but stay inside the single SSE stream as
    // a distinct event kind, per AGENTS.md.
    let mut lifecycle_rx = state.browser_lifecycle_subscribe();
    let lifecycle_subscribed = subscribed_topics.contains(UiEventTopics::BROWSER_LIFECYCLE);

    state.tokio_handle.spawn(async move {
        let mut signatures = EventSignatures::default();
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let _ = heartbeat.tick().await;

        if !emit_topics_if_changed(
            &state_cloned,
            &sender,
            session_id.as_deref(),
            profile_id.as_deref(),
            subscribed_topics,
            &mut signatures,
        )
        .await
        {
            return;
        }

        loop {
            tokio::select! {
                changed = topic_rx.recv() => {
                    let mut dirty_topics = match changed {
                        Ok(topics) => topics & subscribed_topics,
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "UI event stream lagged; forcing subscribed-topic resync");
                            subscribed_topics
                        }
                        Err(RecvError::Closed) => break,
                    };
                    loop {
                        match topic_rx.try_recv() {
                            Ok(extra_topics) => {
                                dirty_topics |= extra_topics & subscribed_topics;
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Lagged(skipped)) => {
                                tracing::warn!(skipped, "UI event stream lagged during coalescing; forcing subscribed-topic resync");
                                dirty_topics = subscribed_topics;
                                break;
                            }
                            Err(TryRecvError::Closed) => return,
                        }
                    }
                    if dirty_topics.is_empty() {
                        continue;
                    }
                    if !emit_topics_if_changed(
                        &state_cloned,
                        &sender,
                        session_id.as_deref(),
                        profile_id.as_deref(),
                        dirty_topics,
                        &mut signatures,
                    ).await {
                        break;
                    }
                }
                lifecycle = lifecycle_rx.recv(), if lifecycle_subscribed => {
                    match lifecycle {
                        Ok(event) => {
                            if !send_event(&sender, "browser_lifecycle", &event).await {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "browser lifecycle stream lagged; dropping events");
                        }
                        Err(RecvError::Closed) => {}
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

async fn emit_topics_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    session_filter: Option<&str>,
    profile_filter: Option<&str>,
    topics: UiEventTopics,
    signatures: &mut EventSignatures,
) -> bool {
    if topics.contains(UiEventTopics::CONFIG)
        && !emit_config_if_changed(state, sender, &mut signatures.config).await
    {
        return false;
    }
    if topics.contains(UiEventTopics::REQUEST_LOGS)
        && !emit_request_logs_if_changed(state, sender, &mut signatures.request_logs).await
    {
        return false;
    }
    if topics.contains(UiEventTopics::NOTIFICATION_MAILBOX)
        && !emit_notification_mailbox_if_changed(
            state,
            sender,
            &mut signatures.notification_mailbox,
        )
        .await
    {
        return false;
    }
    if topics.contains(UiEventTopics::NETWORK_ACCESS_LOGS)
        && !emit_network_access_logs_if_changed(state, sender, &mut signatures.network_access_logs)
            .await
    {
        return false;
    }
    if topics.contains(UiEventTopics::CONTAINER_PREPARE)
        && !emit_container_prepare_if_changed(state, sender, &mut signatures.container_prepare)
            .await
    {
        return false;
    }
    if topics.contains(UiEventTopics::BROWSER_STATUS)
        && !emit_browser_status_if_changed(state, sender, &mut signatures.browser_status).await
    {
        return false;
    }
    if topics.contains(UiEventTopics::SESSION_RUNTIME)
        && !emit_session_runtime_if_changed(
            state,
            sender,
            session_filter,
            &mut signatures.session_runtime,
        )
        .await
    {
        return false;
    }
    if topics.contains(UiEventTopics::SESSION_RUNTIMES)
        && !emit_session_runtimes_if_changed(
            state,
            sender,
            profile_filter,
            &mut signatures.session_runtimes,
        )
        .await
    {
        return false;
    }
    if topics.contains(UiEventTopics::PROFILE_SESSIONS)
        && !emit_profile_sessions_if_changed(
            state,
            sender,
            profile_filter,
            &mut signatures.profile_sessions,
        )
        .await
    {
        return false;
    }
    if topics.contains(UiEventTopics::REMOTE_ACCESS)
        && !emit_remote_access_if_changed(state, sender, &mut signatures.remote_access).await
    {
        return false;
    }
    true
}

async fn emit_config_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_config_sig: &mut String,
) -> bool {
    let config_payload = {
        let config = state.config_snapshot();
        build_ui_config_response(&config)
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
    let logs = match state.db().list_request_logs(300).await {
        Ok(entries) => entries
            .into_iter()
            .map(super::types::RequestLogEntry::from)
            .collect(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to list request logs");
            return true;
        }
    };
    let cache_summary = match state.db().assistant_cache_usage_summary().await {
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

async fn emit_browser_status_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_browser_status_sig: &mut String,
) -> bool {
    let takeover_session_id = match state.runtime_service.find_browser_takeover_session().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, "failed to load browser takeover status");
            None
        }
    };
    let browser_manager = state.browser_manager();
    let enabled = state.config_snapshot().browser.enabled;
    let owner_session_id = state.browser_owner_session_id();
    let running = if let Some(manager) = browser_manager.as_ref() {
        manager.is_running().await
    } else {
        false
    };
    let payload = BrowserStatusEventPayload {
        enabled,
        running,
        cdp_endpoint: if running {
            browser_manager
                .as_ref()
                .and_then(|manager| manager.cdp_endpoint())
        } else {
            None
        },
        owner_session_id,
        takeover_session_id,
    };
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_browser_status_sig
    {
        if !send_event(sender, "browser_status", &payload).await {
            return false;
        }
        *last_browser_status_sig = sig;
    }
    true
}

async fn emit_notification_mailbox_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_sig: &mut String,
) -> bool {
    let items = match state.db().list_notification_mailbox_entries(100).await {
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

async fn emit_network_access_logs_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_sig: &mut String,
) -> bool {
    let items = match state.db().list_network_access_logs(200).await {
        Ok(items) => items
            .into_iter()
            .map(NetworkAccessLogEntryResponse::from)
            .collect(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to load network access logs");
            return true;
        }
    };
    let payload = NetworkAccessLogsResponse { items };
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        if !send_event(sender, "network_access_logs", &payload).await {
            return false;
        }
        *last_sig = sig;
    }
    true
}

async fn emit_container_prepare_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_sig: &mut String,
) -> bool {
    let payload = state.snapshot_container_prepare_statuses();
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

async fn emit_remote_access_if_changed(
    state: &Arc<UiServerState>,
    sender: &skyzen::responder::sse::Sender,
    last_sig: &mut String,
) -> bool {
    let payload = RemoteAccessEventPayload {
        status: state.remote_access_manager.snapshot(),
    };
    if let Ok(sig) = serde_json::to_string(&payload)
        && sig != *last_sig
    {
        if !send_event(sender, "remote_access", &payload).await {
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
    let db = state.db();
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
