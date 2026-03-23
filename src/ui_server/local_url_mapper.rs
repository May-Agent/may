use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, HOST, HeaderName};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request as HyperRequest, Response as HyperResponse, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{OnceCell, mpsc, oneshot, watch};
use zenwave::Client;
use zenwave::ResponseExt;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum ProxyScheme {
    Http,
    Https,
}

impl ProxyScheme {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LocalUrlProxyUpstream {
    pub(crate) scheme: ProxyScheme,
    pub(crate) container_ip: String,
    pub(crate) target_port: u16,
    pub(crate) host_header: String,
}

#[derive(Debug, Clone, Eq)]
pub struct LocalUrlProxyKey {
    pub(crate) session_id: String,
    pub(crate) scheme: ProxyScheme,
    pub(crate) requested_host: String,
    pub(crate) target_port: u16,
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
            slot_id: uuid::Uuid::new_v4().to_string(),
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
    ) -> anyhow::Result<u16> {
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

                Ok::<LocalUrlProxyEntry, anyhow::Error>(LocalUrlProxyEntry {
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
        reply: oneshot::Sender<anyhow::Result<u16>>,
    },
    Drop {
        key: LocalUrlProxyKey,
        slot_id: String,
    },
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
    pub(crate) fn new() -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let actor_tx = command_tx.clone();
        tokio::spawn(run_local_url_proxy_actor(command_rx, actor_tx));
        Self { command_tx }
    }

    pub(crate) async fn ensure_port(
        self: &Arc<Self>,
        key: LocalUrlProxyKey,
        upstream: LocalUrlProxyUpstream,
    ) -> anyhow::Result<u16> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(LocalUrlProxyCommand::EnsurePort {
                key,
                upstream,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("local url proxy mapper actor is unavailable"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("local url proxy mapper actor dropped reply"))?
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
        }
    }
}

async fn ensure_local_url_proxy_port(
    entries: &mut HashMap<LocalUrlProxyKey, Arc<LocalUrlProxySlot>>,
    key: LocalUrlProxyKey,
    upstream: LocalUrlProxyUpstream,
    command_tx: mpsc::UnboundedSender<LocalUrlProxyCommand>,
) -> anyhow::Result<u16> {
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

async fn proxy_http_request(
    request: HyperRequest<Incoming>,
    upstream: LocalUrlProxyUpstream,
) -> Result<HyperResponse<Full<Bytes>>, Infallible> {
    let response = match proxy_http_request_impl(request, upstream).await {
        Ok(response) => response,
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
) -> anyhow::Result<HyperResponse<Full<Bytes>>> {
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
    let body_bytes = body.collect().await?.to_bytes().to_vec();

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
    let body: Bytes = upstream_response.into_bytes().await?;

    let mut response = HyperResponse::new(Full::new(body));
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

fn to_zenwave_method(method: &Method) -> anyhow::Result<zenwave::Method> {
    method
        .as_str()
        .parse::<zenwave::Method>()
        .map_err(|error| anyhow::anyhow!("unsupported request method {}: {error}", method.as_str()))
}

fn should_skip_request_header(name: &HeaderName) -> bool {
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

fn should_skip_response_header(name: &HeaderName) -> bool {
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

fn error_response(status: StatusCode, message: &str) -> HyperResponse<Full<Bytes>> {
    let mut response = HyperResponse::new(Full::new(Bytes::from(message.as_bytes().to_vec())));
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
    match target.port() {
        Some(explicit) => format!("{}:{explicit}", target.host_str().unwrap_or("localhost")),
        None => match target.scheme() {
            "http" if port == 80 => target.host_str().unwrap_or("localhost").to_string(),
            "https" if port == 443 => target.host_str().unwrap_or("localhost").to_string(),
            _ => format!("{}:{port}", target.host_str().unwrap_or("localhost")),
        },
    }
}

pub fn parse_local_proxy_target(url: &str) -> anyhow::Result<(url::Url, ProxyScheme, u16)> {
    let parsed = url::Url::parse(url)?;
    let scheme = ProxyScheme::from_str(parsed.scheme())
        .ok_or_else(|| anyhow::anyhow!("local-url map only supports http/https"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("local-url map requires a known port"))?;
    Ok((parsed, scheme, port))
}
