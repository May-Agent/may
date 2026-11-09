//! `PATCH /api/sessions/{id}`, `POST .../turns`, `GET|DELETE .../queue`,
//! `POST /api/interactions/{id}` and `POST .../actions/{name}` — the
//! consolidated session-command routes.

use std::sync::Arc;

use serde_json::{Value, json};
use skyzen::StatusCode;

use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;
use crate::session::ChatMessage;
use crate::ui_event_bus::UiEventTopics;

const SESSION: &str = "s";

async fn state() -> Arc<DaemonState> {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
    let dir = tempfile::tempdir().expect("temp dir");
    let state = DaemonState::for_test(dir.path(), MayConfig::default())
        .await
        .expect("daemon state");
    std::mem::forget(dir);
    Arc::new(state)
}

async fn seed(state: &DaemonState) {
    seed_into(state, SESSION).await;
}

async fn seed_into(state: &DaemonState, session_id: &str) {
    if state
        .db()
        .get_profile(&state.default_profile_id)
        .await
        .expect("profile")
        .is_none()
    {
        state
            .db()
            .create_profile(crate::db::CreateProfileParams {
                id: &state.default_profile_id,
                name: "Test",
                environment: crate::db::ProfileEnvironmentKind::Local,
                slug: None,
                icon: None,
                persona: None,
                preferences: None,
                location_aware: false,
            })
            .await
            .expect("profile");
    }
    state
        .db()
        .create_session_in_profile(
            session_id,
            Some("Session"),
            Some("test-model"),
            Some(&state.default_profile_id),
        )
        .await
        .expect("session");
}

async fn call(
    router: &skyzen::routing::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    builder = builder.header("content-type", "application/json");
    let body = body.map_or_else(skyzen::Body::empty, |value| {
        skyzen::Body::from(serde_json::to_vec(&value).expect("body"))
    });
    let response = match router.go(builder.body(body).expect("request")).await {
        Ok(response) => response,
        Err(error) => skyzen::error_response(&*error),
    };
    let status = response.status();
    let text = response.into_body().into_string().await.expect("body");
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_patch_applies_every_field_and_notifies_the_list_once() {
    let state = state().await;
    seed(&state).await;
    let mut events = state.ui_event_bus.subscribe();
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let (status, body) = call(
        &router,
        "PATCH",
        &format!("/api/sessions/{SESSION}"),
        Some(json!({"title": "Renamed", "pinned": true, "yolo": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch body: {body}");
    assert_eq!(body["title"].as_str(), Some("Renamed"));
    assert_eq!(body["runtime"]["yolo"].as_bool(), Some(true));

    let session = state
        .db()
        .get_session(SESSION)
        .await
        .expect("db")
        .expect("row");
    assert!(session.pinned);
    assert_eq!(session.title.as_deref(), Some("Renamed"));

    let notifications: Vec<UiEventTopics> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    let list_notifications = notifications
        .iter()
        .filter(|topics| topics.contains(UiEventTopics::PROFILE_SESSIONS))
        .count();
    assert_eq!(
        list_notifications, 1,
        "one profile-sessions notify: {notifications:?}"
    );
}

#[tokio::test]
async fn an_invalid_patch_changes_nothing() {
    let state = state().await;
    seed(&state).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/api/sessions/{SESSION}"),
        Some(json!({"pinned": true, "folderId": "no-such-folder", "title": "changed?"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let session = state
        .db()
        .get_session(SESSION)
        .await
        .expect("db")
        .expect("row");
    assert_eq!(session.title.as_deref(), Some("Session"), "title unchanged");
    assert!(!session.pinned, "pin not applied");
}

#[tokio::test]
async fn a_queued_turn_appears_in_the_queue_and_deletes_by_id() {
    let state = state().await;
    seed(&state).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let (status, runtime) = call(
        &router,
        "POST",
        &format!("/api/sessions/{SESSION}/turns"),
        Some(json!({"content": "later", "mode": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "turn body: {runtime}");
    assert_eq!(
        runtime["queuedCount"].as_u64(),
        Some(1),
        "queued: {runtime}"
    );

    // A follow-up spawned by the turn may claim the queued entry, so the
    // list-and-delete half runs on a second session nothing dispatched on.
    let qsession = "q";
    seed_into(&state, qsession).await;
    state
        .runtime_service
        .enqueue(
            qsession,
            crate::session::PendingUserMessage {
                id: None,
                state: crate::session::PendingMessageState::default(),
                error: None,
                content: "kept".to_string(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                created_at: jiff::Timestamp::now().to_string(),
                delivery_mode: crate::session::PendingUserMessageMode::Queue,
            },
            crate::session::PendingUserMessageMode::Queue,
        )
        .await
        .expect("enqueue");

    let (status, queue) = call(
        &router,
        "GET",
        &format!("/api/sessions/{qsession}/queue"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = queue["entries"].as_array().expect("entries");
    let entry_id = entries
        .iter()
        .find(|entry| entry["content"].as_str() == Some("kept"))
        .expect("kept entry")["id"]
        .as_i64()
        .expect("id");

    let (status, body) = call(
        &router,
        "DELETE",
        &format!("/api/sessions/{qsession}/queue/{entry_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete entry {entry_id}: {body}");

    let (_, queue) = call(
        &router,
        "GET",
        &format!("/api/sessions/{qsession}/queue"),
        None,
    )
    .await;
    assert_eq!(queue["entries"].as_array().expect("entries").len(), 0);

    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/api/sessions/{qsession}/queue/99999"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn each_interaction_kind_reaches_its_approver() {
    let state = state().await;
    seed(&state).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    for kind in ["permission", "workspace"] {
        let (status, _) = call(
            &router,
            "POST",
            "/api/interactions/nope",
            Some(json!({"kind": kind, "approved": true})),
        )
        .await;
        assert!(
            status.is_client_error(),
            "unknown {kind} id is rejected like the old route: {status}"
        );
    }
    let (status, _) = call(
        &router,
        "POST",
        "/api/interactions/nope",
        Some(json!({"kind": "askUser", "answers": []})),
    )
    .await;
    assert!(
        status.is_client_error(),
        "unknown askUser id is rejected: {status}"
    );

    let (status, _) = call(
        &router,
        "POST",
        "/api/interactions/nope",
        Some(json!({"kind": "not-a-kind"})),
    )
    .await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn the_actions_route_dispatches_and_rejects_unknown_names() {
    let state = state().await;
    seed(&state).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let (status, _) = call(
        &router,
        "POST",
        &format!("/api/sessions/{SESSION}/actions/nope"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A real action: rewind needs a user entry to cut at.
    let row = state
        .db()
        .append_chat_message(SESSION, &ChatMessage::user("prompt"))
        .await
        .expect("user row");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/api/sessions/{SESSION}/actions/rewind"),
        Some(json!({"entryId": row.id.to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rewind body: {body}");
    assert_eq!(body.as_i64(), Some(i64::from(row.id)));
}
