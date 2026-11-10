//! `/api` answers negotiate `gzip` or `deflate` from `Accept-Encoding` —
//! every client's JSON shrinks on the wire — while the event stream is left
//! alone: a body of unknown length is never buffered for compression.

use std::io::Read;
use std::sync::Arc;

use flate2::read::GzDecoder;
use hyper::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
use skyzen::{Request, StatusCode};

use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;

/// `GET /api/config` answers the UI's full config: comfortably over the
/// middleware's 512-byte minimum.
const LARGE_JSON: &str = "/api/config";

async fn state() -> Arc<DaemonState> {
    // The home must outlive the state: `for_test` opens the database inside
    // it but does not keep the directory handle.
    let dir = tempfile::tempdir().expect("temp dir");
    let state = DaemonState::for_test(dir.path(), MayConfig::default())
        .await
        .expect("daemon state");
    std::mem::forget(dir);
    Arc::new(state)
}

fn get(uri: &str, accept_encoding: Option<&str>) -> Request {
    let mut builder = hyper::Request::builder().method("GET").uri(uri);
    if let Some(value) = accept_encoding {
        builder = builder.header(ACCEPT_ENCODING, value);
    }
    builder.body(skyzen::Body::empty()).expect("request")
}

async fn body_bytes(response: skyzen::Response) -> Vec<u8> {
    http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn a_client_accepting_gzip_gets_the_same_json_compressed() {
    let router = build_router(state().await, DaemonMode::Rest, None);

    let plain = router
        .go(get(LARGE_JSON, None))
        .await
        .expect("route answers");
    assert_eq!(plain.status(), StatusCode::OK);
    let plain = body_bytes(plain).await;
    assert!(
        plain.len() > 512,
        "{LARGE_JSON} must answer over the 512-byte compression minimum"
    );

    let compressed = router
        .go(get(LARGE_JSON, Some("gzip")))
        .await
        .expect("route answers");
    assert_eq!(compressed.status(), StatusCode::OK);
    assert_eq!(
        compressed
            .headers()
            .get(CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok()),
        Some("gzip")
    );
    let mut decoded = Vec::new();
    GzDecoder::new(body_bytes(compressed).await.as_slice())
        .read_to_end(&mut decoded)
        .expect("gzip body decodes");
    assert_eq!(decoded, plain);
}

#[tokio::test]
async fn a_client_without_accept_encoding_gets_plain_json() {
    let router = build_router(state().await, DaemonMode::Rest, None);
    let response = router
        .go(get(LARGE_JSON, None))
        .await
        .expect("route answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(CONTENT_ENCODING).is_none());
    let body = body_bytes(response).await;
    serde_json::from_slice::<serde_json::Value>(&body).expect("plain JSON");
}

#[tokio::test]
async fn the_event_stream_is_never_compressed() {
    let router = build_router(state().await, DaemonMode::Rest, None);
    let response = router
        .go(get("/api/events", Some("gzip")))
        .await
        .expect("route answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(CONTENT_ENCODING).is_none());
}
