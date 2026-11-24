//! Cross-machine swarm transport, `/api/events?topics=swarm` SSE mirror
//! followers, suspended-codespace notice buffering, and `/api/peers/swarm/*` routes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures_util::TryStreamExt as _;
use http_body_util::BodyDataStream;
use skyzen::utils::State;
use skyzen::{Response, Result as SkyResult, StatusCode};
use zenwave::Client as _;

use super::auth::PeerCaller;
use super::cloud_home::{BridgeRpcOp, BridgeRpcResult};
use super::cloud_sessions::{CloudHost, CloudSessionError, Endpoint};
use super::error::bad_request_error;
use super::peers::{
    DaemonPeerFabric, check_same_owner, http_json, peer_json_response, with_timeout,
};
use super::state::DaemonState;
use crate::agent_link::{Activity, CLOUD_MACHINE};
use crate::db::CloudSessionRow;
use crate::swarm::{
    PeerSwarmDeliverRequest, PeerSwarmDeliverResponse, PeerSwarmErrorBody, PeerSwarmPostRequest,
    PeerSwarmPostResponse, PeerSwarmPrRequest, PeerSwarmReadRequest, PeerSwarmReadResponse,
    PeerSwarmStateResponse, PeerSwarmWorld, RemoteWorkingSession, SwarmBoardSummary,
    SwarmBoardsSsePayload, SwarmError, SwarmTransport,
};
use crate::ui_event_bus::UiEventTopics;

/// Buffer of routine PR / lifecycle swarm notices destined for `Suspended`
/// Codespaces cloud homes, flushed when the codespace next starts without ever
/// booting a sleeping VM for routine PR chatter.
#[derive(Debug, Clone, Default)]
pub struct BufferedCloudNotices {
    queues: Arc<DashMap<String, Vec<PeerSwarmDeliverRequest>>>,
}

impl BufferedCloudNotices {
    /// Appends `notice` to `session_id`'s pending cloud buffer.
    pub fn push(&self, session_id: &str, notice: PeerSwarmDeliverRequest) {
        self.queues
            .entry(session_id.to_string())
            .or_default()
            .push(notice);
    }

    /// Takes all buffered notices queued for `session_id`.
    #[must_use]
    pub fn drain_session(&self, session_id: &str) -> Vec<PeerSwarmDeliverRequest> {
        self.queues
            .remove(session_id)
            .map_or_else(Vec::new, |(_, items)| items)
    }

    /// Number of buffered notices currently waiting for `session_id`.
    #[must_use]
    pub fn buffered_len(&self, session_id: &str) -> usize {
        self.queues.get(session_id).map_or(0, |v| v.len())
    }

    /// Drops buffered `Notice` entries belonging to a dissolved `group_key`.
    pub fn drop_dissolved_group(&self, group_key: &str) {
        for mut entry in self.queues.iter_mut() {
            entry.value_mut().retain(|req| match req {
                PeerSwarmDeliverRequest::Notice { group_key: g, .. } => g != group_key,
                _ => true,
            });
        }
    }
}

struct FollowerEntry {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FollowerEntry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Live `/api/events?topics=swarm` SSE mirror followers per reachable tailnet peer.
#[derive(Clone, Default)]
pub struct SwarmMirrorState {
    mirrored_boards: Arc<DashMap<String, Vec<SwarmBoardSummary>>>,
    followers: Arc<DashMap<String, FollowerEntry>>,
}

impl std::fmt::Debug for SwarmMirrorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwarmMirrorState")
            .field("peers", &self.mirrored_boards.len())
            .finish_non_exhaustive()
    }
}

impl SwarmMirrorState {
    /// Snapshot of all boards mirrored from remote SSE streams.
    #[must_use]
    pub fn snapshot_boards(&self) -> Vec<SwarmBoardSummary> {
        self.mirrored_boards
            .iter()
            .flat_map(|entry| entry.value().clone())
            .collect()
    }

    /// Purges cached mirrored boards for an unreachable peer.
    pub fn purge_peer(&self, machine: &str) {
        self.mirrored_boards.remove(machine);
    }

    /// Ensures each peer in `endpoints` has a running `/api/events?topics=swarm`
    /// SSE follower and tears down followers for removed peers.
    pub fn reconcile_followers(
        &self,
        state: &Arc<DaemonState>,
        endpoints: &BTreeMap<String, String>,
    ) {
        let removed_peers: Vec<String> = self
            .followers
            .iter()
            .filter(|e| endpoints.get(e.key()) != Some(&e.value().base_url))
            .map(|e| e.key().clone())
            .collect();
        for machine in removed_peers {
            self.followers.remove(&machine);
            self.mirrored_boards.remove(&machine);
        }

        for (machine, base_url) in endpoints {
            if self.followers.contains_key(machine) {
                continue;
            }
            let state_clone = state.clone();
            let mirrors = self.mirrored_boards.clone();
            let machine_owned = machine.clone();
            let url_owned = base_url.clone();
            let shutdown = state.daemon_shutdown.clone();
            let task = state.tokio_handle.spawn(async move {
                follow_peer_swarm_sse(state_clone, mirrors, machine_owned, url_owned, shutdown)
                    .await;
            });
            self.followers.insert(
                machine.clone(),
                FollowerEntry {
                    base_url: base_url.clone(),
                    task,
                },
            );
        }
    }
}

async fn follow_peer_swarm_sse(
    state: Arc<DaemonState>,
    mirrors: Arc<DashMap<String, Vec<SwarmBoardSummary>>>,
    machine: String,
    base_url: String,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let sse_url = format!("{base_url}/api/events?topics=swarm");
    loop {
        if !state.config_snapshot().experimental.swarm {
            tokio::select! {
                () = shutdown.notified() => return,
                () = tokio::time::sleep(Duration::from_secs(2)) => continue,
            }
        }
        let connect_fut = stream_peer_sse_once(&state, &mirrors, &machine, &sse_url);
        let disconnected = tokio::select! {
            () = shutdown.notified() => return,
            res = connect_fut => res,
        };
        // Peer disconnected or went offline: purge its mirrored boards and trigger
        // immediate failover reconciliation so the next host bumps `board_epoch` (or
        // dissolves when `<= 1` working sessions remain).
        if mirrors.remove(&machine).is_some()
            && let Some(swarm) = state.services.swarm()
        {
            swarm.apply_mirrored_boards(&machine, Vec::new()).await;
            let _ = swarm.reconcile().await;
        }
        if disconnected.is_err() {
            tokio::select! {
                () = shutdown.notified() => return,
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }
}

async fn stream_peer_sse_once(
    state: &Arc<DaemonState>,
    mirrors: &Arc<DashMap<String, Vec<SwarmBoardSummary>>>,
    machine: &str,
    sse_url: &str,
) -> Result<(), String> {
    let mut client = zenwave::client();
    let builder = client
        .method(zenwave::Method::GET, sse_url)
        .map_err(|e| e.to_string())?;
    let response = builder.await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("SSE status {}", response.status()));
    }
    let mut stream = BodyDataStream::new(response.into_body());
    let mut parser = crate::client::SseParser::default();
    while let Some(chunk) = stream.try_next().await.map_err(|e| e.to_string())? {
        parser.push(&chunk);
        while let Some(event) = parser.next_event() {
            if event.kind == "swarm"
                && let Ok(payload) = serde_json::from_str::<SwarmBoardsSsePayload>(&event.data)
            {
                mirrors.insert(machine.to_string(), payload.boards.clone());
                if let Some(swarm) = state.services.swarm() {
                    swarm.apply_mirrored_boards(machine, payload.boards).await;
                }
            }
        }
    }
    Err("SSE stream closed".to_string())
}

/// Flushes any buffered swarm notices for `session_id` over `endpoint` once its
/// Codespace forward becomes active.
pub async fn flush_cloud_session_notices(
    state: &DaemonState,
    session_id: &str,
    endpoint: &Endpoint,
) {
    let pending = state.buffered_cloud_notices.drain_session(session_id);
    if pending.is_empty() {
        return;
    }
    let timeout = Duration::from_millis(
        state
            .config_snapshot()
            .remote_access
            .peer_discovery_timeout_ms
            .max(100),
    );
    for item in pending {
        if let Ok(bytes) = serde_json::to_vec(&item) {
            let _ = with_timeout(
                timeout,
                endpoint.exchange(hyper::Method::POST, "/api/peers/swarm/deliver", Some(bytes)),
            )
            .await;
        }
    }
}

/// Cross-machine [`SwarmTransport`] backed by [`DaemonState`].
#[derive(Clone)]
pub struct DaemonSwarmTransport {
    state: Arc<DaemonState>,
}

impl std::fmt::Debug for DaemonSwarmTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonSwarmTransport")
            .finish_non_exhaustive()
    }
}

impl DaemonSwarmTransport {
    /// Creates a [`DaemonSwarmTransport`] bound to `state`.
    #[must_use]
    pub const fn new(state: Arc<DaemonState>) -> Self {
        Self { state }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(
            self.state
                .config_snapshot()
                .remote_access
                .peer_discovery_timeout_ms
                .max(100),
        )
    }

    /// Collects the host-side [`PeerSwarmWorld`] across tailnet peers and fronted cloud homes.
    pub(crate) async fn poll_host_world(
        &self,
        exclude_cloud_session: Option<&str>,
    ) -> PeerSwarmWorld {
        let peer_fabric = DaemonPeerFabric::new(self.state.clone());
        let (local_machine, endpoints) = peer_fabric.resolve_peer_endpoints().await;
        self.state
            .swarm_mirrors
            .reconcile_followers(&self.state, &endpoints);
        let timeout = self.timeout();

        let cloud_hosts: Vec<(Arc<CloudSessionRow>, CloudHost)> = self
            .state
            .cloud_sessions
            .hosts_snapshot()
            .into_iter()
            .filter(|(row, _)| exclude_cloud_session != Some(row.session_id.as_str()))
            .collect();

        let local_working = self
            .state
            .db()
            .working_session_ids()
            .await
            .unwrap_or_default();

        let cloud_futs = cloud_hosts.into_iter().map(|(row, host)| {
            let state = self.state.clone();
            let voter = local_machine.clone();
            let shadowed_started = local_working.contains(&row.session_id);
            async move {
                probe_cloud_swarm_sessions(&state, &row, &host, &voter, shadowed_started, timeout)
                    .await
            }
        });

        let tailnet_futs = endpoints.into_iter().map(|(machine, base_url)| async move {
            let state_opt = fetch_peer_swarm_state(&machine, &base_url, timeout).await;
            (machine, state_opt)
        });

        let (cloud_lists, tailnet_states) = futures_lite::future::zip(
            futures_util::future::join_all(cloud_futs),
            futures_util::future::join_all(tailnet_futs),
        )
        .await;

        let mut remote_sessions = Vec::new();
        for list in cloud_lists {
            remote_sessions.extend(list);
        }

        for (machine, state_opt) in &tailnet_states {
            if state_opt.is_none() {
                self.state.swarm_mirrors.purge_peer(machine);
            }
        }

        let mut remote_boards_map: BTreeMap<String, SwarmBoardSummary> = BTreeMap::new();
        for summary in self.state.swarm_mirrors.snapshot_boards() {
            remote_boards_map.insert(summary.group_key.clone(), summary);
        }

        for (_, state_opt) in tailnet_states {
            if let Some(state_resp) = state_opt {
                remote_sessions.extend(state_resp.sessions);
                for mut board in state_resp.hosted_boards {
                    board.host_machine.clone_from(&state_resp.machine);
                    remote_boards_map.insert(board.group_key.clone(), board);
                }
            }
        }

        PeerSwarmWorld {
            local_machine,
            is_cloud_home: false,
            remote_sessions,
            remote_boards: remote_boards_map.into_values().collect(),
        }
    }

    pub(crate) async fn read_host_side(
        &self,
        host_machine: &str,
        request: PeerSwarmReadRequest,
    ) -> Result<PeerSwarmReadResponse, SwarmError> {
        let (local_machine, endpoints) = DaemonPeerFabric::new(self.state.clone())
            .resolve_peer_endpoints()
            .await;
        if host_machine == local_machine {
            let swarm = self
                .state
                .services
                .swarm()
                .ok_or(SwarmError::NoActiveBoard)?;
            return swarm.serve_host_read(request).await;
        }
        let Some(base_url) = endpoints.get(host_machine) else {
            return Err(SwarmError::Remote(format!(
                "Board Host '{host_machine}' is unreachable"
            )));
        };
        let url = format!("{base_url}/api/peers/swarm/read");
        let payload =
            serde_json::to_vec(&request).map_err(|e| SwarmError::Remote(e.to_string()))?;
        let (status, bytes) = http_json(zenwave::Method::POST, &url, Some(payload), self.timeout())
            .await
            .map_err(SwarmError::Remote)?;
        decode_swarm_response(status, &bytes)
    }

    pub(crate) async fn post_host_side(
        &self,
        host_machine: &str,
        request: PeerSwarmPostRequest,
    ) -> Result<PeerSwarmPostResponse, SwarmError> {
        let (local_machine, endpoints) = DaemonPeerFabric::new(self.state.clone())
            .resolve_peer_endpoints()
            .await;
        if host_machine == local_machine {
            let swarm = self
                .state
                .services
                .swarm()
                .ok_or(SwarmError::NoActiveBoard)?;
            return swarm.serve_host_post(request).await;
        }
        let Some(base_url) = endpoints.get(host_machine) else {
            return Err(SwarmError::Remote(format!(
                "Board Host '{host_machine}' is unreachable"
            )));
        };
        let url = format!("{base_url}/api/peers/swarm/post");
        let payload =
            serde_json::to_vec(&request).map_err(|e| SwarmError::Remote(e.to_string()))?;
        let (status, bytes) = http_json(zenwave::Method::POST, &url, Some(payload), self.timeout())
            .await
            .map_err(SwarmError::Remote)?;
        decode_swarm_response(status, &bytes)
    }

    pub(crate) async fn pr_host_side(
        &self,
        host_machine: &str,
        request: PeerSwarmPrRequest,
    ) -> Result<(), SwarmError> {
        let (local_machine, endpoints) = DaemonPeerFabric::new(self.state.clone())
            .resolve_peer_endpoints()
            .await;
        if host_machine == local_machine {
            let swarm = self
                .state
                .services
                .swarm()
                .ok_or(SwarmError::NoActiveBoard)?;
            return swarm.serve_host_pr(request).await;
        }
        let Some(base_url) = endpoints.get(host_machine) else {
            return Err(SwarmError::Remote(format!(
                "Board Host '{host_machine}' is unreachable"
            )));
        };
        let url = format!("{base_url}/api/peers/swarm/pr");
        let payload =
            serde_json::to_vec(&request).map_err(|e| SwarmError::Remote(e.to_string()))?;
        let (status, bytes) = http_json(zenwave::Method::POST, &url, Some(payload), self.timeout())
            .await
            .map_err(SwarmError::Remote)?;
        decode_swarm_response(status, &bytes)
    }

    pub(crate) async fn deliver_host_side(
        &self,
        target_machine: &str,
        request: PeerSwarmDeliverRequest,
    ) -> Result<PeerSwarmDeliverResponse, SwarmError> {
        let timeout = self.timeout();
        if target_machine == CLOUD_MACHINE {
            return self.deliver_to_cloud_home(request, timeout).await;
        }
        let (local_machine, endpoints) = DaemonPeerFabric::new(self.state.clone())
            .resolve_peer_endpoints()
            .await;
        if target_machine == local_machine {
            let swarm = self
                .state
                .services
                .swarm()
                .ok_or(SwarmError::NoActiveBoard)?;
            return swarm.receive_peer_delivery(request).await;
        }
        let Some(base_url) = endpoints.get(target_machine) else {
            return Err(SwarmError::Remote(format!(
                "peer '{target_machine}' is unreachable"
            )));
        };
        let url = format!("{base_url}/api/peers/swarm/deliver");
        let payload =
            serde_json::to_vec(&request).map_err(|e| SwarmError::Remote(e.to_string()))?;
        let (status, bytes) = http_json(zenwave::Method::POST, &url, Some(payload), timeout)
            .await
            .map_err(SwarmError::Remote)?;
        decode_swarm_response(status, &bytes)
    }

    async fn deliver_to_cloud_home(
        &self,
        request: PeerSwarmDeliverRequest,
        timeout: Duration,
    ) -> Result<PeerSwarmDeliverResponse, SwarmError> {
        match &request {
            PeerSwarmDeliverRequest::Mention {
                target_session_id, ..
            } => {
                let Some(host) = self.state.cloud_sessions.host(target_session_id) else {
                    return Err(SwarmError::Remote(format!(
                        "unknown cloud session '{target_session_id}'"
                    )));
                };
                // Explicit `@mention` boots a `Suspended` codespace (`wake = true`).
                let wake_timeout = timeout.max(Duration::from_secs(60));
                let endpoint = with_timeout(wake_timeout, host.endpoint(true))
                    .await
                    .map_err(|()| {
                        SwarmError::Remote("timed out starting suspended codespace".to_string())
                    })?
                    .map_err(|e| SwarmError::Remote(e.to_string()))?;
                flush_cloud_session_notices(&self.state, target_session_id, &endpoint).await;
                let payload =
                    serde_json::to_vec(&request).map_err(|e| SwarmError::Remote(e.to_string()))?;
                let (status, bytes) = with_timeout(
                    timeout,
                    endpoint.exchange(
                        hyper::Method::POST,
                        "/api/peers/swarm/deliver",
                        Some(payload),
                    ),
                )
                .await
                .map_err(|()| SwarmError::Remote("timed out delivering cloud mention".to_string()))?
                .map_err(|e| SwarmError::Remote(e.to_string()))?;
                decode_swarm_response(status, &bytes)
            }
            PeerSwarmDeliverRequest::Notice {
                target_session_id, ..
            } => {
                let target_id = target_session_id.clone();
                let Some(host) = self.state.cloud_sessions.host(&target_id) else {
                    return Ok(PeerSwarmDeliverResponse::default());
                };
                // Routine PR / lifecycle notices NEVER wake a suspended codespace (`wake = false`).
                match with_timeout(timeout, host.endpoint(false)).await {
                    Ok(Ok(endpoint)) => {
                        flush_cloud_session_notices(&self.state, &target_id, &endpoint).await;
                        let payload = serde_json::to_vec(&request)
                            .map_err(|e| SwarmError::Remote(e.to_string()))?;
                        let (status, bytes) = with_timeout(
                            timeout,
                            endpoint.exchange(
                                hyper::Method::POST,
                                "/api/peers/swarm/deliver",
                                Some(payload),
                            ),
                        )
                        .await
                        .map_err(|()| {
                            SwarmError::Remote("timed out delivering cloud notice".to_string())
                        })?
                        .map_err(|e| SwarmError::Remote(e.to_string()))?;
                        decode_swarm_response(status, &bytes)
                    }
                    Ok(Err(CloudSessionError::Suspended)) => {
                        self.state.buffered_cloud_notices.push(&target_id, request);
                        Ok(PeerSwarmDeliverResponse {
                            woken: false,
                            capped: false,
                        })
                    }
                    Ok(Err(_)) | Err(()) => {
                        self.state.buffered_cloud_notices.push(&target_id, request);
                        Ok(PeerSwarmDeliverResponse::default())
                    }
                }
            }
            PeerSwarmDeliverRequest::Dissolved { group_key, .. } => {
                self.state
                    .buffered_cloud_notices
                    .drop_dissolved_group(group_key);
                for (row, host) in self.state.cloud_sessions.hosts_snapshot() {
                    if let Ok(Ok(endpoint)) = with_timeout(timeout, host.endpoint(false)).await
                        && let Ok(payload) = serde_json::to_vec(&request)
                    {
                        let _ = with_timeout(
                            timeout,
                            endpoint.exchange(
                                hyper::Method::POST,
                                "/api/peers/swarm/deliver",
                                Some(payload),
                            ),
                        )
                        .await;
                    } else {
                        let _ = row;
                    }
                }
                Ok(PeerSwarmDeliverResponse::default())
            }
        }
    }
}

impl SwarmTransport for DaemonSwarmTransport {
    fn local_machine(&self) -> String {
        DaemonPeerFabric::new(self.state.clone()).local_machine_name()
    }

    async fn poll_world(&self) -> PeerSwarmWorld {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let timeout = self.timeout();
            return match self
                .state
                .cloud_home_bridge
                .call(BridgeRpcOp::SwarmWorld, timeout)
                .await
            {
                Ok(BridgeRpcResult::SwarmWorld(world)) => world,
                _ => PeerSwarmWorld {
                    local_machine: CLOUD_MACHINE.to_string(),
                    is_cloud_home: true,
                    remote_sessions: Vec::new(),
                    remote_boards: Vec::new(),
                },
            };
        }
        self.poll_host_world(None).await
    }

    fn notify_hosted_boards_changed(&self) {
        self.state.ui_event_bus.publish(UiEventTopics::SWARM);
    }

    async fn read_remote_board(
        &self,
        host_machine: &str,
        request: PeerSwarmReadRequest,
    ) -> Result<PeerSwarmReadResponse, SwarmError> {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let reply = self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::SwarmRead {
                        host_machine: host_machine.to_string(),
                        request,
                    },
                    self.timeout(),
                )
                .await
                .map_err(SwarmError::Remote)?;
            return match reply {
                BridgeRpcResult::SwarmRead(Ok(r)) => Ok(r),
                BridgeRpcResult::SwarmRead(Err(e)) => Err(e.into_swarm_error()),
                _ => Err(SwarmError::Remote("unexpected bridge response".to_string())),
            };
        }
        self.read_host_side(host_machine, request).await
    }

    async fn post_remote_board(
        &self,
        host_machine: &str,
        request: PeerSwarmPostRequest,
    ) -> Result<PeerSwarmPostResponse, SwarmError> {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let reply = self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::SwarmPost {
                        host_machine: host_machine.to_string(),
                        request,
                    },
                    self.timeout(),
                )
                .await
                .map_err(SwarmError::Remote)?;
            return match reply {
                BridgeRpcResult::SwarmPost(Ok(r)) => Ok(r),
                BridgeRpcResult::SwarmPost(Err(e)) => Err(e.into_swarm_error()),
                _ => Err(SwarmError::Remote("unexpected bridge response".to_string())),
            };
        }
        self.post_host_side(host_machine, request).await
    }

    async fn announce_remote_pr(
        &self,
        host_machine: &str,
        request: PeerSwarmPrRequest,
    ) -> Result<(), SwarmError> {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let reply = self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::SwarmPr {
                        host_machine: host_machine.to_string(),
                        request,
                    },
                    self.timeout(),
                )
                .await
                .map_err(SwarmError::Remote)?;
            return match reply {
                BridgeRpcResult::SwarmPr(Ok(())) => Ok(()),
                BridgeRpcResult::SwarmPr(Err(e)) => Err(e.into_swarm_error()),
                _ => Err(SwarmError::Remote("unexpected bridge response".to_string())),
            };
        }
        self.pr_host_side(host_machine, request).await
    }

    async fn deliver_to_peer(
        &self,
        target_machine: &str,
        request: PeerSwarmDeliverRequest,
    ) -> Result<PeerSwarmDeliverResponse, SwarmError> {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let reply = self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::SwarmDeliver {
                        target_machine: target_machine.to_string(),
                        request,
                    },
                    self.timeout(),
                )
                .await
                .map_err(SwarmError::Remote)?;
            return match reply {
                BridgeRpcResult::SwarmDeliver(Ok(r)) => Ok(r),
                BridgeRpcResult::SwarmDeliver(Err(e)) => Err(e.into_swarm_error()),
                _ => Err(SwarmError::Remote("unexpected bridge response".to_string())),
            };
        }
        self.deliver_host_side(target_machine, request).await
    }
}

async fn probe_cloud_swarm_sessions(
    state: &DaemonState,
    row: &CloudSessionRow,
    host: &CloudHost,
    fronting_machine: &str,
    shadow_started: bool,
    timeout: Duration,
) -> Vec<RemoteWorkingSession> {
    let fallback_group = row
        .clone_repository
        .as_deref()
        .and_then(|repo| {
            crate::forge::ForgeRepo::parse_binding(&format!("github:{repo}"))
                .ok()
                .map(|r| r.canonical_binding())
        })
        .unwrap_or_else(|| format!("{CLOUD_MACHINE}:{}", row.workspace));

    match with_timeout(timeout, host.endpoint(false)).await {
        Ok(Ok(endpoint)) => {
            flush_cloud_session_notices(state, &row.session_id, &endpoint).await;
            if let Ok(Ok((status, bytes))) = with_timeout(
                timeout,
                endpoint.exchange(hyper::Method::GET, "/api/peers/swarm/state", None),
            )
            .await
                && status.is_success()
                && let Ok(parsed) = serde_json::from_slice::<PeerSwarmStateResponse>(&bytes)
            {
                return parsed
                    .sessions
                    .into_iter()
                    .map(|mut s| {
                        s.machine = CLOUD_MACHINE.to_string();
                        s.voter_machine = fronting_machine.to_string();
                        if !crate::swarm::is_forge_group_key(&s.group_key)
                            && crate::swarm::is_forge_group_key(&fallback_group)
                        {
                            s.group_key.clone_from(&fallback_group);
                        }
                        s
                    })
                    .collect();
            }
            Vec::new()
        }
        Ok(Err(CloudSessionError::Suspended)) => {
            if !shadow_started {
                return Vec::new();
            }
            let title = state
                .db()
                .get_session(&row.session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.title)
                .unwrap_or_else(|| row.session_id.clone());
            vec![RemoteWorkingSession {
                machine: CLOUD_MACHINE.to_string(),
                voter_machine: fronting_machine.to_string(),
                session_id: row.session_id.clone(),
                name: title,
                activity: Activity::Idle,
                suspended: true,
                roots: Vec::new(),
                group_key: fallback_group,
            }]
        }
        Ok(Err(_)) | Err(()) => Vec::new(),
    }
}

async fn fetch_peer_swarm_state(
    machine: &str,
    base_url: &str,
    timeout: Duration,
) -> Option<PeerSwarmStateResponse> {
    let url = format!("{base_url}/api/peers/swarm/state");
    let (status, bytes) = http_json(zenwave::Method::GET, &url, None, timeout)
        .await
        .ok()?;
    if !status.is_success() {
        return None;
    }
    let mut parsed: PeerSwarmStateResponse = serde_json::from_slice(&bytes).ok()?;
    parsed.machine = machine.to_string();
    for s in &mut parsed.sessions {
        if s.machine != CLOUD_MACHINE {
            s.machine = machine.to_string();
            s.voter_machine = machine.to_string();
        } else if s.voter_machine.is_empty() {
            s.voter_machine = machine.to_string();
        }
    }
    Some(parsed)
}

fn decode_swarm_response<T: serde::de::DeserializeOwned>(
    status: StatusCode,
    bytes: &[u8],
) -> Result<T, SwarmError> {
    if status.is_success() {
        return serde_json::from_slice(bytes)
            .map_err(|e| SwarmError::Remote(format!("invalid swarm response JSON: {e}")));
    }
    if let Ok(err) = serde_json::from_slice::<PeerSwarmErrorBody>(bytes) {
        return Err(err.into_swarm_error());
    }
    Err(SwarmError::Remote(format!("HTTP {status}")))
}

fn swarm_error_response(err: SwarmError) -> Response {
    let status = match &err {
        SwarmError::NoActiveBoard => StatusCode::NOT_FOUND,
        SwarmError::AmbiguousMention(_) | SwarmError::EmptyMessage(_) => StatusCode::CONFLICT,
        _ => StatusCode::BAD_GATEWAY,
    };
    let body = PeerSwarmErrorBody::from_swarm_error(err);
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut resp = Response::new(skyzen::Body::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    resp
}

/// `GET /api/peers/swarm/state`: returns local working sessions and hosted swarm board summaries.
pub async fn get_peer_swarm_state(
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    if state.draining.load(std::sync::atomic::Ordering::Acquire) {
        let mut draining_resp = Response::new(skyzen::Body::empty());
        *draining_resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        return Ok(draining_resp);
    }
    let machine = DaemonPeerFabric::new(state.clone()).local_machine_name();
    let Some(swarm) = state.services.swarm() else {
        return Ok(peer_json_response(&PeerSwarmStateResponse {
            machine,
            sessions: Vec::new(),
            hosted_boards: Vec::new(),
        }));
    };
    let local_sessions = match swarm.local_working_sessions(&machine).await {
        Ok(mut sessions) => {
            // Also advertise any `Suspended` / active cloud homes fronted by this
            // daemon so tailnet peers include them in repository-scoped swarms.
            if !state.cloud_home_bridge.is_cloud_home() {
                let local_working = state.db().working_session_ids().await.unwrap_or_default();
                let timeout = Duration::from_millis(
                    state
                        .config_snapshot()
                        .remote_access
                        .peer_discovery_timeout_ms
                        .max(100),
                );
                for (row, host) in state.cloud_sessions.hosts_snapshot() {
                    let started = local_working.contains(&row.session_id);
                    let cloud_entries =
                        probe_cloud_swarm_sessions(&state, &row, &host, &machine, started, timeout)
                            .await;
                    sessions.extend(cloud_entries);
                }
            }
            sessions
        }
        Err(err) => return Ok(swarm_error_response(err)),
    };
    let hosted_boards = swarm.hosted_board_summaries();
    Ok(peer_json_response(&PeerSwarmStateResponse {
        machine,
        sessions: local_sessions,
        hosted_boards,
    }))
}

/// `POST /api/peers/swarm/read`: serves a board read on this Board Host.
pub async fn post_peer_swarm_read(
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let req: PeerSwarmReadRequest = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let Some(swarm) = state.services.swarm() else {
        return Ok(swarm_error_response(SwarmError::NoActiveBoard));
    };
    match swarm.serve_host_read(req).await {
        Ok(resp) => Ok(peer_json_response(&resp)),
        Err(err) => Ok(swarm_error_response(err)),
    }
}

/// `POST /api/peers/swarm/post`: appends a board post on this Board Host.
pub async fn post_peer_swarm_post(
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let req: PeerSwarmPostRequest = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let Some(swarm) = state.services.swarm() else {
        return Ok(swarm_error_response(SwarmError::NoActiveBoard));
    };
    match swarm.serve_host_post(req).await {
        Ok(resp) => Ok(peer_json_response(&resp)),
        Err(err) => Ok(swarm_error_response(err)),
    }
}

/// `POST /api/peers/swarm/pr`: records and fans out a PR transition on this Board Host.
pub async fn post_peer_swarm_pr(
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let req: PeerSwarmPrRequest = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let Some(swarm) = state.services.swarm() else {
        return Ok(peer_json_response(&()));
    };
    match swarm.serve_host_pr(req).await {
        Ok(()) => Ok(peer_json_response(&())),
        Err(err) => Ok(swarm_error_response(err)),
    }
}

/// `POST /api/peers/swarm/deliver`: delivers a `@mention` steer, PR notice, or dissolution signal locally.
pub async fn post_peer_swarm_deliver(
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let req: PeerSwarmDeliverRequest = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let Some(swarm) = state.services.swarm() else {
        return Ok(peer_json_response(&PeerSwarmDeliverResponse::default()));
    };
    match swarm.receive_peer_delivery(req).await {
        Ok(resp) => Ok(peer_json_response(&resp)),
        Err(err) => Ok(swarm_error_response(err)),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use aither_agent::{ContextSource as _, Moment};

    use super::*;
    use crate::cloud::home::{CloudHome, FrontingToken};
    use crate::config::MayConfig;
    use crate::db::{MessageMeta, PromptCacheRequest, RunFence, SwarmReadMode};
    use crate::server::{DaemonMode, build_router};
    use crate::session::PendingUserMessage;
    use crate::swarm::{SwarmConditions, SwarmSeenState, SwarmService};

    /// A delivered steer or notice is queued on the receiving daemon before
    /// its HTTP response returns, but the pending row is only claimable once
    /// the enqueue settles — poll the claim instead of racing the first read.
    async fn claim_pending_with_deadline(
        state: &DaemonState,
        fence: &RunFence,
        session_id: &str,
    ) -> Option<PendingUserMessage> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match state
                .db()
                .claim_next_pending_message(fence, session_id)
                .await
            {
                Ok(Some(message)) => return Some(message),
                Ok(None) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) => return None,
                Err(error) => panic!("claim_next_pending_message failed: {error}"),
            }
        }
    }

    struct SwarmTestDaemon {
        dir: tempfile::TempDir,
        state: Arc<DaemonState>,
        swarm: SwarmService,
        addr: SocketAddr,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for SwarmTestDaemon {
        fn drop(&mut self) {
            self.state
                .draining
                .store(true, std::sync::atomic::Ordering::Release);
            self.state.daemon_shutdown.notify_waiters();
            self.task.abort();
        }
    }

    impl SwarmTestDaemon {
        async fn spawn(machine_name: &str) -> Self {
            crate::test_support::install_global_executor();
            let dir = tempfile::tempdir().expect("temp dir");
            let mut cfg = MayConfig::default();
            cfg.experimental.swarm = true;
            cfg.remote_access.machine_name = Some(machine_name.to_string());
            let state = Arc::new(
                DaemonState::for_test(dir.path(), cfg)
                    .await
                    .expect("daemon state"),
            );
            DaemonPeerFabric::install_cloud_bridge(&state);
            let swarm = SwarmService::new(
                crate::chat::agent_messaging(&state),
                state.may_dir.clone(),
                state.config.clone(),
            )
            .with_transport(DaemonSwarmTransport::new(state.clone()));
            state.services.set_swarm(swarm.clone());

            let router = Arc::new(build_router(
                state.clone(),
                DaemonMode::Rest,
                crate::auth::LoopbackOnly,
            ));
            let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let task = tokio::spawn(crate::serve::serve_tcp(listener, router));
            Self {
                dir,
                state,
                swarm,
                addr,
                task,
            }
        }

        async fn start_forge_session(&self, id: &str, title: &str, repo: &str) {
            let checkout = self.dir.path().join(format!("checkout-{id}"));
            std::fs::create_dir_all(&checkout).unwrap();
            self.state
                .db()
                .create_session(id, Some(title), None)
                .await
                .unwrap();
            self.state
                .db()
                .set_session_cwd(id, &checkout.to_string_lossy())
                .await
                .unwrap();
            self.state
                .db()
                .set_session_repo(id, Some(repo))
                .await
                .unwrap();
            self.state
                .db()
                .append_message(
                    id,
                    crate::session::MessageRole::User,
                    "implement feature — 日本語 / 한국어 / 中文",
                    None,
                    MessageMeta::default(),
                )
                .await
                .unwrap();
        }

        async fn start_local_only_session(&self, id: &str, title: &str, path: &std::path::Path) {
            std::fs::create_dir_all(path).unwrap();
            self.state
                .db()
                .create_session(id, Some(title), None)
                .await
                .unwrap();
            self.state
                .db()
                .set_session_cwd(id, &path.to_string_lossy())
                .await
                .unwrap();
            self.state
                .db()
                .append_message(
                    id,
                    crate::session::MessageRole::User,
                    "local work",
                    None,
                    MessageMeta::default(),
                )
                .await
                .unwrap();
        }

        async fn set_cold(&self, session_id: &str) {
            let usage = aither_core::llm::Usage::new(100, 10).with_cache_tokens(0, 100);
            let at = jiff::Timestamp::now()
                - jiff::SignedDuration::try_from(Duration::from_secs(7200)).unwrap();
            self.state
                .db()
                .observe_prompt_cache(PromptCacheRequest {
                    session_id,
                    provider_id: "anthropic",
                    model: "opus",
                    at,
                    usage: &usage,
                    prefix: aither_agent::context::PromptPrefix {
                        chain: aither_agent::context::PrefixChain::of(&[], &[], false),
                        rewrites: aither_agent::context::PrefixRewrites::default(),
                        first_request: true,
                    },
                    lifetime: crate::cache_lifetime::CacheLifetime::default(),
                    published_block: None,
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cross_machine_forge_swarm_elects_single_host_mirrors_sse_zero_rtt_and_fails_over() {
        let mini = SwarmTestDaemon::spawn("mac-mini").await;
        let macbook = SwarmTestDaemon::spawn("macbook").await;

        mini.state.update_config(|cfg| {
            cfg.remote_access
                .peer_endpoints
                .insert("macbook".to_string(), format!("http://{}", macbook.addr));
        });
        macbook.state.update_config(|cfg| {
            cfg.remote_access
                .peer_endpoints
                .insert("mac-mini".to_string(), format!("http://{}", mini.addr));
        });

        // Unrelated local-only directories with identical path stay isolated per machine.
        let shared_tmp = tempfile::tempdir().unwrap();
        mini.start_local_only_session("loc1", "Mini Local", shared_tmp.path())
            .await;
        macbook
            .start_local_only_session("loc2", "Book Local", shared_tmp.path())
            .await;
        mini.swarm.reconcile().await.unwrap();
        macbook.swarm.reconcile().await.unwrap();
        assert!(mini.swarm.active_board_for("loc1").is_none());
        assert!(macbook.swarm.active_board_for("loc2").is_none());

        // Start one `github:May-Agent/may` session on each machine (`total = 2 > 1`).
        mini.start_forge_session("badger", "Backend", "github:May-Agent/may")
            .await;
        mini.set_cold("badger").await;
        macbook
            .start_forge_session("otter", "Frontend", "github:May-Agent/may")
            .await;

        mini.swarm.reconcile().await.unwrap();
        macbook.swarm.reconcile().await.unwrap();

        // `mac-mini` < `macbook`, so `mac-mini` is elected the single-writer Board Host.
        let mini_board = mini.swarm.active_board_for("badger").expect("mini board");
        let book_board = macbook
            .swarm
            .active_board_for("otter")
            .expect("macbook board");
        assert!(mini_board.is_local_host);
        assert_eq!(mini_board.board_epoch, 1);
        assert!(!book_board.is_local_host);
        assert_eq!(book_board.host_machine, "mac-mini");

        // Wait briefly for `/api/events?topics=swarm` SSE mirror to push `max_seq = 1` (lifecycle open).
        for _ in 0..30 {
            if macbook
                .swarm
                .active_board_for("otter")
                .is_some_and(|b| b.max_seq >= 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // `macbook:otter` posts a cross-machine `@mac-mini:badger` message -> routed to `mac-mini`
        // single writer (`seq = 2`), waking cold `badger`, and pushed back over SSE!
        let posted = macbook
            .swarm
            .post_message(
                "otter",
                "Schema updated (@mac-mini:badger please check — 日本語 / 한국어 / 中文)",
            )
            .await
            .expect("cross-machine post");
        assert!(posted.contains("1:2"), "monotonic seq on host: {posted}");
        assert!(posted.contains("mac-mini:badger"), "{posted}");

        let badger_pending = claim_pending_with_deadline(
            &mini.state,
            &RunFence {
                session_id: "badger".to_string(),
                run_id: "r-badger".to_string(),
            },
            "badger",
        )
        .await
        .expect("cold remote peer received @mention steer");
        let mention_xml = badger_pending.hidden_context_xml.unwrap().into_inner();
        assert!(
            mention_xml.contains("id=\"macbook:otter\""),
            "{mention_xml}"
        );

        // Wait for SSE mirror on `macbook`, then verify `SwarmConditions` reports unread with zero RTT!
        mini.swarm
            .post_message("badger", "Ack from mini")
            .await
            .expect("mini reply");
        for _ in 0..30 {
            if macbook
                .swarm
                .active_board_for("otter")
                .is_some_and(|b| b.max_seq >= 3)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut cond = SwarmConditions::new(macbook.swarm.clone(), "otter", SwarmSeenState::new());
        let blocks = cond.blocks(Moment::Request { reassembled: false }).await;
        assert!(
            blocks
                .iter()
                .any(|b| b.block().content().contains("unread message")),
            "SSE mirror drives zero-RTT unread indicator: {blocks:?}"
        );

        // Reading from `macbook` fetches from `mac-mini` and advances local cursor so next request has 0 unread.
        let read_out = macbook
            .swarm
            .read_board("otter", SwarmReadMode::Unread, None)
            .await
            .expect("remote read");
        assert!(read_out.contains("Ack from mini"), "{read_out}");
        let blocks_after = cond.blocks(Moment::Request { reassembled: false }).await;
        assert!(
            blocks_after.is_empty(),
            "cursor synced locally after remote read: {blocks_after:?}"
        );

        // Add a second local worktree session on `macbook`, then disconnect `mac-mini` (Board Host failover!).
        macbook
            .start_forge_session("fox", "Tests", "github:May-Agent/may")
            .await;
        drop(mini);
        macbook.swarm.reconcile().await.unwrap();

        let failed_over = macbook
            .swarm
            .active_board_for("otter")
            .expect("macbook takes over as Board Host");
        assert!(failed_over.is_local_host);
        assert_eq!(
            failed_over.board_epoch, 2,
            "failover opens board under strictly incremented board_epoch"
        );
    }

    #[tokio::test]
    async fn suspended_cloud_notices_buffer_without_booting_and_flush_on_wake() {
        let fronting = SwarmTestDaemon::spawn("macbook").await;
        let notice = PeerSwarmDeliverRequest::Notice {
            group_key: "github:May-Agent/may".to_string(),
            board_epoch: 1,
            seq: 4,
            target_session_id: "lynx".to_string(),
            kind: "PR".to_string(),
            body: "PR #99 merged".to_string(),
        };
        fronting.state.buffered_cloud_notices.push("lynx", notice);
        assert_eq!(
            fronting.state.buffered_cloud_notices.buffered_len("lynx"),
            1
        );

        // Spin up the codespace daemon and flush buffered notices over its `Endpoint`.
        let cloud_dir = tempfile::tempdir().unwrap();
        let token = FrontingToken::generate();
        let home = CloudHome::for_token(&token, "lexoliu".to_string());
        let mut cloud_cfg = MayConfig::default();
        cloud_cfg.experimental.swarm = true;
        let cloud_state = Arc::new(
            DaemonState::for_test(cloud_dir.path(), cloud_cfg)
                .await
                .unwrap(),
        );
        let cloud_swarm = SwarmService::new(
            crate::chat::agent_messaging(&cloud_state),
            cloud_state.may_dir.clone(),
            cloud_state.config.clone(),
        );
        cloud_state.services.set_swarm(cloud_swarm);
        cloud_state
            .db()
            .create_session("lynx", Some("Cloud Lynx"), None)
            .await
            .unwrap();

        let router = Arc::new(build_router(
            cloud_state.clone(),
            DaemonMode::Rest,
            crate::auth::RequireIdentity::new(vec![Box::new(
                crate::auth::FrontingTokenIdentitySource::new(
                    home.digest().unwrap(),
                    "lexoliu".to_string(),
                ),
            )]),
        ));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let _srv = tokio::spawn(crate::serve::serve_tcp(listener, router));

        let endpoint = Endpoint::new(addr, token.expose());
        flush_cloud_session_notices(&fronting.state, "lynx", &endpoint).await;
        assert_eq!(
            fronting.state.buffered_cloud_notices.buffered_len("lynx"),
            0
        );

        let claimed = claim_pending_with_deadline(
            &cloud_state,
            &RunFence {
                session_id: "lynx".to_string(),
                run_id: "r-lynx".to_string(),
            },
            "lynx",
        )
        .await
        .expect("flushed notice queued on codespace");
        let xml = claimed.hidden_context_xml.unwrap().into_inner();
        assert!(xml.contains("PR #99 merged"), "{xml}");
    }
}
