//! The remote listener requires an identity everywhere — config reads, chat
//! sends, approval responses and the event stream — while the local loopback
//! listener keeps trusting the machine's own user. The Tailscale identity
//! accepts only a listed `Tailscale-User-Login`, never a Funnel request.

use std::sync::Arc;

use skyzen::extract::PeerAddr;
use skyzen::{Request, StatusCode};

use super::auth::{TailnetLogin, TailscaleServeIdentitySource};
use super::remote_access::{parse_whois_login, whois_login};
use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;

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

fn tailscale_source(allowed: &[&str]) -> TailscaleServeIdentitySource {
    TailscaleServeIdentitySource::new(allowed.iter().map(|s| (*s).to_string()).collect())
}

fn remote_router(
    state: &Arc<DaemonState>,
    source: TailscaleServeIdentitySource,
) -> skyzen::routing::Router {
    build_router(
        state.clone(),
        DaemonMode::Rest,
        Some(vec![Box::new(source)]),
    )
}

fn local_router(state: &Arc<DaemonState>) -> skyzen::routing::Router {
    build_router(state.clone(), DaemonMode::Rest, None)
}

fn request(method: &str, uri: &str) -> Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

fn tailscale_request(method: &str, uri: &str, login: &str) -> Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("Tailscale-User-Login", login)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

/// The status a request gets through `router.go` — `Ok` carries the route's
/// status, `Err` the error's HTTP status.
async fn status(router: &skyzen::routing::Router, request: Request) -> StatusCode {
    match router.go(request).await {
        Ok(response) => response.status(),
        Err(error) => skyzen::error_response(&*error).status(),
    }
}

const API_PATHS: [(&str, &str); 5] = [
    ("GET", "/api/health"),
    ("GET", "/api/config"),
    ("POST", "/api/chat/enqueue"),
    ("POST", "/api/permissions/respond"),
    ("GET", "/api/events"),
];

#[tokio::test]
async fn the_remote_listener_rejects_every_unauthenticated_api_and_event_request() {
    let state = state().await;
    let router = remote_router(&state, tailscale_source(&["owner@example.com"]));
    for (method, uri) in API_PATHS {
        let response = status(&router, request(method, uri)).await;
        assert_eq!(
            response,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} should be 401 without an identity"
        );
    }
}

#[tokio::test]
async fn the_local_listener_serves_them_without_identity() {
    let state = state().await;
    let router = local_router(&state);
    for (method, uri) in API_PATHS {
        let response = status(&router, request(method, uri)).await;
        assert_ne!(
            response,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must not 401 on the local listener"
        );
    }
}

#[tokio::test]
async fn a_loopback_peer_on_the_remote_listener_is_not_exempt() {
    let state = state().await;
    let router = remote_router(&state, tailscale_source(&["owner@example.com"]));
    let mut req = request("GET", "/api/health");
    req.extensions_mut()
        .insert(PeerAddr("127.0.0.1:43210".parse().expect("peer")));
    assert_eq!(
        status(&router, req).await,
        StatusCode::UNAUTHORIZED,
        "a loopback connection is still remote and still needs an identity"
    );
}

#[tokio::test]
async fn a_listed_tailscale_login_is_accepted() {
    let state = state().await;
    let router = remote_router(&state, tailscale_source(&["owner@example.com"]));
    let response = status(
        &router,
        tailscale_request("GET", "/api/health", "owner@example.com"),
    )
    .await;
    assert_ne!(
        response,
        StatusCode::UNAUTHORIZED,
        "a listed Tailscale-User-Login passes the identity layer"
    );
}

#[tokio::test]
async fn an_unlisted_tailscale_login_is_rejected() {
    let state = state().await;
    let router = remote_router(&state, tailscale_source(&["owner@example.com"]));
    assert_eq!(
        status(
            &router,
            tailscale_request("GET", "/api/health", "stranger@example.com"),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
}

#[tokio::test]
async fn a_funnel_request_needs_a_session_not_its_header() {
    let state = state().await;
    let router = remote_router(&state, tailscale_source(&["owner@example.com"]));
    let req = hyper::Request::builder()
        .method("GET")
        .uri("/api/health")
        .header("Tailscale-User-Login", "owner@example.com")
        .header("Tailscale-Funnel-Request", "?1")
        .body(skyzen::Body::empty())
        .expect("request builds");
    assert_eq!(
        status(&router, req).await,
        StatusCode::UNAUTHORIZED,
        "a Funnel request is not a tailnet identity"
    );
}

#[tokio::test]
async fn a_direct_bind_rejects_even_a_listed_tailscale_login() {
    // A non-loopback `[daemon] listen` address is remote access with no
    // trusted source yet: tailscale serve's header is only trustworthy on
    // the remote listener its proxy connects to, never on a direct bind.
    let state = state().await;
    let router = direct_router(&state, &tailscale_source(&["owner@example.com"]));
    assert_eq!(
        status(
            &router,
            tailscale_request("GET", "/api/health", "owner@example.com"),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "a header that only tailscale serve sets is meaningless on a direct bind"
    );
}

const WHOIS_OWNER: &str = include_str!("fixtures/whois_owner.json");
const WHOIS_GUEST: &str = include_str!("fixtures/whois_guest.json");
const WHOIS_TAGGED: &str = include_str!("fixtures/whois_tagged.json");

fn direct_router(
    state: &Arc<DaemonState>,
    source: &TailscaleServeIdentitySource,
) -> skyzen::routing::Router {
    build_router(
        state.clone(),
        DaemonMode::Rest,
        Some(vec![Box::new(source.whois_source())]),
    )
}

/// A request as the accept path delivers it on a direct bind: the login the
/// connection's `WhoIs` resolved, parsed from a CLI fixture.
fn whois_request(fixture: &str) -> Request {
    let mut req = request("GET", "/api/health");
    if let Some(login) = parse_whois_login(fixture).expect("fixture parses") {
        req.extensions_mut().insert(login);
    }
    req
}

#[test]
fn whois_output_yields_the_user_login_and_none_for_a_tagged_node() {
    assert_eq!(
        parse_whois_login(WHOIS_OWNER).expect("parses"),
        Some(TailnetLogin("owner@example.com".into()))
    );
    assert_eq!(parse_whois_login(WHOIS_TAGGED).expect("parses"), None);
    assert!(parse_whois_login("not json").is_err());
}

#[tokio::test]
async fn a_direct_bind_accepts_the_node_owner_and_rejects_others() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let router = direct_router(&state, &source);
    assert_ne!(
        status(&router, whois_request(WHOIS_OWNER)).await,
        StatusCode::UNAUTHORIZED,
        "the node owner's WhoIs login is accepted"
    );
    assert_eq!(
        status(&router, whois_request(WHOIS_GUEST)).await,
        StatusCode::UNAUTHORIZED,
        "a shared-device user is not accepted unless listed"
    );
}

#[tokio::test]
async fn a_listed_shared_device_user_is_accepted_through_the_live_allow_list() {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let router = direct_router(&state, &source);
    assert_eq!(
        status(&router, whois_request(WHOIS_GUEST)).await,
        StatusCode::UNAUTHORIZED
    );
    source.set_allowed(
        ["owner@example.com", "guest@example.com"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    );
    assert_ne!(
        status(&router, whois_request(WHOIS_GUEST)).await,
        StatusCode::UNAUTHORIZED,
        "adding the login in settings applies to the direct bind live"
    );
}

#[tokio::test]
async fn a_tagged_node_is_rejected_even_when_its_pseudo_login_is_listed() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com", "tagged-devices"]);
    let router = direct_router(&state, &source);
    assert_eq!(
        status(&router, whois_request(WHOIS_TAGGED)).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_failed_whois_leaves_the_connection_unattributed() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let router = direct_router(&state, &source);
    let resolved = whois_login("/nonexistent/tailscale", "100.64.0.10".parse().expect("ip")).await;
    assert_eq!(resolved, None, "a CLI that cannot run resolves nobody");
    assert_eq!(
        status(&router, request("GET", "/api/health")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn whois_is_ignored_on_the_serve_router_and_the_header_on_the_direct_one() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let serve = remote_router(&state, source.clone());
    assert_eq!(
        status(&serve, whois_request(WHOIS_OWNER)).await,
        StatusCode::UNAUTHORIZED,
        "a resolved connection login means nothing behind tailscale serve"
    );
    let direct = direct_router(&state, &source);
    assert_eq!(
        status(
            &direct,
            tailscale_request("GET", "/api/health", "owner@example.com")
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_local_listener_ignores_tailscale_headers() {
    let state = state().await;
    let router = local_router(&state);
    let response = status(
        &router,
        tailscale_request("GET", "/api/health", "anyone@example.com"),
    )
    .await;
    assert_ne!(
        response,
        StatusCode::UNAUTHORIZED,
        "the local listener never checks identity"
    );
}
