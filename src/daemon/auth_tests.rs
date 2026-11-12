//! Each listener's access policy runs before any route or fallback. The
//! remote and direct listeners require an identity everywhere — config
//! reads, chat sends, approval responses, the event stream, unmatched paths
//! and the tunnel `Referer` fallback — and so does a non-loopback `--host`
//! bind — and refuse an identified request whose `Origin` is not the origin
//! it was addressed to. A loopback bind admits only loopback `Host` and
//! `Origin` headers; the unix socket admits its owner. The Tailscale identity
//! accepts only a listed `Tailscale-User-Login`, never a Funnel request.

use std::net::SocketAddr;
use std::sync::Arc;

use skyzen::extract::PeerAddr;
use skyzen::{Request, StatusCode};

use super::TcpRouters;
use super::auth::{
    DirectListener, LocalAddr, LoopbackOnly, NodeNames, NodeNamesCache, NodeNamesSource,
    RequireIdentity, SocketOwner, TailnetLogin, TailscaleServeIdentitySource,
};
use super::remote_access::{parse_node_names, parse_whois_login, whois_login};
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
            with_host(
                tailscale_request("GET", "/api/health", "owner@example.com"),
                DIRECT_HOST
            ),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "a header that only tailscale serve sets is meaningless on a direct bind"
    );
}

const WHOIS_OWNER: &str = include_str!("fixtures/whois_owner.json");
const WHOIS_GUEST: &str = include_str!("fixtures/whois_guest.json");
const WHOIS_TAGGED: &str = include_str!("fixtures/whois_tagged.json");

/// The node's names as `tailscale status --json` reports them for the test
/// node: the addresses and `MagicDNS` name the direct tests address it by.
fn node_names() -> NodeNames {
    NodeNames::new(
        [
            "100.64.0.10".parse().expect("ipv4"),
            "fd7a:115c:a1e0::10".parse().expect("ipv6"),
        ],
        "mac.tail1234.ts.net.",
    )
}

/// A stand-in for the `tailscale status` read: answers what the test set and
/// counts its reads.
#[derive(Clone)]
struct StubNodeNames {
    answer: Arc<std::sync::Mutex<Result<NodeNames, String>>>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl StubNodeNames {
    fn new(answer: Result<NodeNames, String>) -> Self {
        Self {
            answer: Arc::new(std::sync::Mutex::new(answer)),
            reads: Arc::default(),
        }
    }

    fn set(&self, answer: Result<NodeNames, String>) {
        *self.answer.lock().expect("stub lock") = answer;
    }

    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl NodeNamesSource for StubNodeNames {
    fn read(&self) -> impl Future<Output = Result<NodeNames, String>> + Send {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(self.answer.lock().expect("stub lock").clone())
    }
}

fn direct_router_with(
    state: &Arc<DaemonState>,
    source: &TailscaleServeIdentitySource,
    names: &StubNodeNames,
) -> skyzen::routing::Router {
    let known = names
        .answer
        .lock()
        .expect("stub lock")
        .clone()
        .unwrap_or_default();
    build_router(
        state.clone(),
        DaemonMode::Rest,
        DirectListener::new(
            NodeNamesCache::with_names(names.clone(), known),
            RequireIdentity::new(vec![Box::new(source.whois_source())]),
        ),
    )
}

/// The direct listener of the test node, whose names are known up front.
fn direct_router(
    state: &Arc<DaemonState>,
    source: &TailscaleServeIdentitySource,
) -> skyzen::routing::Router {
    direct_router_with(state, source, &StubNodeNames::new(Ok(node_names())))
}

/// `request` addressed with `host`.
fn with_host(mut request: Request, host: &str) -> Request {
    request
        .headers_mut()
        .insert("Host", host.parse().expect("host header"));
    request
}

/// A request as the accept path delivers it on a direct bind: the login the
/// connection's `WhoIs` resolved, parsed from a CLI fixture.
fn whois_request(fixture: &str) -> Request {
    let mut req = with_host(request("GET", "/api/health"), DIRECT_HOST);
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
        status(
            &router,
            with_host(request("GET", "/api/health"), DIRECT_HOST)
        )
        .await,
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
            with_host(
                tailscale_request("GET", "/api/health", "owner@example.com"),
                DIRECT_HOST
            )
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
        .remote_access_manager
        .set_local_listener_addr(LOCAL_HOST.parse().expect("local listener address"));
    let tunnel = open_tunnel(state).await;
    format!("http://{LOCAL_HOST}{}", tunnel.path)
}

/// Opens a tunnel of session `s` to [`upstream`].
async fn open_tunnel(state: &Arc<DaemonState>) -> crate::agent::tunnel_tool::TunnelResult {
    state
        .db()
        .create_session("s", None, None)
        .await
        .expect("session");
    super::tunnel::handle_tunnel_request(
        state,
        TunnelRequest {
            session_id: "s".to_string(),
            url: upstream().await.to_string(),
        },
    )
    .await
    .expect("tunnel registers")
}

#[tokio::test]
async fn a_tunnels_local_url_is_served_by_the_local_listener_on_its_bound_port() {
    let state = state().await;
    // A listener bound to port 0, as the daemon's may be: the URL must carry
    // the port it actually got, not a configured one.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the local listener");
    let bound = listener.local_addr().expect("local listener address");
    state.remote_access_manager.set_local_listener_addr(bound);
    let tunnel = open_tunnel(&state).await;
    assert_eq!(
        tunnel.local_url,
        format!("http://127.0.0.1:{}{}", bound.port(), tunnel.path)
    );
    let uri: hyper::Uri = tunnel.local_url.parse().expect("local url parses");
    let host = uri
        .authority()
        .expect("local url has an authority")
        .as_str();
    let path = uri.path();
    let router = local_router(&state);
    assert_eq!(
        status(&router, browser_request("GET", path, host, None)).await,
        StatusCode::OK,
        "the local URL needs no identity on the local listener"
    );
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
            (
                "a matched route",
                with_host(request("GET", "/api/health"), DIRECT_HOST),
            ),
            (
                "an unmatched path",
                with_host(request("GET", "/src/main.tsx"), DIRECT_HOST),
            ),
            (
                "an unmatched path with a tunnel Referer",
                with_referer(
                    with_host(request("GET", "/src/main.tsx"), DIRECT_HOST),
                    &referer,
                ),
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

/// The node's `MagicDNS` name, the `Host` a browser sends through `tailscale
/// serve --https=443`.
const SERVE_HOST: &str = "mac.tail1234.ts.net";
/// The web UI's origin through `tailscale serve --https=443`.
const SERVE_ORIGIN: &str = "https://mac.tail1234.ts.net";
/// A direct bind's tailnet address, as a browser names it in `Host`.
const DIRECT_HOST: &str = "100.64.0.10:8787";
/// The web UI's origin on that direct bind.
const DIRECT_ORIGIN: &str = "http://100.64.0.10:8787";

/// An identified browser request as `tailscale serve` forwards it after
/// terminating TLS: the browser's `Host` and `Origin` unchanged, the
/// forwarding headers and the listed login added.
fn serve_request(method: &str, uri: &str, host: &str, origin: Option<&str>) -> Request {
    let mut req = browser_request(method, uri, host, origin);
    let headers = req.headers_mut();
    headers.insert(
        "Tailscale-User-Login",
        "owner@example.com".parse().expect("login header"),
    );
    headers.insert("X-Forwarded-Host", host.parse().expect("forwarded host"));
    headers.insert(
        "X-Forwarded-Proto",
        "https".parse().expect("forwarded proto"),
    );
    req
}

/// An identified browser request on a direct bind: the login the
/// connection's `WhoIs` resolved, and the browser's `Host` and `Origin`.
fn direct_request(method: &str, uri: &str, host: &str, origin: Option<&str>) -> Request {
    let mut req = browser_request(method, uri, host, origin);
    req.extensions_mut()
        .insert(TailnetLogin("owner@example.com".into()));
    req
}

#[tokio::test]
async fn an_identified_request_from_a_foreign_or_null_origin_gets_403_on_every_path() {
    let state = state().await;
    let referer = tunnel_referer(&state).await;
    let source = tailscale_source(&["owner@example.com"]);
    let remote = remote_router(&state, source.clone());
    let direct = direct_router(&state, &source);
    let paths = [
        ("a matched route", "POST", "/api/daemon/shutdown", None),
        ("a matched route", "POST", "/api/sessions/delete-all", None),
        ("a matched route", "GET", "/api/config", None),
        ("an unmatched path", "GET", "/src/main.tsx", None),
        (
            "the tunnel fallback",
            "GET",
            "/src/main.tsx",
            Some(referer.as_str()),
        ),
    ];
    for origin in ["https://evil.example", "null"] {
        for (case, method, uri, referer) in paths {
            for (listener, router, req) in [
                (
                    "remote",
                    &remote,
                    serve_request(method, uri, SERVE_HOST, Some(origin)),
                ),
                (
                    "direct",
                    &direct,
                    direct_request(method, uri, DIRECT_HOST, Some(origin)),
                ),
            ] {
                let req = match referer {
                    Some(referer) => with_referer(req, referer),
                    None => req,
                };
                assert_eq!(
                    status(router, req).await,
                    StatusCode::FORBIDDEN,
                    "{case} ({method} {uri}) on the {listener} listener must refuse Origin {origin}"
                );
            }
        }
    }
}

#[tokio::test]
async fn only_the_exact_addressed_origin_passes() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let remote = remote_router(&state, source.clone());
    let direct = direct_router(&state, &source);
    let remote_refused = [
        (
            "plain http on the served name",
            "http://mac.tail1234.ts.net",
        ),
        ("another port", "https://mac.tail1234.ts.net:8443"),
        ("another tailnet node", "https://other.tail1234.ts.net"),
        (
            "a name under the served one",
            "https://mac.tail1234.ts.net.evil.example",
        ),
        ("a non-http Origin", "file://mac.tail1234.ts.net"),
    ];
    for (case, origin) in remote_refused {
        assert_eq!(
            status(
                &remote,
                serve_request("POST", "/api/daemon/shutdown", SERVE_HOST, Some(origin))
            )
            .await,
            StatusCode::FORBIDDEN,
            "{case} is foreign to the remote listener"
        );
    }
    let direct_refused = [
        ("another port", "http://100.64.0.10:5173"),
        ("another tailnet address", "http://100.64.0.11:8787"),
        ("https on a plain-http bind", "https://100.64.0.10:8787"),
    ];
    for (case, origin) in direct_refused {
        assert_eq!(
            status(
                &direct,
                direct_request("POST", "/api/daemon/shutdown", DIRECT_HOST, Some(origin))
            )
            .await,
            StatusCode::FORBIDDEN,
            "{case} is foreign to the direct listener"
        );
    }
    // A direct peer reaches the bind without a proxy, so a forwarding header
    // it sends is its own and does not change the addressed scheme.
    let mut forged = direct_request(
        "POST",
        "/api/daemon/shutdown",
        DIRECT_HOST,
        Some("https://100.64.0.10:8787"),
    );
    forged.headers_mut().insert(
        "X-Forwarded-Proto",
        "https".parse().expect("forwarded proto"),
    );
    assert_eq!(
        status(&direct, forged).await,
        StatusCode::FORBIDDEN,
        "the direct listener never reads X-Forwarded-Proto"
    );
    // With no authority there is no origin the request was addressed to.
    let mut no_host = tailscale_request("POST", "/api/daemon/shutdown", "owner@example.com");
    no_host
        .headers_mut()
        .insert("Origin", SERVE_ORIGIN.parse().expect("origin header"));
    assert_eq!(
        status(&remote, no_host).await,
        StatusCode::FORBIDDEN,
        "an Origin with no authority to match is foreign"
    );
}

#[tokio::test]
async fn a_foreign_origin_without_an_identity_still_gets_401() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let remote = remote_router(&state, source.clone());
    let direct = direct_router(&state, &source);
    let mut unidentified = serve_request(
        "GET",
        "/api/health",
        SERVE_HOST,
        Some("https://evil.example"),
    );
    unidentified.headers_mut().remove("Tailscale-User-Login");
    assert_eq!(
        status(&remote, unidentified).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            &direct,
            browser_request(
                "GET",
                "/api/health",
                DIRECT_HOST,
                Some("https://evil.example")
            )
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_served_web_ui_and_identified_clients_without_origin_keep_working() {
    let state = state().await;
    let referer = tunnel_referer(&state).await;
    let source = tailscale_source(&["owner@example.com"]);
    let remote = remote_router(&state, source.clone());
    let direct = direct_router(&state, &source);

    let mut plain_serve = serve_request(
        "GET",
        "/api/health",
        SERVE_HOST,
        Some("http://mac.tail1234.ts.net"),
    );
    plain_serve.headers_mut().remove("X-Forwarded-Proto");
    let admitted = [
        (
            "the web UI through tailscale serve",
            &remote,
            serve_request("GET", "/api/health", SERVE_HOST, Some(SERVE_ORIGIN)),
        ),
        (
            "the web UI on another serve port",
            &remote,
            serve_request(
                "GET",
                "/api/health",
                "mac.tail1234.ts.net:8443",
                Some("https://mac.tail1234.ts.net:8443"),
            ),
        ),
        ("the web UI through plain-http serve", &remote, plain_serve),
        (
            "a client without Origin through tailscale serve",
            &remote,
            serve_request("GET", "/api/health", SERVE_HOST, None),
        ),
        (
            "a tunnel page through tailscale serve",
            &remote,
            with_referer(
                serve_request("GET", "/src/main.tsx", SERVE_HOST, Some(SERVE_ORIGIN)),
                &referer,
            ),
        ),
        (
            "the web UI on a direct bind",
            &direct,
            direct_request("GET", "/api/health", DIRECT_HOST, Some(DIRECT_ORIGIN)),
        ),
        (
            "the web UI on a direct bind by MagicDNS name",
            &direct,
            direct_request(
                "GET",
                "/api/health",
                "mac.tail1234.ts.net:8787",
                Some("http://mac.tail1234.ts.net:8787"),
            ),
        ),
        (
            "the web UI on a direct IPv6 bind",
            &direct,
            direct_request(
                "GET",
                "/api/health",
                "[fd7a:115c:a1e0::10]:8787",
                Some("http://[fd7a:115c:a1e0::10]:8787"),
            ),
        ),
        (
            "a client without Origin on a direct bind",
            &direct,
            direct_request("GET", "/api/health", DIRECT_HOST, None),
        ),
        (
            "a tunnel page on a direct bind",
            &direct,
            with_referer(
                direct_request("GET", "/src/main.tsx", DIRECT_HOST, Some(DIRECT_ORIGIN)),
                &referer,
            ),
        ),
    ];
    for (case, router, req) in admitted {
        assert_eq!(
            status(router, req).await,
            StatusCode::OK,
            "{case} is served"
        );
    }
    for (listener, router, req) in [
        (
            "remote",
            &remote,
            serve_request("GET", "/no/such/path", SERVE_HOST, Some(SERVE_ORIGIN)),
        ),
        (
            "direct",
            &direct,
            direct_request("GET", "/no/such/path", DIRECT_HOST, Some(DIRECT_ORIGIN)),
        ),
    ] {
        assert_eq!(
            status(router, req).await,
            StatusCode::NOT_FOUND,
            "a same-origin request to an unmatched path on the {listener} listener reaches the 404"
        );
    }
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
                status(
                    routers.router_for(addr),
                    browser_request(method, uri, DIRECT_HOST, None)
                )
                .await,
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

const STATUS_SELF: &str = include_str!("fixtures/status_self.json");

#[test]
fn status_output_yields_the_nodes_addresses_and_lowercase_name() {
    assert_eq!(
        parse_node_names(STATUS_SELF).expect("parses"),
        node_names(),
        "Self.TailscaleIPs and Self.DNSName without its trailing dot, lowercased"
    );
    assert!(parse_node_names("not json").is_err());
    assert!(
        parse_node_names(r#"{"BackendState": "NoState"}"#).is_err(),
        "a status without a Self node names nothing"
    );
}

#[tokio::test]
async fn the_direct_listener_refuses_a_foreign_host_even_when_identified() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let direct = direct_router(&state, &source);
    // `rebind.example` re-pointed at the node's address: the peer's `WhoIs`
    // resolves the user's own device and the browser's `Origin` matches.
    for host in [
        "rebind.example:8787",
        "rebind.example",
        "100.64.0.11:8787",
        "mac.tail1234.ts.net.evil.example:8787",
        "evil.example.mac.tail1234.ts.net",
        "localhost:8787",
        "[fd7a:115c:a1e0::11]:8787",
    ] {
        let origin = format!("http://{host}");
        assert_eq!(
            status(
                &direct,
                direct_request("GET", "/api/health", host, Some(origin.as_str()))
            )
            .await,
            StatusCode::FORBIDDEN,
            "an identified request to {host} is refused"
        );
        assert_eq!(
            status(
                &direct,
                browser_request("GET", "/api/health", host, Some(origin.as_str()))
            )
            .await,
            StatusCode::FORBIDDEN,
            "a request to {host} is refused before its identity is asked for"
        );
    }
    assert_eq!(
        status(
            &direct,
            with_host(request("GET", "/api/health"), "rebind.example:8787")
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let mut no_host = request("GET", "/api/health");
    no_host
        .extensions_mut()
        .insert(TailnetLogin("owner@example.com".into()));
    assert_eq!(
        status(&direct, no_host).await,
        StatusCode::FORBIDDEN,
        "a request naming no host is refused"
    );
}

#[tokio::test]
async fn the_direct_listener_answers_requests_addressed_to_the_node() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let direct = direct_router(&state, &source);
    for host in [
        "100.64.0.10",
        "100.64.0.10:8787",
        "[fd7a:115c:a1e0::10]:8787",
        "[FD7A:115C:A1E0:0:0:0:0:10]:8787",
        "mac.tail1234.ts.net",
        "mac.tail1234.ts.net:8787",
        "MAC.Tail1234.TS.net",
        "Mac.Tail1234.Ts.Net:8787",
        "mac.tail1234.ts.net.:8787",
    ] {
        assert_eq!(
            status(&direct, direct_request("GET", "/api/health", host, None)).await,
            StatusCode::OK,
            "a request addressed to {host} is served"
        );
    }
}

#[tokio::test]
async fn the_direct_listener_answers_the_address_it_is_bound_to() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let direct = direct_router(&state, &source);
    let bound = |req: Request, local: &str| {
        let mut req = req;
        req.extensions_mut()
            .insert(LocalAddr(local.parse().expect("local address")));
        req
    };
    assert_eq!(
        status(
            &direct,
            bound(
                direct_request("GET", "/api/health", "192.168.1.20:8787", None),
                "192.168.1.20:8787"
            )
        )
        .await,
        StatusCode::OK,
        "the address the connection arrived on names the node"
    );
    assert_eq!(
        status(
            &direct,
            bound(
                direct_request("GET", "/api/health", "[::1]:8787", None),
                "[::1]:8787"
            )
        )
        .await,
        StatusCode::OK,
        "so does an IPv6 bound address"
    );
    assert_eq!(
        status(
            &direct,
            bound(
                direct_request("GET", "/api/health", "192.168.1.21:8787", None),
                "192.168.1.20:8787"
            )
        )
        .await,
        StatusCode::FORBIDDEN,
        "another address does not"
    );
    assert_eq!(
        status(
            &direct,
            direct_request("GET", "/api/health", "192.168.1.20:8787", None)
        )
        .await,
        StatusCode::FORBIDDEN,
        "an address the node was never reported with is foreign without a bound address"
    );
    assert_eq!(
        status(
            &direct,
            bound(
                direct_request("GET", "/api/health", "rebind.example:8787", None),
                "192.168.1.20:8787"
            )
        )
        .await,
        StatusCode::FORBIDDEN,
        "a name is never the bound address"
    );
}

#[tokio::test]
async fn a_host_unknown_at_first_passes_once_a_refreshed_status_reports_it() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let names = StubNodeNames::new(Ok(NodeNames::default()));
    let direct = direct_router_with(&state, &source, &names);
    assert_eq!(
        names.reads(),
        0,
        "nothing reads the names before a request needs them"
    );
    // The node got its address after the listener started.
    names.set(Ok(node_names()));
    assert_eq!(
        status(
            &direct,
            direct_request("GET", "/api/health", DIRECT_HOST, None)
        )
        .await,
        StatusCode::OK,
        "the refreshed status names the address"
    );
    assert_eq!(names.reads(), 1);
    // A known name needs no read, and the refreshed names are kept.
    assert_eq!(
        status(
            &direct,
            direct_request("GET", "/api/health", "mac.tail1234.ts.net", None)
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(names.reads(), 1);
}

#[tokio::test]
async fn an_unreadable_status_keeps_the_previous_names_and_refuses_the_unmatched_host() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let names = StubNodeNames::new(Ok(node_names()));
    let direct = direct_router_with(&state, &source, &names);
    names.set(Err("tailscaled is not running".into()));
    assert_eq!(
        status(
            &direct,
            direct_request("GET", "/api/health", "rebind.example:8787", None)
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(names.reads(), 1, "the unmatched host was re-read once");
    assert_eq!(
        status(
            &direct,
            direct_request("GET", "/api/health", DIRECT_HOST, None)
        )
        .await,
        StatusCode::OK,
        "the previous names still answer"
    );
}

#[tokio::test]
async fn foreign_hosts_do_not_make_the_listener_read_the_status_without_bound() {
    let state = state().await;
    let source = tailscale_source(&["owner@example.com"]);
    let names = StubNodeNames::new(Ok(node_names()));
    let direct = direct_router_with(&state, &source, &names);
    for _ in 0..5 {
        assert_eq!(
            status(
                &direct,
                direct_request("GET", "/api/health", "rebind.example:8787", None)
            )
            .await,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        names.reads(),
        1,
        "requests within the refresh interval share one read"
    );
}
