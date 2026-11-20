//! The `/api/forge-auth` routes against a test daemon: listing, host
//! validation, configure persistence, logout and device-flow state.

use std::path::Path;
use std::sync::Arc;

use skyzen::StatusCode;

use super::auth::SocketOwner;
use super::forge_auth::{ForgeAuthStatusResponse, ForgeHostStatus, ForgeOauthPollResponse};
use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;

async fn state(home: &Path) -> Arc<DaemonState> {
    Arc::new(
        DaemonState::for_test(home, MayConfig::default())
            .await
            .expect("daemon state"),
    )
}

async fn call(
    state: &Arc<DaemonState>,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> Result<skyzen::Response, String> {
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);
    let request = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(match body {
            Some(body) => skyzen::Body::from(body.to_string()),
            None => skyzen::Body::empty(),
        })
        .expect("request");
    router.go(request).await.map_err(|error| error.to_string())
}

async fn call_ok(
    state: &Arc<DaemonState>,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> skyzen::Response {
    call(state, method, uri, body).await.expect("route answers")
}

async fn json<T: serde::de::DeserializeOwned>(response: skyzen::Response) -> T {
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

#[tokio::test]
async fn list_reports_the_default_hosts_unconfigured() {
    let home = tempfile::tempdir().expect("temp dir");
    let state = state(home.path()).await;

    let response = call_ok(&state, "GET", "/api/forge-auth", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let list: ForgeAuthStatusResponse = json(response).await;
    let github = list
        .hosts
        .iter()
        .find(|host| host.host == "github.com")
        .expect("github.com is listed");
    assert!(!github.configured);
    assert!(!github.authenticated);
    assert_eq!(github.kind.as_deref(), Some("github"));
    assert!(list.hosts.iter().any(|host| host.host == "gitlab.com"));
}

#[tokio::test]
async fn configure_persists_the_entry_and_logout_drops_it() {
    let home = tempfile::tempdir().expect("temp dir");
    let state = state(home.path()).await;

    let status: ForgeHostStatus = json(
        call_ok(
            &state,
            "POST",
            "/api/forge-auth/configure",
            Some(serde_json::json!({
                "host": "ghe.corp.test",
                "kind": "github",
                "mode": "cli",
            })),
        )
        .await,
    )
    .await;
    assert!(status.configured);
    assert_eq!(status.mode, Some(crate::forge::ForgeAuthMode::Cli));
    assert_eq!(status.kind.as_deref(), Some("github"));
    assert!(
        state.config_snapshot().forge.contains_key("ghe.corp.test"),
        "the entry landed in the live config"
    );
    let list: ForgeAuthStatusResponse =
        json(call_ok(&state, "GET", "/api/forge-auth", None).await).await;
    assert!(list.hosts.iter().any(|host| host.host == "ghe.corp.test"));

    let response = call_ok(
        &state,
        "POST",
        "/api/forge-auth/logout",
        Some(serde_json::json!({ "host": "ghe.corp.test" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !state.config_snapshot().forge.contains_key("ghe.corp.test"),
        "the entry left the live config"
    );
}

#[tokio::test]
async fn configure_rejects_a_bad_host_and_an_unknown_kind() {
    let home = tempfile::tempdir().expect("temp dir");
    let state = state(home.path()).await;

    for body in [
        serde_json::json!({ "host": "http://evil", "mode": "cli" }),
        serde_json::json!({ "host": "ghe.corp.test", "kind": "svn", "mode": "cli" }),
        serde_json::json!({ "host": "ghe.corp.test", "mode": "cli" }),
        serde_json::json!({ "host": "ghe.corp.test", "kind": "github", "mode": "oauth", "token": "x" }),
    ] {
        call(&state, "POST", "/api/forge-auth/configure", Some(body))
            .await
            .expect_err("a bad host, unknown kind, missing kind or a stray token is rejected");
    }
}

#[tokio::test]
async fn oauth_poll_requires_a_started_flow() {
    let home = tempfile::tempdir().expect("temp dir");
    let state = state(home.path()).await;

    let error = call(
        &state,
        "POST",
        "/api/forge-auth/oauth/poll",
        Some(serde_json::json!({ "host": "github.com", "device_code": "nope" })),
    )
    .await
    .expect_err("a poll needs a started device flow");
    assert!(error.contains("no device flow"), "{error}");
    let poll: ForgeOauthPollResponse = serde_json::from_value(serde_json::json!({
        "status": "pending",
        "retry_after_secs": 5,
    }))
    .expect("poll body shape");
    assert_eq!(
        poll.status,
        super::forge_auth::ForgeOauthPollStatus::Pending
    );
}
