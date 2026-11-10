//! `POST /api/sessions/{id}/attachments` stores raw bytes under the session's
//! `files/` and `GET /api/sessions/{id}/files/{path}` serves them back.

use std::sync::Arc;

use skyzen::StatusCode;

use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;

const SESSION: &str = "s";

async fn state() -> Arc<DaemonState> {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = DaemonState::for_test(dir.path(), MayConfig::default())
        .await
        .expect("daemon state");
    std::mem::forget(dir);
    Arc::new(state)
}

async fn session(state: &DaemonState, id: &str) {
    state
        .db()
        .create_session(id, Some("Session"), Some("test-model"))
        .await
        .expect("session creates");
}

#[tokio::test]
async fn an_uploaded_attachment_serves_its_bytes_with_their_content_type() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    let bytes = b"\x89PNG\r\n\x1a\nbytes".to_vec();
    let upload = hyper::Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{SESSION}/attachments?name=shot.png"))
        .header("content-type", "image/png")
        .body(skyzen::Body::from(bytes.clone()))
        .expect("request");
    let response = router.go(upload).await.expect("upload response");
    let status = response.status();
    let body = response.into_body().into_string().await.expect("body");
    assert_eq!(status, StatusCode::OK, "{body}");
    let response = hyper::Response::new(skyzen::Body::from(body));
    let record: serde_json::Value =
        serde_json::from_str(&response.into_body().into_string().await.expect("body"))
            .expect("json");
    let url = record["url"].as_str().expect("attachment url");
    assert_eq!(record["type"], "image");
    assert!(url.starts_with("/api/sessions/s/files/"), "{url}");

    let fetched = router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(url)
            .body(skyzen::Body::empty())
            .expect("request"))
        .await
        .expect("file response");
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(
        fetched
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        fetched
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(
        fetched.into_body().into_bytes().await.expect("bytes"),
        bytes
    );
}

#[tokio::test]
async fn a_file_path_escaping_files_is_rejected() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);

    for path in ["../secrets", "..%2Fsecrets", "%2e%2e/secrets"] {
        if let Ok(response) = router
            .go(hyper::Request::builder()
                .method("GET")
                .uri(format!("/api/sessions/{SESSION}/files/{path}"))
                .body(skyzen::Body::empty())
                .expect("request"))
            .await
        {
            assert!(
                !response.status().is_success(),
                "{path} got {}",
                response.status()
            );
        }
    }

    if let Ok(response) = router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(format!("/api/sessions/{SESSION}/files/nope.png"))
            .body(skyzen::Body::empty())
            .expect("request"))
        .await
    {
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
