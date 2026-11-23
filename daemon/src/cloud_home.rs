//! What a cloud home's daemon takes from and exchanges with the daemon fronting it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Response, Result as SkyResult, StatusCode};

use super::auth::Fronted;
use super::cloud_sessions::Endpoint;
use super::error::{bad_request_error, internal_error};
use super::state::DaemonState;
use crate::agent_link::{
    PeerBtwRequest, PeerBtwResponse, PeerErrorBody, PeerSendRequest, RemoteListing,
};
use crate::cloud::access::{ProviderPush, PushedTiers};

/// Long-poll window for `GET /api/cloud-home/peer-rpc`.
const BRIDGE_POLL_WINDOW: Duration = Duration::from_secs(15);

/// Reverse peer operation initiated by an agent inside a cloud home and served
/// by its fronting daemon over the active forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeRpcOp {
    /// Discover sessions on the fronting daemon, sibling cloud homes, and tailnet peers.
    List {
        /// Session inside this cloud home requesting the listing.
        caller_session_id: String,
    },
    /// Send a message from this cloud home to `machine:target_session_id`.
    Send {
        /// Destination machine prefix.
        machine: String,
        /// Bare session id on `machine`.
        target_session_id: String,
        /// Message payload.
        request: PeerSendRequest,
    },
    /// Ask a confined read-only `btw` question from this cloud home to `machine:target_session_id`.
    Btw {
        /// Destination machine prefix.
        machine: String,
        /// Bare session id on `machine`.
        target_session_id: String,
        /// Question payload.
        request: PeerBtwRequest,
    },
}

/// Wire envelope returned by `GET /api/cloud-home/peer-rpc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRpcRequest {
    /// Correlation id matched by `POST /api/cloud-home/peer-rpc/{id}`.
    pub id: u64,
    /// Requested cross-machine operation.
    pub op: BridgeRpcOp,
}

/// Wire envelope posted back to `POST /api/cloud-home/peer-rpc/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum BridgeRpcResult {
    /// Listing result.
    List(RemoteListing),
    /// `agent send` outcome (`Ok(recipient_name)` or structured peer error).
    Send(Result<String, PeerErrorBody>),
    /// `btw` outcome (`Ok(PeerBtwResponse)` or structured peer error).
    Btw(Result<PeerBtwResponse, PeerErrorBody>),
}

/// Cloud-home side reverse RPC queue answered by the fronting daemon while its
/// port forward is connected.
#[derive(Clone)]
pub struct CloudHomeBridge {
    enabled: bool,
    next_id: Arc<AtomicU64>,
    tx: async_channel::Sender<BridgeRpcRequest>,
    rx: async_channel::Receiver<BridgeRpcRequest>,
    pending: Arc<DashMap<u64, async_channel::Sender<BridgeRpcResult>>>,
}

impl std::fmt::Debug for CloudHomeBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudHomeBridge")
            .field("enabled", &self.enabled)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Default for CloudHomeBridge {
    fn default() -> Self {
        Self::new(false)
    }
}

impl CloudHomeBridge {
    /// Creates a bridge (`enabled` is true when `~/.may/cloud-home.json` is present).
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        let (tx, rx) = async_channel::bounded(64);
        Self {
            enabled,
            next_id: Arc::new(AtomicU64::new(1)),
            tx,
            rx,
            pending: Arc::new(DashMap::new()),
        }
    }

    /// Whether this daemon runs inside a fronted cloud home.
    #[must_use]
    pub const fn is_cloud_home(&self) -> bool {
        self.enabled
    }

    /// Enqueues `op` for the fronting daemon and awaits its answer up to `timeout`.
    pub async fn call(
        &self,
        op: BridgeRpcOp,
        timeout: Duration,
    ) -> Result<BridgeRpcResult, String> {
        if !self.enabled {
            return Err("this daemon is not a fronted cloud home".to_string());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        self.pending.insert(id, reply_tx);
        if self.tx.try_send(BridgeRpcRequest { id, op }).is_err() {
            self.pending.remove(&id);
            return Err("fronting daemon bridge queue is full or disconnected".to_string());
        }
        let waited = futures_lite::future::or(
            async {
                reply_rx
                    .recv()
                    .await
                    .map_err(|_| "bridge closed".to_string())
            },
            async {
                async_io::Timer::after(timeout).await;
                Err("fronting daemon did not answer before timeout".to_string())
            },
        )
        .await;
        if waited.is_err() {
            self.pending.remove(&id);
        }
        waited
    }

    async fn poll_next(&self, timeout: Duration) -> Option<BridgeRpcRequest> {
        futures_lite::future::or(async { self.rx.recv().await.ok() }, async {
            async_io::Timer::after(timeout).await;
            None
        })
        .await
    }

    async fn resolve(&self, id: u64, result: BridgeRpcResult) -> bool {
        if let Some((_, tx)) = self.pending.remove(&id) {
            let _ = tx.send(result).await;
            true
        } else {
            false
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Callback registered on `CloudSessions` so active cloud forwards service
/// reverse peer RPCs originating inside their codespaces.
pub struct BridgeDispatcher(
    Box<dyn Fn(String, BridgeRpcOp) -> BoxFuture<'static, BridgeRpcResult> + Send + Sync>,
);

impl std::fmt::Debug for BridgeDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeDispatcher").finish_non_exhaustive()
    }
}

impl BridgeDispatcher {
    /// Wraps an async handler `(origin_cloud_session_id, op) -> BridgeRpcResult`.
    pub fn new<F, Fut>(handler: F) -> Self
    where
        F: Fn(String, BridgeRpcOp) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BridgeRpcResult> + Send + 'static,
    {
        Self(Box::new(move |origin, op| Box::pin(handler(origin, op))))
    }

    async fn dispatch(&self, origin_session_id: String, op: BridgeRpcOp) -> BridgeRpcResult {
        (self.0)(origin_session_id, op).await
    }
}

/// Runs alongside `Mirror::follow` while a cloud session's forward is open,
/// polling `GET /api/cloud-home/peer-rpc` and posting answers back.
pub async fn follow_peer_bridge(
    endpoint: Endpoint,
    origin_session_id: String,
    dispatcher: Arc<arc_swap::ArcSwapOption<BridgeDispatcher>>,
) {
    loop {
        let Ok((status, body)) = endpoint
            .exchange(hyper::Method::GET, "/api/cloud-home/peer-rpc", None)
            .await
        else {
            return;
        };
        if status == StatusCode::NO_CONTENT {
            continue;
        }
        if !status.is_success() {
            return;
        }
        let Ok(req) = serde_json::from_slice::<BridgeRpcRequest>(&body) else {
            continue;
        };
        let Some(handler) = dispatcher.load_full() else {
            async_io::Timer::after(Duration::from_millis(100)).await;
            continue;
        };
        let endpoint_clone = endpoint.clone();
        let origin = origin_session_id.clone();
        executor_core::spawn(async move {
            let result = handler.dispatch(origin, req.op).await;
            if let Ok(encoded) = serde_json::to_vec(&result) {
                let reply_path = format!("/api/cloud-home/peer-rpc/{}", req.id);
                let _ = endpoint_clone
                    .exchange(hyper::Method::POST, &reply_path, Some(encoded))
                    .await;
            }
        })
        .detach();
    }
}

/// `PUT /api/cloud-home/providers/{id}`: the provider `id` as the fronting
/// daemon pushed it. Only the fronting daemon's token is admitted.
pub async fn push_provider(
    params: Params,
    fronted: Fronted,
    State(state): State<Arc<DaemonState>>,
    Json(push): Json<ProviderPush>,
) -> SkyResult<StatusCode> {
    if !fronted.0 {
        return Err(
            SkyError::msg("only the daemon fronting this cloud home pushes its providers")
                .set_status(StatusCode::FORBIDDEN),
        );
    }
    let id = params.get("id").map_err(bad_request_error)?;
    let provider_type = push.instance.resolve_type(id);
    if let Some(token) = &push.access
        && token.provider_type() != provider_type
    {
        return Err(bad_request_error(format!(
            "provider '{id}' is {provider_type:?}, but the access token is for {:?}",
            token.provider_type()
        )));
    }
    state.pushed_providers.push(id, push);
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/cloud-home/tiers`: the fronting daemon's fast and balanced
/// model tiers. Only the fronting daemon's token is admitted.
pub async fn push_tiers(
    fronted: Fronted,
    State(state): State<Arc<DaemonState>>,
    Json(tiers): Json<PushedTiers>,
) -> SkyResult<StatusCode> {
    if !fronted.0 {
        return Err(SkyError::msg(
            "only the daemon fronting this cloud home pushes its model tiers",
        )
        .set_status(StatusCode::FORBIDDEN));
    }
    state.pushed_providers.set_tiers(tiers);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/cloud-home/peer-rpc`: long-polls the next cross-machine agent
/// request initiated inside this cloud home. Only the fronting daemon is admitted.
pub async fn poll_peer_rpc(
    fronted: Fronted,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Response> {
    if !fronted.0 {
        return Err(SkyError::msg(
            "only the daemon fronting this cloud home polls its peer bridge",
        )
        .set_status(StatusCode::FORBIDDEN));
    }
    let Some(next) = state.cloud_home_bridge.poll_next(BRIDGE_POLL_WINDOW).await else {
        let mut empty = Response::new(skyzen::Body::empty());
        *empty.status_mut() = StatusCode::NO_CONTENT;
        return Ok(empty);
    };
    let bytes = serde_json::to_vec(&next).map_err(internal_error)?;
    let mut response = Response::new(skyzen::Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

/// `POST /api/cloud-home/peer-rpc/{id}`: delivers the fronting daemon's answer
/// to a pending cloud-home peer request.
pub async fn reply_peer_rpc(
    params: Params,
    fronted: Fronted,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<StatusCode> {
    if !fronted.0 {
        return Err(SkyError::msg(
            "only the daemon fronting this cloud home answers its peer bridge",
        )
        .set_status(StatusCode::FORBIDDEN));
    }
    let raw_id = params.get("id").map_err(bad_request_error)?;
    let id: u64 = raw_id
        .parse()
        .map_err(|_| bad_request_error("invalid peer-rpc request id"))?;
    let result: BridgeRpcResult = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let _ = state.cloud_home_bridge.resolve(id, result).await;
    Ok(StatusCode::NO_CONTENT)
}
