//! `GET /api/sessions/{id}/transcript` pagination and `POST .../rewind`
//! by transcript entry id.

use std::sync::Arc;

use jiff::Timestamp;
use serde_json::{Value, json};
use skyzen::StatusCode;

use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;
use crate::session::{ChatMessage, ToolEventRecord};

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

async fn seed(state: &DaemonState, messages: &[ChatMessage]) -> Vec<i32> {
    state
        .db()
        .create_session(SESSION, Some("Session"), Some("model"))
        .await
        .expect("session");
    let mut ids = Vec::new();
    for message in messages {
        let row = state
            .db()
            .append_chat_message(SESSION, message)
            .await
            .expect("append");
        ids.push(row.id);
    }
    ids
}

async fn page(router: &skyzen::routing::Router, query: &str) -> Value {
    let response = router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(format!("/api/sessions/{SESSION}/transcript{query}"))
            .body(skyzen::Body::empty())
            .expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&response.into_body().into_string().await.expect("body")).expect("json")
}

fn ids_of(page: &Value) -> Vec<String> {
    page["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["id"].as_str().expect("id").to_string())
        .collect()
}

fn sample_rows() -> Vec<ChatMessage> {
    let at = Timestamp::now();
    vec![
        ChatMessage::user("first"),
        ChatMessage::assistant("reply one", at),
        ChatMessage::user("second"),
        ChatMessage::assistant("reply two", at),
        ChatMessage::user("third"),
        ChatMessage::assistant("reply three", at),
    ]
}

#[tokio::test]
async fn a_transcript_returns_everything_without_a_limit() {
    let state = state().await;
    let row_ids = seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let page = page(&router, "").await;
    assert_eq!(
        ids_of(&page),
        row_ids.iter().map(i32::to_string).collect::<Vec<_>>()
    );
    assert_eq!(page["hasMore"], false);
    assert!(page["cursor"].is_string());
    assert_eq!(page["entries"][0]["kind"], "user");
    assert_eq!(page["entries"][0]["text"], "first");
}

#[tokio::test]
async fn a_limit_alone_returns_the_newest_entries() {
    let state = state().await;
    let row_ids = seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let page = page(&router, "?limit=2").await;
    assert_eq!(
        ids_of(&page),
        vec![row_ids[4].to_string(), row_ids[5].to_string()]
    );
    assert_eq!(page["hasMore"], true);
}

#[tokio::test]
async fn before_pages_back_and_paging_to_the_start_equals_the_full_list() {
    let state = state().await;
    let row_ids = seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let first = page(&router, "?limit=2").await;
    let second = page(&router, &format!("?limit=2&before={}", ids_of(&first)[0])).await;
    assert_eq!(
        ids_of(&second),
        vec![row_ids[2].to_string(), row_ids[3].to_string()]
    );
    assert_eq!(second["hasMore"], true);
    let third = page(&router, &format!("?limit=2&before={}", ids_of(&second)[0])).await;
    assert_eq!(
        ids_of(&third),
        vec![row_ids[0].to_string(), row_ids[1].to_string()]
    );
    assert_eq!(third["hasMore"], false);

    let all = page(&router, "").await;
    let mut paged = ids_of(&third);
    paged.extend(ids_of(&second));
    paged.extend(ids_of(&first));
    assert_eq!(paged, ids_of(&all));
}

#[tokio::test]
async fn rows_written_while_paging_neither_repeat_nor_disappear() {
    let state = state().await;
    let row_ids = seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let first = page(&router, "?limit=2").await;
    // A new turn lands between pages.
    let extra = seed_extra(&state).await;
    let older = page(&router, &format!("?limit=4&before={}", ids_of(&first)[0])).await;
    let mut paged = ids_of(&older);
    paged.extend(ids_of(&first));
    paged.extend(extra.iter().map(i32::to_string));
    let all = page(&router, "").await;
    assert_eq!(paged, ids_of(&all));
    assert_eq!(row_ids.iter().map(i32::to_string).collect::<Vec<_>>(), {
        let mut seen = paged[..row_ids.len()].to_vec();
        seen.sort_by_key(|s| s.parse::<i64>().unwrap());
        seen
    });
}

async fn seed_extra(state: &DaemonState) -> Vec<i32> {
    let mut ids = Vec::new();
    for message in [
        ChatMessage::user("fourth"),
        ChatMessage::assistant("reply four", Timestamp::now()),
    ] {
        let row = state
            .db()
            .append_chat_message(SESSION, &message)
            .await
            .expect("append");
        ids.push(row.id);
    }
    ids
}

#[tokio::test]
async fn an_unknown_before_is_a_bad_request() {
    let state = state().await;
    seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let response = router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(format!("/api/sessions/{SESSION}/transcript?before=nope"))
            .body(skyzen::Body::empty())
            .expect("request"))
        .await;
    if let Ok(r) = response {
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_tool_call_folds_start_end_and_stopped_rows_into_one_entry() {
    let state = state().await;
    let at = Timestamp::now();
    let start = ToolEventRecord::Start {
        id: "c1".into(),
        name: "terminal".into(),
        arguments: "{\"cmd\":\"ls\"}".into(),
    };
    let end = ToolEventRecord::End {
        id: "c1".into(),
        name: "terminal".into(),
        result: aither_core::llm::ToolResult::text("done!"),
    };
    let stopped = ToolEventRecord::Stopped {
        id: "c2".into(),
        name: "monitor".into(),
    };
    let start_stopped = ToolEventRecord::Start {
        id: "c2".into(),
        name: "monitor".into(),
        arguments: "{}".into(),
    };
    let rows = vec![
        ChatMessage::user("go"),
        ChatMessage::tool("c1", serde_json::to_string(&start).unwrap(), at),
        ChatMessage::tool("c2", serde_json::to_string(&start_stopped).unwrap(), at),
        ChatMessage::tool("c1", serde_json::to_string(&end).unwrap(), at),
        ChatMessage::tool("c2", serde_json::to_string(&stopped).unwrap(), at),
        ChatMessage::assistant("fin", at),
    ];
    seed(&state, &rows).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let page = page(&router, "").await;
    let entries = page["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[1]["kind"], "toolCall");
    assert_eq!(entries[1]["callId"], "c1");
    assert_eq!(entries[1]["status"], "done");
    assert_eq!(entries[1]["name"], "terminal");
    assert!(entries[1]["result"].is_object() || entries[1]["result"].is_string());
    assert_eq!(entries[2]["kind"], "toolCall");
    assert_eq!(entries[2]["callId"], "c2");
    assert_eq!(entries[2]["status"], "stopped");
}

async fn remaining_ids(state: &DaemonState) -> Vec<i32> {
    state
        .db()
        .get_messages(SESSION)
        .await
        .expect("rows")
        .iter()
        .map(|r| r.id)
        .collect()
}

#[tokio::test]
async fn rewind_by_entry_id_cuts_the_prompt_and_its_hidden_context() {
    let at = Timestamp::now();
    let hidden = ChatMessage {
        id: None,
        role: crate::session::MessageRole::HiddenContext,
        content: "secret".into(),
        attachments: Vec::new(),
        timestamp: Some(at),
        tool_call_id: None,
        stats: None,
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        translation: None,
        reading: None,
        thinking: None,
    };
    let rows = vec![
        ChatMessage::user("first"),
        ChatMessage::assistant("reply one", at),
        hidden,
        ChatMessage::user("second"),
        ChatMessage::assistant("reply two", at),
    ];

    let state_a = state().await;
    seed(&state_a, &rows).await;
    let router_a = build_router(state_a.clone(), DaemonMode::Rest, None);
    let full = page(&router_a, "").await;
    let user_entry = full["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["kind"] == "user" && e["text"] == "second")
        .expect("user entry");
    let entry_id = user_entry["id"].as_str().expect("id").to_string();
    router_a
        .go(hyper::Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/{SESSION}/actions/rewind"))
            .header("content-type", "application/json")
            .body(skyzen::Body::from(json!({"entryId": entry_id}).to_string()))
            .expect("request"))
        .await
        .expect("rewind");

    let state_b = state().await;
    let row_ids = seed(&state_b, &rows).await;
    assert_eq!(
        remaining_ids(&state_a).await,
        vec![row_ids[0], row_ids[1]],
        "the prompt and the hidden context before it are cut"
    );
    let router_b = build_router(state_b.clone(), DaemonMode::Rest, None);

    // A non-user entryId is a 400.
    let bad = router_b
        .go(hyper::Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/{SESSION}/actions/rewind"))
            .header("content-type", "application/json")
            .body(skyzen::Body::from(json!({"entryId": "999999"}).to_string()))
            .expect("request"))
        .await;
    if let Ok(r) = bad {
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn the_session_read_no_longer_embeds_messages() {
    let state = state().await;
    seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let response = router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(format!("/api/sessions/{SESSION}"))
            .body(skyzen::Body::empty())
            .expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_str(&response.into_body().into_string().await.expect("body"))
            .expect("json");
    assert!(body.get("messages").is_none(), "{body}");
    assert!(body.get("cursor").is_some(), "{body}");
}

#[tokio::test]
async fn the_history_get_is_gone_and_rewind_index_is_rejected() {
    let state = state().await;
    seed(&state, &sample_rows()).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let history = router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(format!("/api/sessions/{SESSION}/history"))
            .body(skyzen::Body::empty())
            .expect("request"))
        .await;
    if let Ok(r) = history {
        assert!(
            r.status() == StatusCode::NOT_FOUND || r.status() == StatusCode::METHOD_NOT_ALLOWED
        );
    }
    let rewind = router
        .go(hyper::Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/{SESSION}/actions/rewind"))
            .header("content-type", "application/json")
            .body(skyzen::Body::from(json!({"index": 2}).to_string()))
            .expect("request"))
        .await;
    if let Ok(r) = rewind {
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}
