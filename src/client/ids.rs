//! A session's model-facing ids leased from the daemon's pool, for a
//! process that is not the daemon (`may exec`).
//!
//! The daemon owns every session's pool. [`DaemonIds`] holds one connection
//! to its unix socket, and every lease it takes is held by that connection:
//! the daemon drops a connection's leases when the connection closes, so a
//! process that exits or crashes never leaves a word leased. A lease the
//! process [keeps](crate::model_ids::StoredIds::keep) for a stored id moves
//! to the session's stored leases, which outlive the connection.
//!
//! [`IdAllocator`] is synchronous, so each lease is one blocking round trip
//! on a local socket, served by a thread of its own that drives the
//! connection outside any async runtime.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aither_core::{AnyIdAllocator, IdAllocator, IdLease, IdsExhausted, WordUnavailable};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Method, StatusCode};

use crate::model_ids::{SessionIds, StoredIds};

/// `POST /api/sessions/{id}/ids` request body.
#[derive(Debug, serde::Serialize)]
struct LeaseRequest<'a> {
    /// The word to claim; any free word when absent.
    word: Option<&'a str>,
}

/// `POST /api/sessions/{id}/ids` response body.
#[derive(Debug, serde::Deserialize)]
struct Leased {
    word: String,
}

/// What the daemon answered.
struct Answer {
    status: StatusCode,
    body: Bytes,
}

/// One request on the connection, answered on `reply` when it has one.
struct Exchange {
    method: Method,
    path: String,
    body: Option<Vec<u8>>,
    reply: Option<async_channel::Sender<Result<Answer, String>>>,
}

/// One connection to the daemon that every lease of this process goes
/// through. Clones share it; it closes when the last clone and every pool
/// built from it drop.
#[derive(Debug, Clone)]
pub struct DaemonIds {
    exchanges: async_channel::Sender<Exchange>,
}

impl DaemonIds {
    /// Connects to the daemon listening on `socket`.
    ///
    /// # Errors
    /// The socket cannot be reached, or the connection's thread cannot start.
    pub async fn connect(socket: &Path) -> std::io::Result<Self> {
        let stream = async_net::unix::UnixStream::connect(socket).await?;
        let (exchanges, requests) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("may-daemon-ids".to_string())
            .spawn(move || futures_lite::future::block_on(serve(stream, requests)))?;
        Ok(Self { exchanges })
    }

    /// The pool of `session_id`, leasing from the daemon's.
    #[must_use]
    pub fn session(&self, session_id: &str) -> SessionIds {
        let pool = SessionPool(Arc::new(PoolState {
            daemon: self.clone(),
            session_id: session_id.to_string(),
            kept: Mutex::new(HashSet::new()),
        }));
        SessionIds::new(AnyIdAllocator::new(pool.clone()), pool)
    }

    /// Sends a request and waits for its answer.
    ///
    /// # Panics
    /// When the connection to the daemon failed: this process's leases went
    /// with it.
    fn round_trip(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Answer {
        let (reply, answer) = async_channel::bounded(1);
        let sent = self.exchanges.send_blocking(Exchange {
            method,
            path: path.to_string(),
            body,
            reply: Some(reply),
        });
        let answer = sent
            .ok()
            .and_then(|()| answer.recv_blocking().ok())
            .unwrap_or_else(|| Err("the connection closed".to_string()));
        answer.unwrap_or_else(|error| {
            panic!("the May daemon connection holding this process's ids failed on {path}: {error}")
        })
    }
}

/// The state one session's pool shares between its allocator and its
/// stored leases.
struct PoolState {
    daemon: DaemonIds,
    session_id: String,
    /// Words kept for stored ids: the daemon holds them for the session, so
    /// dropping their connection leases releases nothing.
    kept: Mutex<HashSet<String>>,
}

impl PoolState {
    fn path(&self, tail: &str) -> String {
        super::session_path(&self.session_id, tail)
    }

    fn kept(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.kept.lock().expect("kept session ids lock poisoned")
    }

    /// Leases `word`, or any free word, on the connection.
    fn lease(&self, word: Option<&str>) -> Option<String> {
        let body = serde_json::to_vec(&LeaseRequest { word }).expect("a lease request serializes");
        let path = self.path("/ids");
        let answer = self.daemon.round_trip(Method::POST, &path, Some(body));
        match answer.status {
            StatusCode::OK => {
                let leased: Leased = serde_json::from_slice(&answer.body).unwrap_or_else(|error| {
                    panic!("the May daemon answered {path} with an unexpected body: {error}")
                });
                Some(leased.word)
            }
            StatusCode::CONFLICT => None,
            status => refused(&path, status, &answer.body),
        }
    }

    /// Sends a request whose answer carries nothing but its status.
    fn expect_success(&self, method: Method, tail: &str) {
        let path = self.path(tail);
        let answer = self.daemon.round_trip(method, &path, None);
        if !answer.status.is_success() {
            refused(&path, answer.status, &answer.body);
        }
    }
}

fn refused(path: &str, status: StatusCode, body: &[u8]) -> ! {
    panic!(
        "the May daemon refused {path}: {status} {}",
        String::from_utf8_lossy(body).trim()
    )
}

/// One session's pool in the daemon, as this process leases from it.
#[derive(Clone)]
struct SessionPool(Arc<PoolState>);

impl IdAllocator for SessionPool {
    fn lease(&self) -> Result<String, IdsExhausted> {
        self.0.lease(None).ok_or(IdsExhausted)
    }

    fn claim(&self, word: &str) -> Result<(), WordUnavailable> {
        let leased = self
            .0
            .lease(Some(word))
            .ok_or_else(|| WordUnavailable(word.to_string()))?;
        assert_eq!(
            leased, word,
            "the May daemon leased another word on a claim"
        );
        Ok(())
    }

    fn release(&self, word: &str) {
        if self.0.kept().remove(word) {
            return;
        }
        let mut tail = String::from("/ids/");
        tail.push_str(word);
        // No answer is awaited: a closed connection already released it.
        let sent = self.0.daemon.exchanges.try_send(Exchange {
            method: Method::DELETE,
            path: self.0.path(&tail),
            body: None,
            reply: None,
        });
        if sent.is_err() {
            tracing::debug!(
                word,
                "the daemon id connection is closed; its leases went with it"
            );
        }
    }
}

impl StoredIds for SessionPool {
    fn keep(&self, lease: IdLease) {
        let word = lease.to_string();
        self.0.kept().insert(word.clone());
        let mut tail = String::from("/ids/");
        tail.push_str(&word);
        tail.push_str("/keep");
        self.0.expect_success(Method::POST, &tail);
        drop(lease);
    }

    fn release(&self, word: &str) {
        let mut tail = String::from("/stored-ids/");
        tail.push_str(word);
        self.0.expect_success(Method::DELETE, &tail);
    }
}

/// Serves `requests` on one HTTP/1.1 connection over `stream`, in order,
/// until every sender is gone or the connection fails.
async fn serve(stream: async_net::unix::UnixStream, requests: async_channel::Receiver<Exchange>) {
    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(smol_hyper::rt::FuturesIo::new(stream)).await {
            Ok(handshake) => handshake,
            Err(error) => {
                tracing::error!(%error, "the daemon id connection failed its handshake");
                return;
            }
        };
    let exchanges = async move {
        while let Ok(exchange) = requests.recv().await {
            let answer = send(&mut sender, &exchange).await;
            match exchange.reply {
                Some(reply) => {
                    let _ = reply.send(answer).await;
                }
                None => match answer {
                    Ok(answer) if answer.status.is_success() => {}
                    Ok(answer) => refused(&exchange.path, answer.status, &answer.body),
                    Err(error) => {
                        tracing::error!(path = exchange.path, %error, "daemon id request failed");
                        return;
                    }
                },
            }
        }
    };
    futures_lite::future::or(
        async {
            if let Err(error) = connection.await {
                tracing::error!(%error, "the daemon id connection closed");
            }
        },
        exchanges,
    )
    .await;
}

async fn send(
    sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    exchange: &Exchange,
) -> Result<Answer, String> {
    let mut request = hyper::Request::builder()
        .method(exchange.method.clone())
        .uri(exchange.path.as_str())
        .header(hyper::header::HOST, "may-daemon");
    if exchange.body.is_some() {
        request = request.header(hyper::header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(Full::new(Bytes::from(
            exchange.body.clone().unwrap_or_default(),
        )))
        .map_err(|error| error.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|error| error.to_string())?
        .to_bytes();
    Ok(Answer { status, body })
}
