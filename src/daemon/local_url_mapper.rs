use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt as _};
use http_body_util::{BodyDataStream, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{CONTENT_LENGTH, HOST, HeaderName};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request as HyperRequest, Response as HyperResponse, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{OnceCell, mpsc, oneshot, watch};
use zenwave::Client;

/// Upper bound on buffered request payloads accepted by a mapped local-URL proxy.
pub const MAX_LOCAL_PROXY_REQUEST_BYTES: usize = 32 * 1024 * 1024;

type ProxyResponseBody =
    StreamBody<futures_util::stream::MapOk<skyzen::Body, fn(Bytes) -> Frame<Bytes>>>;

fn into_frame(bytes: Bytes) -> Frame<Bytes> {
    Frame::data(bytes)
}

fn stream_response_body(body: skyzen::Body) -> ProxyResponseBody {
    StreamBody::new(body.map_ok(into_frame as fn(_) -> _))
}

#[derive(Debug, thiserror::Error)]
pub enum LocalUrlProxyError {
    #[error("failed to bind local url proxy listener: {0}")]
    BindListener(#[from] std::io::Error),
    #[error("failed to parse local proxy target URL: {0}")]
    ParseTargetUrl(#[from] url::ParseError),
    #[error("local-url map only supports http/https, got {scheme}")]
    UnsupportedScheme { scheme: String },
    #[error("local-url map requires a known port")]
    MissingKnownPort,
    #[error("local url proxy mapper actor is unavailable")]
    ActorUnavailable,
    #[error("local url proxy mapper actor dropped reply")]
    ActorReplyDropped,
    #[error("unsupported request method {method}: {source}")]
    UnsupportedRequestMethod {
        method: String,
        #[source]
        source: hyper::http::method::InvalidMethod,
    },
    #[error("failed to read proxy request body: {0}")]
    ReadRequestBody(#[source] hyper::Error),
    #[error("proxy request body exceeds {limit} bytes")]
    RequestBodyTooLarge { limit: usize },
    #[error("failed to build or execute upstream proxy request: {0}")]
    UpstreamRequest(#[from] zenwave::Error),
    #[error("invalid upstream status code: {0}")]
    InvalidUpstreamStatus(#[from] hyper::http::status::InvalidStatusCode),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum ProxyScheme {
    Http,
    Https,
}

impl ProxyScheme {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LocalUrlProxyUpstream {
    pub scheme: ProxyScheme,
    pub container_ip: String,
    pub target_port: u16,
    pub host_header: String,
}

#[derive(Debug, Clone, Eq)]
pub struct LocalUrlProxyKey {
    pub session_id: String,
    pub scheme: ProxyScheme,
    pub requested_host: String,
    pub target_port: u16,
}

impl PartialEq for LocalUrlProxyKey {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.scheme == other.scheme
            && self.requested_host == other.requested_host
            && self.target_port == other.target_port
    }
}

impl Hash for LocalUrlProxyKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session_id.hash(state);
        self.scheme.hash(state);
        self.requested_host.hash(state);
        self.target_port.hash(state);
    }
}

#[derive(Debug)]
struct LocalUrlProxyEntry {
    mapped_port: u16,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug)]
struct LocalUrlProxySlot {
    slot_id: String,
    upstream: LocalUrlProxyUpstream,
    entry: OnceCell<LocalUrlProxyEntry>,
}

impl LocalUrlProxySlot {
    fn new(upstream: LocalUrlProxyUpstream) -> Self {
        Self {
            slot_id: crate::id::new(),
            upstream,
            entry: OnceCell::new(),
        }
    }

    fn shutdown(&self) {
        if let Some(entry) = self.entry.get() {
            let _ = entry.shutdown_tx.send(true);
        }
    }

    async fn ensure_port(
        self: &Arc<Self>,
        key: &LocalUrlProxyKey,
        command_tx: &mpsc::UnboundedSender<LocalUrlProxyCommand>,
    ) -> Result<u16, LocalUrlProxyError> {
        let entry = self
            .entry
            .get_or_try_init(|| async {
                let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
                let mapped_port = listener.local_addr()?.port();
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let task_upstream = self.upstream.clone();
                let task_key = key.clone();
                let task_slot_id = self.slot_id.clone();
                let task_command_tx = command_tx.clone();

                tokio::spawn(async move {
                    run_proxy_listener(listener, task_upstream, shutdown_rx).await;
                    let _ = task_command_tx.send(LocalUrlProxyCommand::Drop {
                        key: task_key,
                        slot_id: task_slot_id,
                    });
                });

                Ok::<LocalUrlProxyEntry, LocalUrlProxyError>(LocalUrlProxyEntry {
                    mapped_port,
                    shutdown_tx,
                })
            })
            .await?;
        Ok(entry.mapped_port)
    }
}

enum LocalUrlProxyCommand {
    EnsurePort {
        key: LocalUrlProxyKey,
        upstream: LocalUrlProxyUpstream,
        reply: oneshot::Sender<Result<u16, LocalUrlProxyError>>,
    },
    Drop {
        key: LocalUrlProxyKey,
        slot_id: String,
    },
    RemoveSession {
        session_id: String,
    },
    RemoveAll,
}

pub struct LocalUrlPortMapper {
    command_tx: mpsc::UnboundedSender<LocalUrlProxyCommand>,
}

impl std::fmt::Debug for LocalUrlPortMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalUrlPortMapper").finish_non_exhaustive()
    }
}

impl LocalUrlPortMapper {
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let actor_tx = command_tx.clone();
        tokio::spawn(run_local_url_proxy_actor(command_rx, actor_tx));
        Self { command_tx }
    }

    pub async fn ensure_port(
        self: &Arc<Self>,
        key: LocalUrlProxyKey,
        upstream: LocalUrlProxyUpstream,
    ) -> Result<u16, LocalUrlProxyError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(LocalUrlProxyCommand::EnsurePort {
                key,
                upstream,
                reply: reply_tx,
            })
            .map_err(|_| LocalUrlProxyError::ActorUnavailable)?;
        reply_rx
            .await
            .map_err(|_| LocalUrlProxyError::ActorReplyDropped)?
    }

    /// Shuts down and forgets every mapped port belonging to `session_id`.
    pub fn remove_session(&self, session_id: &str) {
        let _ = self.command_tx.send(LocalUrlProxyCommand::RemoveSession {
            session_id: session_id.to_string(),
        });
    }

    /// Shuts down and forgets every mapped port across all sessions.
    pub fn remove_all(&self) {
        let _ = self.command_tx.send(LocalUrlProxyCommand::RemoveAll);
    }
}

async fn run_local_url_proxy_actor(
    mut command_rx: mpsc::UnboundedReceiver<LocalUrlProxyCommand>,
    command_tx: mpsc::UnboundedSender<LocalUrlProxyCommand>,
) {
    let mut entries: HashMap<LocalUrlProxyKey, Arc<LocalUrlProxySlot>> = HashMap::new();

    while let Some(command) = command_rx.recv().await {
        match command {
            LocalUrlProxyCommand::EnsurePort {
                key,
                upstream,
                reply,
            } => {
                let result =
                    ensure_local_url_proxy_port(&mut entries, key, upstream, command_tx.clone())
                        .await;
                let _ = reply.send(result);
            }
            LocalUrlProxyCommand::Drop { key, slot_id } => {
                if entries
                    .get(&key)
                    .is_some_and(|current| current.slot_id == slot_id)
                {
                    entries.remove(&key);
                }
            }
            LocalUrlProxyCommand::RemoveSession { session_id } => {
                entries.retain(|key, slot| {
                    if key.session_id == session_id {
                        slot.shutdown();
                        false
                    } else {
                        true
                    }
                });
            }
            LocalUrlProxyCommand::RemoveAll => {
                for (_, slot) in entries.drain() {
                    slot.shutdown();
                }
            }
        }
    }
}

async fn ensure_local_url_proxy_port(
    entries: &mut HashMap<LocalUrlProxyKey, Arc<LocalUrlProxySlot>>,
    key: LocalUrlProxyKey,
    upstream: LocalUrlProxyUpstream,
    command_tx: mpsc::UnboundedSender<LocalUrlProxyCommand>,
) -> Result<u16, LocalUrlProxyError> {
    loop {
        if let Some(existing) = entries.get(&key).cloned() {
            if existing.upstream == upstream {
                return existing.ensure_port(&key, &command_tx).await;
            }
            existing.shutdown();
            entries.remove(&key);
            continue;
        }

        let slot = Arc::new(LocalUrlProxySlot::new(upstream.clone()));
        entries.insert(key.clone(), Arc::clone(&slot));
        return slot.ensure_port(&key, &command_tx).await;
    }
}

async fn run_proxy_listener(
    listener: TcpListener,
    upstream: LocalUrlProxyUpstream,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let listen_addr = listener.local_addr().ok();
    tracing::info!(
        listen_addr = ?listen_addr,
        upstream_scheme = upstream.scheme.as_str(),
        upstream_container_ip = %upstream.container_ip,
        upstream_target_port = upstream.target_port,
        "local artifact url proxy started"
    );

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else {
                    tracing::warn!(listen_addr = ?listen_addr, "local artifact url proxy accept failed");
                    continue;
                };

                let request_upstream = upstream.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request| {
                        proxy_http_request(request, request_upstream.clone())
                    });
                    if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::warn!(error = %error, "local artifact url proxy connection failed");
                    }
                });
            }
        }
    }

    tracing::info!(listen_addr = ?listen_addr, "local artifact url proxy stopped");
}

async fn collect_bounded_request_body<S, E>(
    mut stream: S,
    limit: usize,
    map_err: fn(E) -> LocalUrlProxyError,
) -> Result<Vec<u8>, LocalUrlProxyError>
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin,
{
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_err)?;
        if out.len().saturating_add(chunk.len()) > limit {
            return Err(LocalUrlProxyError::RequestBodyTooLarge { limit });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn proxy_http_request(
    request: HyperRequest<Incoming>,
    upstream: LocalUrlProxyUpstream,
) -> Result<HyperResponse<ProxyResponseBody>, Infallible> {
    if !super::auth::has_loopback_host_parts(request.uri(), request.headers()) {
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            "the local proxy answers only a loopback Host",
        ));
    }
    if !super::auth::has_loopback_origin_or_none_headers(request.headers()) {
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            "the local proxy refuses requests from a foreign Origin",
        ));
    }
    let response =
        match proxy_http_request_impl(request, upstream, MAX_LOCAL_PROXY_REQUEST_BYTES).await {
            Ok(response) => response,
            Err(LocalUrlProxyError::RequestBodyTooLarge { .. }) => error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "local artifact proxy request body exceeds limit",
            ),
            Err(error) => {
                tracing::warn!(error = %error, "local artifact url proxy request failed");
                error_response(StatusCode::BAD_GATEWAY, "local artifact proxy failed")
            }
        };
    Ok(response)
}

async fn proxy_http_request_impl(
    request: HyperRequest<Incoming>,
    upstream: LocalUrlProxyUpstream,
    max_request_bytes: usize,
) -> Result<HyperResponse<ProxyResponseBody>, LocalUrlProxyError> {
    if let Some(length) = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && length > max_request_bytes
    {
        return Err(LocalUrlProxyError::RequestBodyTooLarge {
            limit: max_request_bytes,
        });
    }
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", zenwave::uri::PathAndQuery::as_str);
    let upstream_url = format!(
        "{}://{}:{}{}",
        upstream.scheme.as_str(),
        upstream.container_ip,
        upstream.target_port,
        path_and_query
    );

    let method = to_zenwave_method(request.method())?;
    let (parts, body) = request.into_parts();
    let body_bytes = collect_bounded_request_body(
        BodyDataStream::new(body),
        max_request_bytes,
        LocalUrlProxyError::ReadRequestBody,
    )
    .await?;

    let mut client = zenwave::client();
    let mut upstream_request = client.method(method, upstream_url.as_str())?;

    for name in parts.headers.keys() {
        if should_skip_request_header(name) {
            continue;
        }
        for value in &parts.headers.get_all(name) {
            upstream_request = upstream_request.header(name.as_str(), value.as_bytes())?;
        }
    }
    upstream_request = upstream_request.header(HOST.as_str(), upstream.host_header.as_str())?;

    if !body_bytes.is_empty() {
        upstream_request = upstream_request.bytes_body(body_bytes);
    }

    let upstream_response: zenwave::Response = match upstream_request.await {
        Ok(response) => response,
        Err(zenwave::Error::Http { response, .. }) => response.response,
        Err(error) => return Err(error.into()),
    };
    let status = StatusCode::from_u16(upstream_response.status().as_u16())?;
    let headers = upstream_response.headers().clone();

    let mut response = HyperResponse::new(stream_response_body(upstream_response.into_body()));
    *response.status_mut() = status;

    for name in headers.keys() {
        if should_skip_response_header(name) {
            continue;
        }
        let Ok(header_name) = HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        for value in &headers.get_all(name) {
            let Ok(header_value) = hyper::header::HeaderValue::from_bytes(value.as_bytes()) else {
                continue;
            };
            response
                .headers_mut()
                .append(header_name.clone(), header_value);
        }
    }

    Ok(response)
}

pub fn to_zenwave_method(method: &Method) -> Result<zenwave::Method, LocalUrlProxyError> {
    method
        .as_str()
        .parse::<zenwave::Method>()
        .map_err(|source| LocalUrlProxyError::UnsupportedRequestMethod {
            method: method.as_str().to_string(),
            source,
        })
}

pub fn should_skip_request_header(name: &HeaderName) -> bool {
    let lowered = name.as_str().to_ascii_lowercase();
    lowered == "connection"
        || lowered == "keep-alive"
        || lowered == "proxy-authenticate"
        || lowered == "proxy-authorization"
        || lowered == "te"
        || lowered == "trailer"
        || lowered == "transfer-encoding"
        || lowered == "upgrade"
        || lowered == "host"
}

pub fn should_skip_response_header(name: &HeaderName) -> bool {
    let lowered = name.as_str().to_ascii_lowercase();
    lowered == "connection"
        || lowered == "keep-alive"
        || lowered == "proxy-authenticate"
        || lowered == "proxy-authorization"
        || lowered == "te"
        || lowered == "trailer"
        || lowered == "transfer-encoding"
        || lowered == "upgrade"
        || lowered == CONTENT_LENGTH.as_str()
}

fn error_response(status: StatusCode, message: &str) -> HyperResponse<ProxyResponseBody> {
    let mut response = HyperResponse::new(stream_response_body(skyzen::Body::from(
        message.as_bytes().to_vec(),
    )));
    *response.status_mut() = status;
    response
}

pub fn mapped_url_from_target(mapped_port: u16, target: &url::Url) -> String {
    let mut mapped = format!("http://localhost:{mapped_port}{}", target.path());
    if let Some(query) = target.query() {
        mapped.push('?');
        mapped.push_str(query);
    }
    mapped
}

pub fn target_host_header(target: &url::Url, port: u16) -> String {
    target.port().map_or_else(
        || match target.scheme() {
            "http" if port == 80 => target.host_str().unwrap_or("localhost").to_string(),
            "https" if port == 443 => target.host_str().unwrap_or("localhost").to_string(),
            _ => format!("{}:{port}", target.host_str().unwrap_or("localhost")),
        },
        |explicit| format!("{}:{explicit}", target.host_str().unwrap_or("localhost")),
    )
}

pub fn parse_local_proxy_target(
    url: &str,
) -> Result<(url::Url, ProxyScheme, u16), LocalUrlProxyError> {
    let parsed = url::Url::parse(url)?;
    let scheme = ProxyScheme::from_str(parsed.scheme()).ok_or_else(|| {
        LocalUrlProxyError::UnsupportedScheme {
            scheme: parsed.scheme().to_string(),
        }
    })?;
    let port = parsed
        .port_or_known_default()
        .ok_or(LocalUrlProxyError::MissingKnownPort)?;
    Ok((parsed, scheme, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[tokio::test]
    async fn local_url_mapper_enforces_loopback_policy_body_bounds_and_session_cleanup() {
        let upstream_listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("upstream");
        let upstream_port = upstream_listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = upstream_listener.accept().await {
                tokio::spawn(http1::Builder::new().serve_connection(
                    TokioIo::new(stream),
                    service_fn(|_req| async {
                        Ok::<_, Infallible>(HyperResponse::new(Full::new(Bytes::from_static(
                            b"proxied-ok",
                        ))))
                    }),
                ));
            }
        });

        let mapper = Arc::new(LocalUrlPortMapper::new());
        let port = mapper
            .ensure_port(
                LocalUrlProxyKey {
                    session_id: "sess-1".to_string(),
                    scheme: ProxyScheme::Http,
                    requested_host: "localhost".to_string(),
                    target_port: upstream_port,
                },
                LocalUrlProxyUpstream {
                    scheme: ProxyScheme::Http,
                    container_ip: "127.0.0.1".to_string(),
                    target_port: upstream_port,
                    host_header: format!("localhost:{upstream_port}"),
                },
            )
            .await
            .expect("mapped port");

        let send = |host: String, origin: Option<&'static str>, content_len: Option<usize>| async move {
            let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect mapped port");
            let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .expect("handshake");
            tokio::spawn(conn);
            let mut builder = HyperRequest::builder()
                .method("POST")
                .uri("/check")
                .header("Host", host);
            if let Some(origin) = origin {
                builder = builder.header("Origin", origin);
            }
            if let Some(len) = content_len {
                builder = builder.header("Content-Length", len.to_string());
            }
            let req = builder
                .body(Full::new(Bytes::from_static(b"hi")))
                .expect("req");
            sender.send_request(req).await.expect("resp").status()
        };

        assert_eq!(
            send("rebind.evil.example".to_string(), None, None).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                format!("localhost:{port}"),
                Some("https://evil.example"),
                None
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                format!("localhost:{port}"),
                None,
                Some(MAX_LOCAL_PROXY_REQUEST_BYTES + 1)
            )
            .await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            send(format!("localhost:{port}"), None, None).await,
            StatusCode::OK
        );

        let chunk_err = collect_bounded_request_body(
            futures_util::stream::iter([
                Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 40])),
                Ok(Bytes::from(vec![b'b'; 30])),
            ]),
            64,
            |_| unreachable!(),
        )
        .await
        .expect_err("stream over cap must fail");
        assert!(matches!(
            chunk_err,
            LocalUrlProxyError::RequestBodyTooLarge { limit: 64 }
        ));

        mapper.remove_session("sess-1");
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(closed.is_ok(), "remove_session must shut down mapped port");
    }
}
