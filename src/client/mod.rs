//! Client of the May daemon over its unix socket.
//!
//! The TUI and `may` one-shot runs are clients like the web UI: REST calls
//! for every action and the multiplexed `/api/events` stream for everything
//! the daemon pushes. Each request opens its own connection; the socket is
//! local, so a connection costs microseconds.

mod sse;
mod wire;

pub use sse::{ServerEvent, SseParser};
pub use wire::{RunEnd, SessionAgentBody, SessionAgentFrame, WireAgentError, WireAgentEvent};

use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ui_server::DaemonHealth;

/// Path of the session endpoint `tail` (such as `/stop`) under
/// `/api/sessions/{session_id}`.
#[must_use]
pub fn session_path(session_id: &str, tail: &str) -> String {
    let mut path = String::from("/api/sessions/");
    path.push_str(session_id);
    path.push_str(tail);
    path
}

/// Why a daemon request failed.
#[derive(Debug, thiserror::Error)]
pub enum DaemonClientError {
    /// The socket could not be reached: no daemon is running.
    #[error("the May daemon is not reachable at {}: {source}", .socket.display())]
    Unreachable {
        /// The daemon socket.
        socket: PathBuf,
        /// The connect failure.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP exchange failed.
    #[error("daemon request failed: {0}")]
    Http(#[from] hyper::Error),
    /// The request could not be built.
    #[error("invalid daemon request: {0}")]
    Request(#[from] hyper::http::Error),
    /// The daemon answered with an error status.
    #[error("the May daemon refused {path}: {status} {message}")]
    Status {
        /// Request path.
        path: String,
        /// Response status.
        status: hyper::StatusCode,
        /// Response body.
        message: String,
    },
    /// A body was not the expected JSON.
    #[error("unexpected daemon response: {0}")]
    Json(#[from] serde_json::Error),
}

/// Handle on the daemon's API. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
}

impl DaemonClient {
    /// Client of the daemon listening on `socket`.
    #[must_use]
    pub const fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn send(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<hyper::Response<Incoming>, DaemonClientError> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|source| DaemonClientError::Unreachable {
                socket: self.socket.clone(),
                source,
            })?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "daemon connection closed");
            }
        });
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, "may-daemon");
        if body.is_some() {
            request = request.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        let request = request.body(Full::new(Bytes::from(body.unwrap_or_default())))?;
        let response = sender.send_request(request).await?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let message = response.into_body().collect().await?.to_bytes();
        Err(DaemonClientError::Status {
            path: path.to_string(),
            status,
            message: String::from_utf8_lossy(&message).trim().to_string(),
        })
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, DaemonClientError> {
        let response = self.send(method, path, body).await?;
        let bytes = response.into_body().collect().await?.to_bytes();
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// `GET path`, decoding the JSON answer.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses the request.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, DaemonClientError> {
        self.json(hyper::Method::GET, path, None).await
    }

    /// `POST path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses the request.
    pub async fn post<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, DaemonClientError> {
        self.json(hyper::Method::POST, path, Some(serde_json::to_vec(body)?))
            .await
    }

    /// `PUT path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses the request.
    pub async fn put<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, DaemonClientError> {
        self.json(hyper::Method::PUT, path, Some(serde_json::to_vec(body)?))
            .await
    }

    /// `DELETE path`, decoding the JSON answer.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses the request.
    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, DaemonClientError> {
        self.json(hyper::Method::DELETE, path, None).await
    }

    /// The daemon's health and build.
    ///
    /// # Errors
    /// Fails when no daemon answers.
    pub async fn health(&self) -> Result<DaemonHealth, DaemonClientError> {
        self.get("/api/health").await
    }

    /// Asks the daemon to stop after its running turns.
    ///
    /// # Errors
    /// Fails when no daemon answers.
    pub async fn shutdown(&self) -> Result<(), DaemonClientError> {
        let _: serde_json::Value = self
            .post("/api/daemon/shutdown", &serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Opens `/api/events` for `query` (already URL-encoded, without `?`)
    /// and returns the stream of server events.
    ///
    /// # Errors
    /// Fails when the daemon is unreachable or refuses the subscription.
    pub async fn events(&self, query: &str) -> Result<EventStream, DaemonClientError> {
        let mut path = String::from("/api/events?");
        path.push_str(query);
        let response = self.send(hyper::Method::GET, &path, None).await?;
        Ok(EventStream {
            body: response.into_body(),
            parser: SseParser::default(),
        })
    }
}

/// Server events of one `/api/events` subscription.
#[derive(Debug)]
pub struct EventStream {
    body: Incoming,
    parser: SseParser,
}

impl EventStream {
    /// The next event; `None` once the daemon closed the stream.
    ///
    /// # Errors
    /// Fails when the connection breaks.
    pub async fn next(&mut self) -> Result<Option<ServerEvent>, DaemonClientError> {
        loop {
            if let Some(event) = self.parser.next_event() {
                return Ok(Some(event));
            }
            match self.body.frame().await {
                Some(frame) => {
                    if let Ok(data) = frame?.into_data() {
                        self.parser.push(&data);
                    }
                }
                None => return Ok(None),
            }
        }
    }
}
