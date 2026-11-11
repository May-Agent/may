//! `POST /api/sessions/{id}/attachments` stores raw bytes under the session's
//! `files/` and `GET /api/sessions/{id}/files/{path}` serves them back.

use std::sync::Arc;

use skyzen::StatusCode;

use super::auth::SocketOwner;
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
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);

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
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);

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

async fn get(router: &skyzen::routing::Router, uri: &str) -> skyzen::Response {
    router
        .go(hyper::Request::builder()
            .method("GET")
            .uri(uri)
            .body(skyzen::Body::empty())
            .expect("request"))
        .await
        .expect("response")
}

fn header<'a>(response: &'a skyzen::Response, name: &str) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

async fn write_session_file(state: &DaemonState, dir: &str, name: &str, bytes: &[u8]) {
    let paths = super::helpers::load_session_paths(state, SESSION)
        .await
        .expect("paths");
    let dir = match dir {
        "files" => paths.files_dir(),
        _ => paths.artifacts_dir(),
    };
    async_fs::create_dir_all(&dir).await.expect("mkdir");
    async_fs::write(dir.join(name), bytes).await.expect("write");
}

const NO_SCRIPT_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

#[tokio::test]
async fn session_html_and_svg_files_are_served_inert() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    write_session_file(&state, "files", "page.html", b"<script>alert(1)</script>").await;
    write_session_file(
        &state,
        "files",
        "pic.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>",
    )
    .await;

    for (name, content_type) in [
        ("page.html", "text/html; charset=utf-8"),
        ("pic.svg", "image/svg+xml; charset=utf-8"),
    ] {
        let response = get(&router, &format!("/api/sessions/{SESSION}/files/{name}")).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(header(&response, "content-type"), Some(content_type));
        assert_eq!(header(&response, "x-content-type-options"), Some("nosniff"));
        assert_eq!(
            header(&response, "content-security-policy"),
            Some(NO_SCRIPT_POLICY),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_session_image_keeps_its_type_and_gets_nosniff_only() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    write_session_file(&state, "files", "shot.png", b"\x89PNG\r\n\x1a\n").await;

    let response = get(&router, &format!("/api/sessions/{SESSION}/files/shot.png")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), Some("image/png"));
    assert_eq!(header(&response, "x-content-type-options"), Some("nosniff"));
    assert_eq!(header(&response, "content-security-policy"), None);
}

#[tokio::test]
async fn an_artifact_download_is_an_attachment_with_nosniff_and_a_sandbox() {
    let state = state().await;
    session(&state, SESSION).await;
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    write_session_file(&state, "artifacts", "page.html", b"<script>1</script>").await;

    let response = get(
        &router,
        &format!("/profile/-/session/{SESSION}/artifacts/page.html?download"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        header(&response, "content-disposition")
            .is_some_and(|value| value.starts_with("attachment; filename=\"page.html\"")),
        "{:?}",
        response.headers()
    );
    assert_eq!(header(&response, "x-content-type-options"), Some("nosniff"));
    assert_eq!(
        header(&response, "content-security-policy"),
        Some("sandbox allow-scripts")
    );
}
