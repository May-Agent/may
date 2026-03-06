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
use tokio::sync::{Mutex, watch};
use zenwave::Client;
use zenwave::ResponseExt;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) enum ProxyScheme {
    Http,
    Https,
}

impl ProxyScheme {
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) struct LocalUrlProxyUpstream {
    pub(crate) scheme: ProxyScheme,
    pub(crate) container_ip: String,
    pub(crate) target_port: u16,
    pub(crate) host_header: String,
}

#[derive(Debug, Clone, Eq)]
pub(crate) struct LocalUrlProxyKey {
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
    upstream: LocalUrlProxyUpstream,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug, Default)]
pub(crate) struct LocalUrlPortMapper {
    entries: Mutex<HashMap<LocalUrlProxyKey, LocalUrlProxyEntry>>,
}

impl LocalUrlPortMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn ensure_port(
        self: &Arc<Self>,
        key: LocalUrlProxyKey,
        upstream: LocalUrlProxyUpstream,
    ) -> anyhow::Result<u16> {
        let mut entries = self.entries.lock().await;

        if let Some(existing) = entries.get(&key) {
            if existing.upstream == upstream {
                return Ok(existing.mapped_port);
            }
            let _ = existing.shutdown_tx.send(true);
            entries.remove(&key);
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let mapped_port = listener.local_addr()?.port();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task_upstream = upstream.clone();
        let task_key = key.clone();
        let manager = Arc::clone(self);

        tokio::spawn(async move {
            run_proxy_listener(listener, task_upstream, shutdown_rx).await;
            manager.remove_entry(&task_key, mapped_port).await;
        });

        entries.insert(
            key,
            LocalUrlProxyEntry {
                mapped_port,
                upstream,
                shutdown_tx,
            },
        );

        Ok(mapped_port)
    }

    async fn remove_entry(self: &Arc<Self>, key: &LocalUrlProxyKey, mapped_port: u16) {
        let mut entries = self.entries.lock().await;
        if entries
            .get(key)
            .is_some_and(|entry| entry.mapped_port == mapped_port)
        {
            entries.remove(key);
        }
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
        upstream = %format!(
            "{}://{}:{}",
            upstream.scheme.as_str(),
            upstream.container_ip,
            upstream.target_port
        ),
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
        .map(|value| value.as_str())
        .unwrap_or("/");
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
        for value in parts.headers.get_all(name).iter() {
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
        for value in headers.get_all(name).iter() {
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

pub(crate) fn mapped_url_from_target(mapped_port: u16, target: &url::Url) -> String {
    let mut mapped = format!("http://localhost:{mapped_port}{}", target.path());
    if let Some(query) = target.query() {
        mapped.push('?');
        mapped.push_str(query);
    }
    mapped
}

pub(crate) fn target_host_header(target: &url::Url, port: u16) -> String {
    match target.port() {
        Some(explicit) => format!("{}:{explicit}", target.host_str().unwrap_or("localhost")),
        None => match target.scheme() {
            "http" if port == 80 => target.host_str().unwrap_or("localhost").to_string(),
            "https" if port == 443 => target.host_str().unwrap_or("localhost").to_string(),
            _ => format!("{}:{port}", target.host_str().unwrap_or("localhost")),
        },
    }
}

pub(crate) fn parse_local_proxy_target(url: &str) -> anyhow::Result<(url::Url, ProxyScheme, u16)> {
    let parsed = url::Url::parse(url)?;
    let scheme = ProxyScheme::from_str(parsed.scheme())
        .ok_or_else(|| anyhow::anyhow!("local-url map only supports http/https"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("local-url map requires a known port"))?;
    Ok((parsed, scheme, port))
}
