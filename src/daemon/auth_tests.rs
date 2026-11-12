//! Each listener's access policy runs before any route or fallback. The
//! remote and direct listeners require an identity everywhere — config
//! reads, chat sends, approval responses, the event stream, unmatched paths
//! and the tunnel `Referer` fallback — and so does a non-loopback `--host`
//! bind. A loopback bind admits only loopback `Host` and `Origin` headers;
//! the unix socket admits its owner. The Tailscale identity accepts only a
//! listed `Tailscale-User-Login`, never a Funnel request.

use std::net::SocketAddr;
use std::sync::Arc;

use skyzen::extract::PeerAddr;
use skyzen::{Request, StatusCode};

use super::TcpRouters;
use super::auth::{
    LoopbackOnly, RequireIdentity, SocketOwner, TailnetLogin, TailscaleServeIdentitySource,
};
use super::remote_access::{parse_whois_login, whois_login};
use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::agent::tunnel_tool::TunnelRequest;
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
        RequireIdentity::new(vec![Box::new(source)]),
    )
}

fn local_router(state: &Arc<DaemonState>) -> skyzen::routing::Router {
    build_router(state.clone(), DaemonMode::Rest, LoopbackOnly)
}

/// The listener's address as a browser or curl names it in `Host`.
const LOCAL_HOST: &str = "127.0.0.1:8787";

fn request(method: &str, uri: &str) -> Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

/// A request as curl sends it to the loopback listener: a `Host`, no `Origin`.
fn local_request(method: &str, uri: &str) -> Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("Host", LOCAL_HOST)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

/// A request with the given `Host` and, when set, `Origin`.
fn browser_request(method: &str, uri: &str, host: &str, origin: Option<&str>) -> Request {
    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("Host", host);
    if let Some(origin) = origin {
        builder = builder.header("Origin", origin);
    }
    builder.body(skyzen::Body::empty()).expect("request builds")
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
    ("POST", "/api/interactions/some-id"),
    ("DELETE", "/api/sessions/some-id/turns/current"),
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
        let response = status(&router, local_request(method, uri)).await;
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
        RequireIdentity::new(vec![Box::new(source.whois_source())]),
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
    let mut req = tailscale_request("GET", "/api/health", "anyone@example.com");
    req.headers_mut()
        .insert("Host", LOCAL_HOST.parse().expect("host header"));
    let response = status(&router, req).await;
    assert_eq!(
        response,
        StatusCode::OK,
        "the local listener never checks identity"
    );
}

/// An HTTP server on loopback answering every request with `200 upstream`,
/// standing in for a session's dev server behind a tunnel.
async fn upstream() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind the upstream");
    let addr = listener.local_addr().expect("upstream address");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(
                hyper_util::rt::TokioIo::new(stream),
                hyper::service::service_fn(|_request| async {
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(
                        http_body_util::Full::new(bytes::Bytes::from_static(b"upstream")),
                    ))
                }),
            ));
        }
    });
    addr
}

/// A live tunnel of session `s` to [`upstream`], and the `Referer` of a page
/// served under it.
async fn tunnel_referer(state: &Arc<DaemonState>) -> String {
    state
        .db()
        .create_session("s", None, None)
        .await
        .expect("session");
    let tunnel = super::tunnel::handle_tunnel_request(
        state,
        TunnelRequest {
            session_id: "s".to_string(),
            url: upstream().await.to_string(),
        },
    )
    .await
    .expect("tunnel registers");
    format!("http://{LOCAL_HOST}{}", tunnel.path)
}

fn with_referer(mut request: Request, referer: &str) -> Request {
    request
        .headers_mut()
        .insert("Referer", referer.parse().expect("referer header"));
    request
}

#[tokio::test]
async fn no_identity_gets_401_on_matched_unmatched_and_tunnel_referer_paths() {
    let state = state().await;
    let referer = tunnel_referer(&state).await;
    let source = tailscale_source(&["owner@example.com"]);
    let routers = [
        ("remote", remote_router(&state, source.clone())),
        ("direct", direct_router(&state, &source)),
    ];
    for (listener, router) in &routers {
        for (case, req) in [
            ("a matched route", request("GET", "/api/health")),
            ("an unmatched path", request("GET", "/src/main.tsx")),
            (
                "an unmatched path with a tunnel Referer",
                with_referer(request("GET", "/src/main.tsx"), &referer),
            ),
        ] {
            assert_eq!(
                status(router, req).await,
                StatusCode::UNAUTHORIZED,
                "{case} on the {listener} listener needs an identity"
            );
        }
    }
}

#[tokio::test]
async fn an_identified_tunnel_referer_request_still_reaches_the_tunnel() {
    let state = state().await;
    let referer = tunnel_referer(&state).await;
    let router = remote_router(&state, tailscale_source(&["owner@example.com"]));
    let req = with_referer(
        tailscale_request("GET", "/src/main.tsx", "owner@example.com"),
        &referer,
    );
    assert_eq!(
        status(&router, req).await,
        StatusCode::OK,
        "the fallback proxies an identified request to the tunnel"
    );
}

fn tcp_routers(state: &Arc<DaemonState>) -> TcpRouters {
    let source = tailscale_source(&["owner@example.com"]);
    TcpRouters {
        state: state.clone(),
        loopback: Arc::new(local_router(state)),
        direct: Arc::new(direct_router(state, &source)),
    }
}

#[tokio::test]
async fn a_non_loopback_host_bind_serves_no_route_without_an_identity() {
    let state = state().await;
    let routers = tcp_routers(&state);
    for bind in ["0.0.0.0:8787", "[::]:8787", "192.168.1.20:8787"] {
        let addr: SocketAddr = bind.parse().expect("bind address");
        for (method, uri) in API_PATHS {
            assert_eq!(
                status(routers.router_for(addr), local_request(method, uri)).await,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} on a {bind} bind needs an identity"
            );
        }
    }
    let loopback: SocketAddr = "127.0.0.1:8787".parse().expect("bind address");
    assert_eq!(
        status(
            routers.router_for(loopback),
            local_request("GET", "/api/health")
        )
        .await,
        StatusCode::OK,
        "a loopback bind keeps serving the machine's user"
    );
}

#[tokio::test]
async fn the_loopback_listener_refuses_foreign_origins_and_non_loopback_hosts() {
    let state = state().await;
    let router = local_router(&state);
    let refused = [
        ("a foreign Origin", LOCAL_HOST, Some("https://evil.example")),
        ("a sandboxed frame's null Origin", LOCAL_HOST, Some("null")),
        (
            "an Origin under a loopback-looking name",
            LOCAL_HOST,
            Some("http://127.0.0.1.evil.example:8787"),
        ),
        (
            "an Origin under a localhost-looking name",
            LOCAL_HOST,
            Some("http://localhost.evil.example"),
        ),
        ("a non-http Origin", LOCAL_HOST, Some("file://localhost")),
        ("a rebinding Host", "rebind.evil.example:8787", None),
        (
            "a rebinding Host with its own Origin",
            "rebind.evil.example:8787",
            Some("http://rebind.evil.example:8787"),
        ),
        ("a LAN Host", "192.168.1.20:8787", None),
    ];
    for (case, host, origin) in refused {
        for (method, uri) in [
            ("POST", "/api/daemon/shutdown"),
            ("POST", "/api/sessions/delete-all"),
            ("GET", "/api/config"),
        ] {
            assert_eq!(
                status(&router, browser_request(method, uri, host, origin)).await,
                StatusCode::FORBIDDEN,
                "{case} must not reach {method} {uri}"
            );
        }
    }
    assert_eq!(
        status(&router, request("GET", "/api/health")).await,
        StatusCode::FORBIDDEN,
        "a request naming no Host is refused"
    );
    assert_eq!(
        status(
            &router,
            browser_request("GET", "/no/such/path", "rebind.evil.example", None)
        )
        .await,
        StatusCode::FORBIDDEN,
        "the policy runs before the 404"
    );
}

#[tokio::test]
async fn the_web_ui_the_tui_and_curl_keep_working() {
    let state = state().await;
    let router = local_router(&state);
    let admitted = [
        ("curl", LOCAL_HOST, None),
        (
            "the daemon-served web UI",
            "localhost:8787",
            Some("http://localhost:8787"),
        ),
        (
            "the web UI through the Vite proxy",
            LOCAL_HOST,
            Some("http://localhost:5173"),
        ),
        (
            "an IPv6 loopback page",
            "[::1]:8787",
            Some("http://[::1]:8787"),
        ),
        (
            "a .localhost name",
            "may.localhost:8787",
            Some("http://may.localhost:8787"),
        ),
        (
            "a 127/8 address",
            "127.0.0.2:8787",
            Some("https://127.0.0.2"),
        ),
    ];
    for (case, host, origin) in admitted {
        assert_eq!(
            status(&router, browser_request("GET", "/api/health", host, origin)).await,
            StatusCode::OK,
            "{case} reaches the API"
        );
    }
    // The TUI and one-shot runs use the unix socket, whose requests name no
    // meaningful Host.
    let socket = build_router(state.clone(), DaemonMode::Rest, SocketOwner);
    assert_eq!(
        status(&socket, request("GET", "/api/health")).await,
        StatusCode::OK,
        "the unix socket serves its owner"
    );
}
