//! `GET /api/sessions/{id}/events` starts after a cursor: a change between
//! reading it and subscribing arrives exactly once, nothing is replayed
//! without `after`, and a cursor the daemon cannot continue from gets a
//! `reset`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt as _;
use http_body_util::BodyDataStream;
use skyzen::{Request, StatusCode};

use super::server::{DaemonMode, build_router};
use super::session_events::{SessionChange, diff, format_cursor, parse_cursor};
use super::state::DaemonState;
use crate::config::MayConfig;
use crate::session::SessionRuntimeState;

const SESSION: &str = "s";

async fn state() -> Arc<DaemonState> {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
    // The home must outlive the state: `for_test` opens the database inside
    // it but does not keep the directory handle.
    let dir = tempfile::tempdir().expect("temp dir");
    let state = DaemonState::for_test(dir.path(), MayConfig::default())
        .await
        .expect("daemon state");
    std::mem::forget(dir);
    Arc::new(state)
}

fn request(method: &str, uri: &str) -> Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

async fn session(state: &DaemonState, id: &str) {
    state
        .db()
        .create_session(id, Some("Session"), Some("test-model"))
        .await
        .expect("session creates");
}

/// One parsed SSE event off the stream.
#[derive(Debug)]
struct Event {
    kind: String,
    data: String,
    id: Option<String>,
}

struct Stream {
    body: BodyDataStream<skyzen::Body>,
    buf: Vec<u8>,
}

impl Stream {
    async fn open(router: &skyzen::routing::Router, uri: &str) -> Self {
        let response = router
            .go(request("GET", uri))
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::OK);
        Self {
            body: BodyDataStream::new(response.into_body()),
            buf: Vec::new(),
        }
    }

    /// The next event; the test fails when none arrives in time.
    async fn next(&mut self) -> Event {
        tokio::time::timeout(Duration::from_secs(10), self.read())
            .await
            .expect("an event within 10s")
    }

    /// The next event within `within`, or `None` when the stream is quiet.
    async fn next_within(&mut self, within: Duration) -> Option<Event> {
        tokio::time::timeout(within, self.read()).await.ok()
    }

    async fn read(&mut self) -> Event {
        loop {
            if let Some(event) = self.take_buffered() {
                return event;
            }
            match self.body.try_next().await {
                Ok(Some(bytes)) => self.buf.extend_from_slice(&bytes),
                Ok(None) => panic!("the event stream ended"),
                Err(error) => panic!("the event stream errored: {error}"),
            }
        }
    }

    fn take_buffered(&mut self) -> Option<Event> {
        let end = self.buf.windows(2).position(|window| window == b"\n\n")?;
        let block: Vec<u8> = self.buf.drain(..end + 2).collect();
        let text = String::from_utf8(block).expect("events are utf-8");
        let mut event = Event {
            kind: String::new(),
            data: String::new(),
            id: None,
        };
        for line in text.lines() {
            if let Some(kind) = line.strip_prefix("event:") {
                event.kind = kind.trim().to_string();
            } else if let Some(data) = line.strip_prefix("data:") {
                event.data = data.trim().to_string();
            } else if let Some(id) = line.strip_prefix("id:") {
                event.id = Some(id.trim().to_string());
            }
        }
        Some(event)
    }
}

/// The `cursor` field `GET /api/sessions/{id}` reports.
async fn cursor_of(router: &skyzen::routing::Router, session_id: &str) -> String {
    let response = router
        .go(request("GET", &format!("/api/sessions/{session_id}")))
        .await
        .expect("the router answers");
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body collects")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("session json");
    json["cursor"].as_str().expect("a cursor").to_string()
}

fn publish_turn_frame(state: &DaemonState, session_id: &str, data: &str) {
    state
        .turn_hub
        .publish(session_id, "session_turn", data.into());
}

#[tokio::test]
async fn a_change_between_the_cursor_read_and_subscribing_arrives_exactly_once() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let cursor = cursor_of(&router, SESSION).await;
    state
        .runtime_service
        .set_state(SESSION, SessionRuntimeState::Running)
        .await
        .expect("runtime change");

    let mut stream = Stream::open(
        &router,
        &format!("/api/sessions/{SESSION}/events?after={cursor}"),
    )
    .await;
    let event = stream.next().await;
    assert_eq!(event.kind, "runtime.changed", "the first event");
    let data: serde_json::Value = serde_json::from_str(&event.data).expect("payload json");
    assert_eq!(data["sessionId"], SESSION);
    assert_eq!(data["runtime"]["state"], "running");
    assert_eq!(
        event.id.as_deref(),
        Some(&*format_cursor(state.session_events_epoch, 1))
    );
    assert!(
        stream
            .next_within(Duration::from_millis(400))
            .await
            .is_none(),
        "the change arrives once, not again"
    );
}

#[tokio::test]
async fn a_stream_without_after_replays_nothing_of_the_turn_in_progress() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    state.turn_hub.begin_turn(SESSION);
    publish_turn_frame(&state, SESSION, r#"{"seq":1}"#);
    publish_turn_frame(&state, SESSION, r#"{"seq":2}"#);

    let mut stream = Stream::open(&router, &format!("/api/sessions/{SESSION}/events")).await;
    publish_turn_frame(&state, SESSION, r#"{"seq":3}"#);

    let event = stream.next().await;
    assert_eq!(event.kind, "session_turn");
    assert_eq!(event.data, r#"{"seq":3}"#, "only live frames arrive");
    assert_eq!(
        event.id.as_deref(),
        Some(&*format_cursor(state.session_events_epoch, 3))
    );
    assert!(
        stream
            .next_within(Duration::from_millis(300))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_cursor_read_mid_turn_replays_the_turn_from_its_first_frame() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    state.turn_hub.begin_turn(SESSION);
    publish_turn_frame(&state, SESSION, r#"{"seq":1}"#);
    publish_turn_frame(&state, SESSION, r#"{"seq":2}"#);

    let cursor = cursor_of(&router, SESSION).await;
    let mut stream = Stream::open(
        &router,
        &format!("/api/sessions/{SESSION}/events?after={cursor}"),
    )
    .await;

    let first = stream.next().await;
    let second = stream.next().await;
    assert_eq!(
        (first.kind.as_str(), first.data.as_str()),
        ("session_turn", r#"{"seq":1}"#)
    );
    assert_eq!(
        (second.kind.as_str(), second.data.as_str()),
        ("session_turn", r#"{"seq":2}"#)
    );
    assert_eq!(
        [first.id.as_deref(), second.id.as_deref()],
        [
            Some(&*format_cursor(state.session_events_epoch, 1)),
            Some(&*format_cursor(state.session_events_epoch, 2))
        ],
        "in order, each with its cursor"
    );
}

#[tokio::test]
async fn a_cursor_from_another_epoch_gets_a_reset_then_live_events() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let mut stream = Stream::open(
        &router,
        &format!("/api/sessions/{SESSION}/events?after=1.2"),
    )
    .await;
    let reset = stream.next().await;
    assert_eq!(reset.kind, "reset");
    let data: serde_json::Value = serde_json::from_str(&reset.data).expect("payload json");
    assert_eq!(data["sessionId"], SESSION);

    publish_turn_frame(&state, SESSION, r#"{"seq":1}"#);
    let event = stream.next().await;
    assert_eq!(event.kind, "session_turn");
    assert_eq!(event.data, r#"{"seq":1}"#);
}

#[tokio::test]
async fn the_same_runtime_value_publishes_once_to_every_subscriber() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let mut first = Stream::open(&router, &format!("/api/sessions/{SESSION}/events")).await;
    let mut second = Stream::open(&router, &format!("/api/sessions/{SESSION}/events")).await;

    state
        .runtime_service
        .set_state(SESSION, SessionRuntimeState::Running)
        .await
        .expect("runtime change");
    state
        .runtime_service
        .set_state(SESSION, SessionRuntimeState::Running)
        .await
        .expect("the same runtime change");

    for (name, stream) in [("first", &mut first), ("second", &mut second)] {
        let event = stream.next().await;
        assert_eq!(event.kind, "runtime.changed", "{name} subscriber");
        assert!(
            stream
                .next_within(Duration::from_millis(400))
                .await
                .is_none(),
            "{name} subscriber sees the value once"
        );
    }
}

#[tokio::test]
async fn a_job_added_then_removed_upserts_then_removes() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let mut stream = Stream::open(&router, &format!("/api/sessions/{SESSION}/events")).await;

    let (registry, service) = aither_sandbox::job_registry::job_registry_channel();
    tokio::spawn(service.serve());
    state
        .session_background
        .add_job_registry(SESSION, registry.clone());
    registry
        .register(
            1234,
            "task-1",
            "exec-1",
            "sleep 60",
            aither_sandbox::TerminalMode::Sandboxed,
            None,
        )
        .await;
    state.publish_session_change(SESSION, SessionChange::Background);

    let upserted = stream.next().await;
    assert_eq!(upserted.kind, "job.upserted");
    let data: serde_json::Value = serde_json::from_str(&upserted.data).expect("payload json");
    assert_eq!(
        (data["sessionId"].as_str(), data["job"]["pid"].as_u64()),
        (Some(SESSION), Some(1234))
    );

    // The finished registry falls out of the snapshot once its completion
    // is taken and it is no longer the newest: the job leaves as a removal.
    registry.complete(1234, 0, None).await;
    let taken = registry.owe_completion("task-1", Ok(0)).await;
    drop(taken);
    let (next, next_service) = aither_sandbox::job_registry::job_registry_channel();
    tokio::spawn(next_service.serve());
    state.session_background.add_job_registry(SESSION, next);
    state.publish_session_change(SESSION, SessionChange::Background);

    let removed = stream.next().await;
    assert_eq!(removed.kind, "job.removed");
    let data: serde_json::Value = serde_json::from_str(&removed.data).expect("payload json");
    assert_eq!(
        (data["sessionId"].as_str(), data["pid"].as_u64()),
        (Some(SESSION), Some(1234))
    );
}

#[test]
fn a_cursor_roundtrips_and_rejects_another_epoch() {
    assert_eq!(parse_cursor(&format_cursor(42, 7), 42), Some(7));
    assert_eq!(parse_cursor(&format_cursor(42, 7), 41), None, "other epoch");
    assert_eq!(parse_cursor("garbage", 42), None);
    assert_eq!(parse_cursor("42", 42), None, "missing index");
    assert_eq!(parse_cursor("42.x", 42), None, "bad index");
}

#[test]
fn a_diff_reports_new_and_changed_items_then_removals() {
    let previous = vec![(1, "a"), (2, "b")];
    let current = vec![(1, "a"), (3, "c")];
    let (upserts, removed) = diff(&previous, &current, |item| &item.0);
    assert_eq!(upserts, vec![&(3, "c")], "the added item upserts");
    assert_eq!(removed, vec![&2], "the dropped key removes");

    let changed = vec![(1, "a2"), (2, "b")];
    let (upserts, removed) = diff(&previous, &changed, |item| &item.0);
    assert_eq!(upserts, vec![&(1, "a2")], "the changed item upserts");
    assert_eq!(removed, Vec::<&i32>::new());

    let (upserts, removed) = diff(&previous, &previous, |item| &item.0);
    assert!(
        upserts.is_empty() && removed.is_empty(),
        "equal sets are quiet"
    );
}

#[tokio::test]
async fn the_issues_route_answers_the_list_the_stream_reports() {
    let state = state().await;
    session(&state, SESSION).await;
    // `resolve_issue_store` canonicalizes the session's working directory.
    let sandbox =
        crate::session::SessionStore::new(crate::session::MayHomePaths::new(state.may_dir.clone()))
            .sandbox_dir(SESSION, None);
    std::fs::create_dir_all(&sandbox).expect("session sandbox dir");
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let response = router
        .go(request("GET", &format!("/api/sessions/{SESSION}/issues")))
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body collects")
        .to_bytes();
    let frame: serde_json::Value = serde_json::from_slice(&body).expect("issues json");
    assert_eq!(frame["sessionId"], SESSION);
    assert!(frame["issues"].is_array(), "the issues list");
}

#[tokio::test]
async fn a_removed_session_topic_is_rejected_on_the_shared_stream() {
    let state = state().await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let error = router
        .go(request("GET", "/api/events?topics=session_turn"))
        .await
        .expect_err("a removed topic name is unknown");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
}
