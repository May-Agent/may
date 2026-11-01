//! Agent tunnels: the `tunnel` command registers an HTTP endpoint the daemon
//! can reach, and the daemon serves it under `/tunnel/<id>/` on every address
//! it listens on — so a link works however the user reaches May (localhost,
//! tailnet, LAN).
//!
//! Tunnels live in memory and die with the daemon; they are dropped when
//! their session is deleted.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use futures_util::StreamExt;
use skyzen::Result as SkyResult;
use skyzen::extract::Extractor;
use skyzen::header::{
    CONNECTION, HOST, HeaderMap, HeaderName, HeaderValue, REFERER, SEC_WEBSOCKET_KEY, UPGRADE,
};
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::websocket::{WebSocketUpgrade, WebSocketUpgradeError, WebSocketUpgradeResponder};
use skyzen::{Body, Error, Method, Request, Response, StatusCode, Uri};
use skyzen_core::Responder;
use skyzen_core::middleware::{Middleware, Next};
use tokio::sync::{mpsc, oneshot};
use zenwave::Client;

use super::artifacts::{is_container_local_proxy_host, resolve_container_proxy_target};
use super::error::{bad_request_error, internal_error};
use super::local_url_mapper::{
    ProxyScheme, parse_local_proxy_target, should_skip_request_header, should_skip_response_header,
    target_host_header, to_zenwave_method,
};
use super::state::DaemonState;
use super::types::{TunnelInfoResponse, TunnelRegisterRequest, TunnelRegisterResponse};
use crate::agent::tunnel_tool::{TunnelRequest, TunnelResult};

#[derive(Debug, Clone)]
pub struct TunnelEntry {
    pub session_id: String,
    pub url: url::Url,
}

enum TunnelCommand {
    Register {
        entry: TunnelEntry,
        reply: oneshot::Sender<String>,
    },
    Resolve {
        id: String,
        reply: oneshot::Sender<Option<TunnelEntry>>,
    },
    RemoveSession {
        session_id: String,
    },
    RemoveAll,
}

/// The daemon's tunnel registry: a small actor so request handlers never
/// take a lock on the hot path.
pub struct TunnelRegistry {
    command_tx: mpsc::UnboundedSender<TunnelCommand>,
}

impl fmt::Debug for TunnelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelRegistry").finish_non_exhaustive()
    }
}

impl TunnelRegistry {
    pub fn new() -> Self {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut entries: HashMap<String, TunnelEntry> = HashMap::new();
            while let Some(command) = command_rx.recv().await {
                match command {
                    TunnelCommand::Register { entry, reply } => {
                        let mut id = crate::id::new();
                        while entries.contains_key(&id) {
                            id = crate::id::new();
                        }
                        entries.insert(id.clone(), entry);
                        let _ = reply.send(id);
                    }
                    TunnelCommand::Resolve { id, reply } => {
                        let _ = reply.send(entries.get(&id).cloned());
                    }
                    TunnelCommand::RemoveSession { session_id } => {
                        entries.retain(|_, entry| entry.session_id != session_id);
                    }
                    TunnelCommand::RemoveAll => entries.clear(),
                }
            }
        });
        Self { command_tx }
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> TunnelCommand) -> Option<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx.send(make(reply_tx)).ok()?;
        reply_rx.await.ok()
    }

    pub async fn register(&self, entry: TunnelEntry) -> String {
        self.ask(|reply| TunnelCommand::Register { entry, reply })
            .await
            .expect("tunnel registry actor is alive for the daemon's lifetime")
    }

    pub async fn resolve(&self, id: &str) -> Option<TunnelEntry> {
        self.ask(|reply| TunnelCommand::Resolve {
            id: id.to_string(),
            reply,
        })
        .await
        .flatten()
    }

    pub fn remove_session(&self, session_id: &str) {
        let _ = self.command_tx.send(TunnelCommand::RemoveSession {
            session_id: session_id.to_string(),
        });
    }

    pub fn remove_all(&self) {
        let _ = self.command_tx.send(TunnelCommand::RemoveAll);
    }
}

/// What the request handler needs to reach the target: connection authority
/// and the `Host` header to send.
struct Upstream {
    authority: String,
    host_header: String,
    scheme: ProxyScheme,
}

async fn session_environment(
    state: &DaemonState,
    session_id: &str,
) -> Result<crate::db::ProfileEnvironmentKind, String> {
    let db = state.db();
    let session = db
        .get_session(session_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "session not found".to_string())?;
    let Some(profile_id) = session.profile_id else {
        return Ok(crate::db::ProfileEnvironmentKind::Local);
    };
    let profile = db
        .get_profile(&profile_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "profile not found".to_string())?;
    crate::db::ProfileEnvironmentKind::from_str(profile.environment.as_str())
        .map_err(|error| error.to_string())
}

/// Normalizes a requested target into a connectable http(s) URL: a missing
/// scheme defaults to `http`, and bind-all hosts (`0.0.0.0`, `::`) map to
/// their loopback equivalents.
fn normalize_target(raw: &str) -> Result<url::Url, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("tunnel url must not be empty".to_string());
    }
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let (mut url, _scheme, _port) =
        parse_local_proxy_target(&candidate).map_err(|error| error.to_string())?;
    let all_interfaces = matches!(url.host_str(), Some("0.0.0.0" | "::"));
    if all_interfaces {
        let loopback = if url.host_str() == Some("0.0.0.0") {
            "127.0.0.1"
        } else {
            "::1"
        };
        url.set_host(Some(loopback))
            .map_err(|error| error.to_string())?;
    }
    Ok(url)
}

/// Registers a tunnel for a session; shared by the IPC broker, the REST
/// endpoint and `may exec` (via REST).
pub async fn handle_tunnel_request(
    state: &DaemonState,
    request: TunnelRequest,
) -> Result<TunnelResult, String> {
    let url = normalize_target(&request.url)?;
    let environment = session_environment(state, &request.session_id).await?;
    if environment == crate::db::ProfileEnvironmentKind::Container
        && !is_container_local_proxy_host(url.host_str().unwrap_or_default())
    {
        return Err(
            "container sessions may only tunnel loopback endpoints (localhost/127.x)".to_string(),
        );
    }
    let id = state
        .tunnels
        .register(TunnelEntry {
            session_id: request.session_id,
            url: url.clone(),
        })
        .await;
    let path = format!("/tunnel/{id}/");
    let local_url = state.remote_access_manager.local_addr().map_or_else(
        || format!("http://127.0.0.1:{}{path}", super::server::DEFAULT_PORT),
        |addr| format!("http://{addr}{path}"),
    );
    Ok(TunnelResult {
        id,
        url: url.into(),
        path,
        local_url,
    })
}

/// The session's environment decides where loopback lives: the host for
/// local sessions, the container IP for container sessions.
async fn resolve_upstream(state: &DaemonState, entry: &TunnelEntry) -> SkyResult<Upstream> {
    let environment = session_environment(state, &entry.session_id)
        .await
        .map_err(bad_request_error)?;
    let (_target, scheme, port) =
        parse_local_proxy_target(entry.url.as_str()).map_err(internal_error)?;
    match environment {
        crate::db::ProfileEnvironmentKind::Local => {
            let host = entry
                .url
                .host_str()
                .ok_or_else(|| Error::msg("tunnel target has no host"))?;
            Ok(Upstream {
                authority: format!("{host}:{port}"),
                host_header: target_host_header(&entry.url, port),
                scheme,
            })
        }
        crate::db::ProfileEnvironmentKind::Container => {
            let resolved = resolve_container_proxy_target(state, &entry.session_id).await?;
            Ok(Upstream {
                authority: format!("{}:{port}", resolved.container_ip),
                host_header: target_host_header(&entry.url, port),
                scheme,
            })
        }
    }
}

/// The upstream path for a proxied request: the registered base path plus
/// the request's remaining path and query.
fn upstream_path(base: &url::Url, sub_path: &str, query: Option<&str>) -> String {
    let mut path = String::with_capacity(base.path().len() + sub_path.len() + 8);
    path.push_str(base.path().trim_end_matches('/'));
    path.push('/');
    path.push_str(sub_path.trim_start_matches('/'));
    if let Some(query) = query {
        path.push('?');
        path.push_str(query);
    }
    path
}

/// Rewrites a `Location` value back into the tunnel prefix so redirects keep
/// the client inside `/tunnel/<id>/`.
fn rewrite_location(id: &str, entry: &TunnelEntry, value: &HeaderValue) -> Option<HeaderValue> {
    let location = value.to_str().ok()?;
    let prefix = format!("/tunnel/{id}");
    let rewritten = if location.starts_with('/') && !location.starts_with("//") {
        format!("{prefix}{location}")
    } else {
        let origin = format!(
            "{}://{}",
            entry.url.scheme(),
            entry
                .url
                .host_str()
                .map(|host| {
                    entry
                        .url
                        .port()
                        .map_or_else(|| host.to_string(), |port| format!("{host}:{port}"))
                })
                .unwrap_or_default()
        );
        location
            .strip_prefix(origin.as_str())
            .map(|rest| format!("{prefix}{rest}"))?
    };
    HeaderValue::from_str(rewritten.as_str()).ok()
}

fn skip_request_header(name: &HeaderName) -> bool {
    should_skip_request_header(name)
}

fn skip_response_header(name: &HeaderName) -> bool {
    should_skip_response_header(name)
}

/// Forwards one HTTP request to the tunnel's target and returns the upstream
/// response, with `Location` headers rewritten into the tunnel prefix.
async fn proxy_http(
    state: &DaemonState,
    id: &str,
    entry: &TunnelEntry,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Body,
) -> SkyResult<Response> {
    let upstream = resolve_upstream(state, entry).await?;
    let url = format!(
        "{}://{}{}",
        upstream.scheme.as_str(),
        upstream.authority,
        path_and_query
    );
    let method = to_zenwave_method(method).map_err(internal_error)?;
    let mut client = zenwave::client();
    let mut request = client
        .method(method, url.as_str())
        .map_err(internal_error)?;
    for name in headers.keys() {
        if skip_request_header(name) {
            continue;
        }
        for value in headers.get_all(name) {
            request = request
                .header(name.as_str(), value.as_bytes())
                .map_err(internal_error)?;
        }
    }
    request = request
        .header(HOST.as_str(), upstream.host_header.as_str())
        .map_err(internal_error)?;
    request = request.stream_body(body);
    let upstream_response = match request.await {
        Ok(response) => response,
        Err(zenwave::Error::Http { response, .. }) => response.response,
        Err(error) => {
            return Err(Error::msg(format!(
                "upstream tunnel request failed: {error}"
            )));
        }
    };

    let status = upstream_response.status();
    let upstream_headers = upstream_response.headers().clone();
    let mut response = Response::new(upstream_response.into_body());
    *response.status_mut() = status;
    for name in upstream_headers.keys() {
        if skip_response_header(name) {
            continue;
        }
        for value in upstream_headers.get_all(name) {
            let value = if name.as_str().eq_ignore_ascii_case("location") {
                rewrite_location(id, entry, value).unwrap_or_else(|| value.clone())
            } else {
                value.clone()
            };
            response.headers_mut().append(name.clone(), value);
        }
    }
    Ok(response)
}

/// Shuttles frames between the client's WebSocket and the tunnel target's,
/// so dev-server HMR works through the tunnel.
fn proxy_ws(
    upgrade: WebSocketUpgrade,
    state: Arc<DaemonState>,
    entry: TunnelEntry,
    path_and_query: String,
) -> WebSocketUpgradeResponder {
    upgrade.on_upgrade(move |mut socket| async move {
        let upstream = resolve_upstream(&state, &entry).await?;
        let scheme = match upstream.scheme {
            ProxyScheme::Http => "ws",
            ProxyScheme::Https => "wss",
        };
        let url = format!("{scheme}://{}{path_and_query}", upstream.authority);
        let connection = zenwave::websocket::connect(&url)
            .await
            .map_err(|error| Error::msg(format!("tunnel websocket upstream failed: {error}")))?;
        let (upstream_tx, upstream_rx) = connection.split();
        loop {
            tokio::select! {
                message = socket.next() => match message {
                    Some(Ok(message)) => match message {
                        skyzen::websocket::WebSocketMessage::Text(text) => {
                            upstream_tx.send_text(text).await.map_err(|error| Error::msg(error.to_string()))?;
                        }
                        skyzen::websocket::WebSocketMessage::Binary(bytes) => {
                            upstream_tx.send_binary(bytes).await.map_err(|error| Error::msg(error.to_string()))?;
                        }
                        skyzen::websocket::WebSocketMessage::Ping(_) | skyzen::websocket::WebSocketMessage::Pong(_) => {},
                        skyzen::websocket::WebSocketMessage::Close => {
                            let _ = upstream_tx.close().await;
                            break;
                        }
                    },
                    Some(Err(_)) | None => break,
                },
                message = upstream_rx.recv() => match message {
                    Ok(Some(message)) => match message {
                        skyzen::websocket::WebSocketMessage::Text(text) => socket.send_text(text).await?,
                        skyzen::websocket::WebSocketMessage::Binary(bytes) => socket.send_binary(bytes).await?,
                        skyzen::websocket::WebSocketMessage::Ping(_) | skyzen::websocket::WebSocketMessage::Pong(_) => {},
                        skyzen::websocket::WebSocketMessage::Close => break,
                    },
                    Ok(None) | Err(_) => break,
                },
            }
        }
        Ok::<(), Error>(())
    })
}

/// The handler's response: a plain HTTP response or a WebSocket upgrade.
pub(super) enum TunnelResponse {
    Http(Response),
    Ws(WebSocketUpgradeResponder),
}

impl Responder for TunnelResponse {
    type Error = WebSocketUpgradeError;
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        match self {
            Self::Http(inner) => {
                *response = inner;
                Ok(())
            }
            Self::Ws(responder) => responder.respond_to(request, response),
        }
    }
}

/// Extracts a [`WebSocketUpgrade`] only when the request actually asks for
/// one: `WebSocketUpgrade`'s own extractor logs an error for every plain
/// HTTP request, which would spam the daemon log on every tunneled call.
pub(super) struct MaybeUpgrade(Option<WebSocketUpgrade>);

fn wants_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && headers.contains_key(SEC_WEBSOCKET_KEY)
        && headers
            .get(CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
}

impl Extractor for MaybeUpgrade {
    type Error = std::convert::Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        if !wants_websocket(request.headers()) {
            return Ok(Self(None));
        }
        Ok(Self(WebSocketUpgrade::extract(request).await.ok()))
    }
}

/// `GET|POST|… /tunnel/{id}/{*path}` — proxies to the registered target, or
/// upgrades to a WebSocket shuttle when the client asks for one.
pub(super) async fn tunnel_proxy(
    State(state): State<Arc<DaemonState>>,
    upgrade: MaybeUpgrade,
    params: Params,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> SkyResult<TunnelResponse> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let entry = state
        .tunnels
        .resolve(&id)
        .await
        .ok_or_else(|| Error::msg("unknown tunnel").set_status(StatusCode::NOT_FOUND))?;
    let sub_path = params.get("path").ok().unwrap_or_default().to_string();
    let path_and_query = upstream_path(&entry.url, &sub_path, uri.query());

    if let Some(upgrade) = upgrade.0 {
        return Ok(TunnelResponse::Ws(proxy_ws(
            upgrade,
            state,
            entry,
            path_and_query,
        )));
    }
    proxy_http(
        &state,
        &id,
        &entry,
        &method,
        &path_and_query,
        &headers,
        body,
    )
    .await
    .map(TunnelResponse::Http)
}

/// A request whose Referer points into `/tunnel/<id>/` and that routing
/// answered 404 is re-proxied to that tunnel — covering apps that emit
/// absolute paths like `/src/main.tsx`.
#[derive(Clone)]
pub struct TunnelRefererFallback {
    state: Arc<DaemonState>,
}

impl TunnelRefererFallback {
    pub const fn new(state: Arc<DaemonState>) -> Self {
        Self { state }
    }
}

fn referer_tunnel_id(request: &Request) -> Option<String> {
    let referer = request.headers().get(REFERER)?.to_str().ok()?;
    let url = url::Url::parse(referer).ok()?;
    let id = url
        .path()
        .strip_prefix("/tunnel/")?
        .split('/')
        .next()
        .filter(|id| !id.is_empty())?;
    Some(id.to_string())
}

impl Middleware for TunnelRefererFallback {
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> + Send {
        let state = self.state.clone();
        async move {
            let Some(id) = referer_tunnel_id(request) else {
                return next.run(request).await;
            };
            let Some(entry) = state.tunnels.resolve(&id).await else {
                return next.run(request).await;
            };
            let method = request.method().clone();
            let uri = request.uri().clone();
            let headers = request.headers().clone();
            let body = std::mem::take(request.body_mut());
            let response = next.run(request).await;
            let not_found = match &response {
                Ok(inner) => inner.status() == StatusCode::NOT_FOUND,
                Err(error) => error.status() == StatusCode::NOT_FOUND,
            };
            if !not_found {
                return response;
            }
            let path_and_query = uri
                .path_and_query()
                .map_or("/", |path_and_query| path_and_query.as_str());
            proxy_http(&state, &id, &entry, &method, path_and_query, &headers, body)
                .await
                .or(response)
        }
    }
}

pub(super) async fn register_tunnel(
    State(state): State<Arc<DaemonState>>,
    Json(payload): Json<TunnelRegisterRequest>,
) -> SkyResult<Json<TunnelRegisterResponse>> {
    if payload.session_id.trim().is_empty() || payload.url.trim().is_empty() {
        return Err(bad_request_error("session id and url are required"));
    }
    let result = handle_tunnel_request(
        &state,
        TunnelRequest {
            session_id: payload.session_id,
            url: payload.url,
        },
    )
    .await
    .map_err(bad_request_error)?;
    Ok(Json(TunnelRegisterResponse {
        id: result.id,
        url: result.url,
        path: result.path,
        local_url: result.local_url,
    }))
}

pub(super) async fn get_tunnel(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<TunnelInfoResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let entry = state
        .tunnels
        .resolve(&id)
        .await
        .ok_or_else(|| Error::msg("unknown tunnel").set_status(StatusCode::NOT_FOUND))?;
    Ok(Json(TunnelInfoResponse {
        id,
        target: entry.url.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_target_defaults_to_http_and_maps_bind_all_hosts() {
        assert_eq!(
            normalize_target("localhost:3000").expect("valid").as_str(),
            "http://localhost:3000/"
        );
        assert_eq!(
            normalize_target("0.0.0.0:5173").expect("valid").host_str(),
            Some("127.0.0.1")
        );
        assert!(normalize_target("ftp://localhost:3000").is_err());
    }

    #[test]
    fn upstream_path_joins_base_and_sub_path() {
        let base = url::Url::parse("http://localhost:3000/app/").expect("url");
        assert_eq!(
            upstream_path(&base, "assets/x.js", Some("v=1")),
            "/app/assets/x.js?v=1"
        );
        let root = url::Url::parse("http://localhost:3000").expect("url");
        assert_eq!(upstream_path(&root, "", None), "/");
    }
}
