//! Test support shared by the workspace's `#[cfg(test)]` code.
//!
//! Compiled only under the `test-support` feature, which a crate's
//! `[dev-dependencies]` enable for its own test builds.

use std::sync::{Arc, Mutex};

/// Installs the process-wide executor the production binary installs at
/// startup. Idempotent: every nextest test runs in its own process, so a
/// test that reaches the global executor must install it itself.
pub fn install_global_executor() {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
}

/// One request a [`serve_api`] server received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    /// The method, e.g. `GET`.
    pub method: String,
    /// The path and query.
    pub path: String,
    /// The `Authorization` header, empty when absent.
    pub authorization: String,
    /// The body, as text.
    pub body: String,
}

/// What a [`serve_api`] server answers one request with.
#[derive(Debug, Clone)]
pub struct ApiReply {
    /// The HTTP status.
    pub status: u16,
    /// Headers beyond `content-type: application/json`.
    pub headers: Vec<(&'static str, String)>,
    /// The body.
    pub body: String,
}

impl ApiReply {
    /// A JSON `body` answered with `status`.
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// The reply with header `name` set to `value`.
    #[must_use]
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// Every request a [`serve_api`] server received, in arrival order.
pub type ApiRequests = Arc<Mutex<Vec<ApiRequest>>>;

/// Serves an HTTP API on a loopback port: `handler` answers each request,
/// given it and the server's base URL. Resolves to the base URL and the
/// log of requests.
///
/// # Panics
///
/// Panics when the loopback listener cannot bind, accept or serve.
pub async fn serve_api(
    handler: impl Fn(&ApiRequest, &str) -> ApiReply + Send + Sync + 'static,
) -> (String, ApiRequests) {
    let handler = Arc::new(handler);
    let requests: ApiRequests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the loopback API");
    let base = format!("http://{}", listener.local_addr().expect("local address"));
    let served_base = base.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            let handler = handler.clone();
            let log = log.clone();
            let base = served_base.clone();
            let service = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| {
                    let handler = handler.clone();
                    let log = log.clone();
                    let base = base.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = http_body_util::BodyExt::collect(body)
                            .await
                            .map_or_default(http_body_util::Collected::to_bytes);
                        let request = ApiRequest {
                            method: parts.method.to_string(),
                            path: parts
                                .uri
                                .path_and_query()
                                .map_or("", |pq| pq.as_str())
                                .to_string(),
                            authorization: parts
                                .headers
                                .get(hyper::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("")
                                .to_string(),
                            body: String::from_utf8_lossy(&body).into_owned(),
                        };
                        let reply = handler(&request, &base);
                        log.lock().expect("requests").push(request);
                        let mut response = hyper::Response::builder()
                            .status(reply.status)
                            .header("content-type", "application/json");
                        for (name, value) in reply.headers {
                            response = response.header(name, value);
                        }
                        Ok::<_, std::convert::Infallible>(
                            response
                                .body(http_body_util::Full::new(hyper::body::Bytes::from(
                                    reply.body,
                                )))
                                .expect("response"),
                        )
                    }
                },
            );
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (base, requests)
}
