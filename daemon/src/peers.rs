//! Cross-machine peer agent discovery, messaging, and `btw` over Tailscale and cloud homes.
//!
//! Exposes `/api/peers/agents`, `/api/peers/agents/{id}/messages`, and
//! `/api/peers/agents/{id}/btw` gated to the local user, the fronting daemon,
//! or the Tailscale login that **owns** this node (`allowed_logins` guests on
//! other accounts are rejected with `403`). Implements [`PeerFabric`] for
//! [`DaemonState`] across same-owner tailnet peers and Codespaces cloud homes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use skyzen::routing::Params;
use skyzen::utils::State;
use skyzen::{Response, Result as SkyResult, StatusCode};
use zenwave::Client as _;

use super::auth::PeerCaller;
use super::cloud_home::{BridgeDispatcher, BridgeRpcOp, BridgeRpcResult};
use super::cloud_sessions::{CloudHost, CloudSessionError};
use super::error::bad_request_error;
use super::remote_access::{
    DiscoveredTailnetPeer, node_owner_login, read_tailnet_topology, tailscale_binary,
};
use super::state::DaemonState;
use crate::agent_link::{
    Activity, AgentLinkError, AgentStatus, BtwAnswer, CLOUD_MACHINE, PeerAgentsResponse,
    PeerBtwRequest, PeerBtwResponse, PeerErrorBody, PeerErrorKind, PeerFabric, PeerNodeIssue,
    PeerNodeState, PeerSendRequest, PeerSendResponse, RemoteAgentEntry, RemoteListing,
    sanitize_machine_name,
};
use crate::db::CloudSessionRow;

/// How long parsed `tailscale status --json` peer topology is cached before re-reading.
const TOPOLOGY_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
struct CachedTopology {
    fetched_at: Option<Instant>,
    local_machine: Option<String>,
    peers: Vec<DiscoveredTailnetPeer>,
}

/// Cross-machine [`PeerFabric`] backed by a live [`DaemonState`].
#[derive(Clone)]
pub struct DaemonPeerFabric {
    state: Arc<DaemonState>,
    topology_cache: Arc<async_lock::Mutex<CachedTopology>>,
}

impl std::fmt::Debug for DaemonPeerFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonPeerFabric").finish_non_exhaustive()
    }
}

impl DaemonPeerFabric {
    /// Creates a [`DaemonPeerFabric`] bound to `state`.
    #[must_use]
    pub fn new(state: Arc<DaemonState>) -> Self {
        Self {
            state,
            topology_cache: Arc::new(async_lock::Mutex::new(CachedTopology::default())),
        }
    }

    /// Installs the reverse bridge handler on `state.cloud_sessions` so active
    /// cloud homes can list, message, and `btw` sessions on the fronting daemon
    /// and its tailnet peers.
    pub fn install_cloud_bridge(state: &Arc<DaemonState>) {
        let fabric = Self::new(state.clone());
        let state_ref = state.clone();
        state.cloud_sessions.set_peer_bridge(BridgeDispatcher::new(
            move |origin_cloud_session, op| {
                let fabric = fabric.clone();
                let state = state_ref.clone();
                async move {
                    fabric
                        .serve_cloud_bridge(&state, &origin_cloud_session, op)
                        .await
                }
            },
        ));
    }

    fn configured_machine_name(&self) -> String {
        let cfg = self.state.config_snapshot();
        if let Some(explicit) = cfg
            .remote_access
            .machine_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return sanitize_machine_name(explicit);
        }
        if self.state.cloud_home_bridge.is_cloud_home() {
            return CLOUD_MACHINE.to_string();
        }
        if let Some(guard) = self.topology_cache.try_lock()
            && let Some(cached) = &guard.local_machine
        {
            return cached.clone();
        }
        fallback_hostname()
    }

    fn discovery_timeout(&self) -> Duration {
        let ms = self
            .state
            .config_snapshot()
            .remote_access
            .peer_discovery_timeout_ms
            .max(100);
        Duration::from_millis(ms)
    }

    fn btw_timeout(&self) -> Duration {
        let secs = self
            .state
            .config_snapshot()
            .remote_access
            .peer_btw_timeout_secs
            .max(1);
        Duration::from_secs(secs)
    }

    async fn resolve_peers(&self) -> (String, BTreeMap<String, String>) {
        let cfg = self.state.config_snapshot();
        let mut endpoints: BTreeMap<String, String> = BTreeMap::new();

        // Read or refresh `tailscale status --json` topology when remote access is enabled
        // or when no explicit machine name is configured.
        let mut guard = self.topology_cache.lock().await;
        let stale = guard
            .fetched_at
            .is_none_or(|at| at.elapsed() >= TOPOLOGY_CACHE_TTL);
        if stale && (cfg.remote_access.enabled || !cfg.remote_access.tailscale_binary.is_empty()) {
            if let Ok(topology) = read_tailnet_topology(&cfg.remote_access).await {
                if let Some(owner) = &topology.owner_login {
                    self.state
                        .remote_access_manager
                        .tailscale_identity()
                        .set_owner(Some(owner.clone()));
                }
                guard.local_machine = topology.local_machine;
                guard.peers = topology.peers;
            }
            guard.fetched_at = Some(Instant::now());
        }
        let discovered_local = guard.local_machine.clone();
        for peer in &guard.peers {
            endpoints.insert(peer.machine.clone(), peer.base_url.clone());
        }
        drop(guard);

        let local_machine = cfg
            .remote_access
            .machine_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(sanitize_machine_name)
            .or_else(|| {
                self.state
                    .cloud_home_bridge
                    .is_cloud_home()
                    .then(|| CLOUD_MACHINE.to_string())
            })
            .or(discovered_local)
            .unwrap_or_else(fallback_hostname);

        for (raw_name, raw_url) in &cfg.remote_access.peer_endpoints {
            let machine = sanitize_machine_name(raw_name);
            let trimmed_url = raw_url.trim().trim_end_matches('/').to_string();
            if !trimmed_url.is_empty() && machine != local_machine {
                endpoints.insert(machine, trimmed_url);
            }
        }

        (local_machine, endpoints)
    }

    async fn list_host_side(
        &self,
        exclude_cloud_session: Option<&str>,
    ) -> (String, Vec<RemoteAgentEntry>, Vec<PeerNodeIssue>) {
        let (local_machine, endpoints) = self.resolve_peers().await;
        let timeout = self.discovery_timeout();

        let cloud_hosts: Vec<(Arc<CloudSessionRow>, CloudHost)> = self
            .state
            .cloud_sessions
            .hosts_snapshot()
            .into_iter()
            .filter(|(row, _)| exclude_cloud_session != Some(row.session_id.as_str()))
            .collect();

        let cloud_futs = cloud_hosts.into_iter().map(|(row, host)| {
            let state = self.state.clone();
            async move { probe_cloud_session(&state, &row, &host, timeout).await }
        });

        let tailnet_futs = endpoints.into_iter().map(|(machine, base_url)| async move {
            fetch_peer_agents(&machine, &base_url, timeout).await
        });

        let (cloud_results, tailnet_results) = futures_lite::future::zip(
            futures_util::future::join_all(cloud_futs),
            futures_util::future::join_all(tailnet_futs),
        )
        .await;

        let mut agents = Vec::new();
        let mut issues = Vec::new();

        for (entries, maybe_issue) in cloud_results {
            agents.extend(entries);
            if let Some(issue) = maybe_issue {
                issues.push(issue);
            }
        }
        for res in tailnet_results {
            match res {
                Ok(entries) => agents.extend(entries),
                Err(issue) => issues.push(issue),
            }
        }

        agents.sort_by(|a, b| {
            a.machine
                .cmp(&b.machine)
                .then_with(|| a.status.name.cmp(&b.status.name))
                .then_with(|| a.status.session_id.cmp(&b.status.session_id))
        });
        issues.sort_by(|a, b| a.target.cmp(&b.target));
        (local_machine, agents, issues)
    }

    async fn serve_cloud_bridge(
        &self,
        state: &Arc<DaemonState>,
        origin_cloud_session: &str,
        op: BridgeRpcOp,
    ) -> BridgeRpcResult {
        match op {
            BridgeRpcOp::List { .. } => {
                let (host_machine, mut agents, issues) =
                    self.list_host_side(Some(origin_cloud_session)).await;
                if let Ok(local_entries) = super::chat::local_agent_messaging(state)
                    .directory
                    .local_peer_entries(&host_machine)
                    .await
                {
                    agents.extend(local_entries);
                }
                agents.sort_by(|a, b| {
                    a.machine
                        .cmp(&b.machine)
                        .then_with(|| a.status.name.cmp(&b.status.name))
                        .then_with(|| a.status.session_id.cmp(&b.status.session_id))
                });
                BridgeRpcResult::List(RemoteListing {
                    local_machine: CLOUD_MACHINE.to_string(),
                    agents,
                    issues,
                })
            }
            BridgeRpcOp::Send {
                machine,
                target_session_id,
                request,
            } => {
                let (host_machine, _) = self.resolve_peers().await;
                let res = if machine == host_machine {
                    super::chat::local_agent_messaging(state)
                        .deliver_from_peer(&target_session_id, request)
                        .await
                } else {
                    self.send_host_side(&machine, &target_session_id, request)
                        .await
                };
                BridgeRpcResult::Send(res.map_err(PeerErrorBody::from_link_error))
            }
            BridgeRpcOp::Btw {
                machine,
                target_session_id,
                request,
            } => {
                let (host_machine, _) = self.resolve_peers().await;
                let res = if machine == host_machine {
                    super::chat::local_agent_messaging(state)
                        .answer_from_peer(&target_session_id, request)
                        .await
                } else {
                    self.btw_host_side(&machine, &target_session_id, request)
                        .await
                };
                BridgeRpcResult::Btw(
                    res.map(|ans| PeerBtwResponse {
                        recipient: ans.recipient,
                        session_id: target_session_id,
                        answer: ans.answer,
                    })
                    .map_err(PeerErrorBody::from_link_error),
                )
            }
        }
    }

    async fn send_host_side(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerSendRequest,
    ) -> Result<String, AgentLinkError> {
        let timeout = self.discovery_timeout();
        if machine == CLOUD_MACHINE {
            let Some(host) = self.state.cloud_sessions.host(target_session_id) else {
                return Err(AgentLinkError::NoSuchAgent(format!(
                    "{machine}:{target_session_id}"
                )));
            };
            let title = cloud_session_title(&self.state, target_session_id).await;
            let endpoint = match with_timeout(timeout, host.endpoint(false)).await {
                Ok(Ok(ep)) => ep,
                Ok(Err(CloudSessionError::Suspended)) => {
                    return Err(AgentLinkError::ColdRecipient(title));
                }
                Ok(Err(err)) => {
                    return Err(AgentLinkError::PeerUnavailable {
                        machine: machine.to_string(),
                        reason: err.to_string(),
                    });
                }
                Err(()) => {
                    return Err(AgentLinkError::PeerUnavailable {
                        machine: machine.to_string(),
                        reason: "timed out reaching cloud home".to_string(),
                    });
                }
            };
            let path = format!("/api/peers/agents/{target_session_id}/messages");
            let payload =
                serde_json::to_vec(&request).map_err(|e| AgentLinkError::Fork(e.to_string()))?;
            let (status, bytes) = with_timeout(
                timeout,
                endpoint.exchange(hyper::Method::POST, &path, Some(payload)),
            )
            .await
            .map_err(|()| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: "timed out delivering message to cloud home".to_string(),
            })?
            .map_err(|err| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: err.to_string(),
            })?;
            return decode_send_response(machine, status, &bytes);
        }

        let (_, endpoints) = self.resolve_peers().await;
        let Some(base_url) = endpoints.get(machine) else {
            return Err(AgentLinkError::NoSuchAgent(format!(
                "{machine}:{target_session_id}"
            )));
        };
        let url = format!("{base_url}/api/peers/agents/{target_session_id}/messages");
        let payload =
            serde_json::to_vec(&request).map_err(|e| AgentLinkError::Fork(e.to_string()))?;
        let (status, bytes) = http_json(zenwave::Method::POST, &url, Some(payload), timeout)
            .await
            .map_err(|reason| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason,
            })?;
        decode_send_response(machine, status, &bytes)
    }

    async fn btw_host_side(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerBtwRequest,
    ) -> Result<BtwAnswer, AgentLinkError> {
        let probe_timeout = self.discovery_timeout();
        let btw_timeout = self.btw_timeout();
        if machine == CLOUD_MACHINE {
            let Some(host) = self.state.cloud_sessions.host(target_session_id) else {
                return Err(AgentLinkError::NoSuchAgent(format!(
                    "{machine}:{target_session_id}"
                )));
            };
            let title = cloud_session_title(&self.state, target_session_id).await;
            let endpoint = match with_timeout(probe_timeout, host.endpoint(false)).await {
                Ok(Ok(ep)) => ep,
                Ok(Err(CloudSessionError::Suspended)) => {
                    return Err(AgentLinkError::ColdBtwRecipient(title));
                }
                Ok(Err(err)) => {
                    return Err(AgentLinkError::PeerUnavailable {
                        machine: machine.to_string(),
                        reason: err.to_string(),
                    });
                }
                Err(()) => {
                    return Err(AgentLinkError::PeerUnavailable {
                        machine: machine.to_string(),
                        reason: "timed out reaching cloud home".to_string(),
                    });
                }
            };
            let path = format!("/api/peers/agents/{target_session_id}/btw");
            let payload =
                serde_json::to_vec(&request).map_err(|e| AgentLinkError::Fork(e.to_string()))?;
            let (status, bytes) = with_timeout(
                btw_timeout,
                endpoint.exchange(hyper::Method::POST, &path, Some(payload)),
            )
            .await
            .map_err(|()| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: "timed out waiting for cloud home btw answer".to_string(),
            })?
            .map_err(|err| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: err.to_string(),
            })?;
            return decode_btw_response(machine, status, &bytes);
        }

        let (_, endpoints) = self.resolve_peers().await;
        let Some(base_url) = endpoints.get(machine) else {
            return Err(AgentLinkError::NoSuchAgent(format!(
                "{machine}:{target_session_id}"
            )));
        };
        let url = format!("{base_url}/api/peers/agents/{target_session_id}/btw");
        let payload =
            serde_json::to_vec(&request).map_err(|e| AgentLinkError::Fork(e.to_string()))?;
        let (status, bytes) = http_json(zenwave::Method::POST, &url, Some(payload), btw_timeout)
            .await
            .map_err(|reason| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason,
            })?;
        decode_btw_response(machine, status, &bytes)
    }
}

impl PeerFabric for DaemonPeerFabric {
    fn local_machine(&self) -> String {
        self.configured_machine_name()
    }

    fn is_cloud_shadow(&self, session_id: &str) -> bool {
        self.state.cloud_sessions.is_cloud_session(session_id)
    }

    async fn list_remote(&self, caller_session_id: &str) -> RemoteListing {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let timeout = self.discovery_timeout();
            return match self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::List {
                        caller_session_id: caller_session_id.to_string(),
                    },
                    timeout,
                )
                .await
            {
                Ok(BridgeRpcResult::List(listing)) => listing,
                Ok(_) => RemoteListing {
                    local_machine: CLOUD_MACHINE.to_string(),
                    agents: Vec::new(),
                    issues: Vec::new(),
                },
                Err(reason) => RemoteListing {
                    local_machine: CLOUD_MACHINE.to_string(),
                    agents: Vec::new(),
                    issues: vec![PeerNodeIssue {
                        target: "fronting-host".to_string(),
                        state: PeerNodeState::Offline,
                        detail: Some(reason),
                    }],
                },
            };
        }

        let (local_machine, agents, issues) = self.list_host_side(None).await;
        RemoteListing {
            local_machine,
            agents,
            issues,
        }
    }

    async fn send_remote(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerSendRequest,
    ) -> Result<String, AgentLinkError> {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let timeout = self.discovery_timeout();
            let reply = self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::Send {
                        machine: machine.to_string(),
                        target_session_id: target_session_id.to_string(),
                        request,
                    },
                    timeout,
                )
                .await
                .map_err(|reason| AgentLinkError::PeerUnavailable {
                    machine: machine.to_string(),
                    reason,
                })?;
            return match reply {
                BridgeRpcResult::Send(Ok(name)) => Ok(name),
                BridgeRpcResult::Send(Err(err)) => Err(err.into_link_error(machine)),
                _ => Err(AgentLinkError::PeerUnavailable {
                    machine: machine.to_string(),
                    reason: "unexpected bridge response".to_string(),
                }),
            };
        }

        self.send_host_side(machine, target_session_id, request)
            .await
    }

    async fn btw_remote(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerBtwRequest,
    ) -> Result<BtwAnswer, AgentLinkError> {
        if self.state.cloud_home_bridge.is_cloud_home() {
            let timeout = self.btw_timeout();
            let reply = self
                .state
                .cloud_home_bridge
                .call(
                    BridgeRpcOp::Btw {
                        machine: machine.to_string(),
                        target_session_id: target_session_id.to_string(),
                        request,
                    },
                    timeout,
                )
                .await
                .map_err(|reason| AgentLinkError::PeerUnavailable {
                    machine: machine.to_string(),
                    reason,
                })?;
            return match reply {
                BridgeRpcResult::Btw(Ok(ans)) => Ok(BtwAnswer {
                    recipient: ans.recipient,
                    recipient_id: format!("{machine}:{}", ans.session_id),
                    answer: ans.answer,
                }),
                BridgeRpcResult::Btw(Err(err)) => Err(err.into_link_error(machine)),
                _ => Err(AgentLinkError::PeerUnavailable {
                    machine: machine.to_string(),
                    reason: "unexpected bridge response".to_string(),
                }),
            };
        }

        self.btw_host_side(machine, target_session_id, request)
            .await
    }
}

async fn cloud_session_title(state: &DaemonState, session_id: &str) -> String {
    state
        .runtime_service
        .db()
        .get_session(session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.title)
        .unwrap_or_else(|| session_id.to_string())
}

async fn probe_cloud_session(
    state: &DaemonState,
    row: &CloudSessionRow,
    host: &CloudHost,
    timeout: Duration,
) -> (Vec<RemoteAgentEntry>, Option<PeerNodeIssue>) {
    let title = cloud_session_title(state, &row.session_id).await;
    let repo_binding = row
        .clone_repository
        .as_deref()
        .map(|repo| format!("github:{repo}"));
    let suspended_placeholder = || RemoteAgentEntry {
        machine: CLOUD_MACHINE.to_string(),
        status: AgentStatus {
            session_id: row.session_id.clone(),
            name: title.clone(),
            cwd: PathBuf::from(&row.workspace),
            activity: Activity::Idle,
            roots: Vec::new(),
        },
        suspended: true,
        repo: repo_binding.clone(),
    };

    let endpoint = match with_timeout(timeout, host.endpoint(false)).await {
        Ok(Ok(ep)) => ep,
        Ok(Err(CloudSessionError::Suspended)) => {
            return (vec![suspended_placeholder()], None);
        }
        Ok(Err(err)) => {
            return (
                Vec::new(),
                Some(PeerNodeIssue {
                    target: format!("{CLOUD_MACHINE}:{}", row.session_id),
                    state: PeerNodeState::Offline,
                    detail: Some(err.to_string()),
                }),
            );
        }
        Err(()) => {
            return (
                Vec::new(),
                Some(PeerNodeIssue {
                    target: format!("{CLOUD_MACHINE}:{}", row.session_id),
                    state: PeerNodeState::Offline,
                    detail: Some("timed out probing cloud home".to_string()),
                }),
            );
        }
    };

    match with_timeout(
        timeout,
        endpoint.exchange(hyper::Method::GET, "/api/peers/agents", None),
    )
    .await
    {
        Ok(Ok((status, bytes))) if status.is_success() => {
            if let Ok(parsed) = serde_json::from_slice::<PeerAgentsResponse>(&bytes) {
                let entries = parsed
                    .agents
                    .into_iter()
                    .map(|mut entry| {
                        entry.machine = CLOUD_MACHINE.to_string();
                        if entry.repo.is_none() {
                            entry.repo.clone_from(&repo_binding);
                        }
                        entry
                    })
                    .collect();
                (entries, None)
            } else {
                (
                    Vec::new(),
                    Some(PeerNodeIssue {
                        target: format!("{CLOUD_MACHINE}:{}", row.session_id),
                        state: PeerNodeState::Offline,
                        detail: Some("invalid /api/peers/agents response".to_string()),
                    }),
                )
            }
        }
        Ok(Ok((status, _))) => (
            Vec::new(),
            Some(PeerNodeIssue {
                target: format!("{CLOUD_MACHINE}:{}", row.session_id),
                state: PeerNodeState::Offline,
                detail: Some(format!("HTTP {status}")),
            }),
        ),
        Ok(Err(err)) => (
            Vec::new(),
            Some(PeerNodeIssue {
                target: format!("{CLOUD_MACHINE}:{}", row.session_id),
                state: PeerNodeState::Offline,
                detail: Some(err.to_string()),
            }),
        ),
        Err(()) => (
            Vec::new(),
            Some(PeerNodeIssue {
                target: format!("{CLOUD_MACHINE}:{}", row.session_id),
                state: PeerNodeState::Offline,
                detail: Some("timed out reading cloud home agents".to_string()),
            }),
        ),
    }
}

async fn fetch_peer_agents(
    machine: &str,
    base_url: &str,
    timeout: Duration,
) -> Result<Vec<RemoteAgentEntry>, PeerNodeIssue> {
    let url = format!("{base_url}/api/peers/agents");
    let (status, bytes) = http_json(zenwave::Method::GET, &url, None, timeout)
        .await
        .map_err(|reason| PeerNodeIssue {
            target: machine.to_string(),
            state: PeerNodeState::Offline,
            detail: Some(reason),
        })?;
    if !status.is_success() {
        let detail = serde_json::from_slice::<PeerErrorBody>(&bytes)
            .map_or_else(|_| format!("HTTP {status}"), |err| err.subject);
        return Err(PeerNodeIssue {
            target: machine.to_string(),
            state: PeerNodeState::Offline,
            detail: Some(detail),
        });
    }
    let parsed: PeerAgentsResponse =
        serde_json::from_slice(&bytes).map_err(|err| PeerNodeIssue {
            target: machine.to_string(),
            state: PeerNodeState::Offline,
            detail: Some(format!("unreadable peer listing: {err}")),
        })?;
    Ok(parsed
        .agents
        .into_iter()
        .map(|mut entry| {
            entry.machine = machine.to_string();
            entry
        })
        .collect())
}

fn decode_send_response(
    machine: &str,
    status: StatusCode,
    bytes: &[u8],
) -> Result<String, AgentLinkError> {
    if status.is_success() {
        let parsed: PeerSendResponse =
            serde_json::from_slice(bytes).map_err(|err| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: format!("invalid send response: {err}"),
            })?;
        return Ok(parsed.recipient_name);
    }
    if let Ok(err) = serde_json::from_slice::<PeerErrorBody>(bytes) {
        return Err(err.into_link_error(machine));
    }
    Err(AgentLinkError::PeerUnavailable {
        machine: machine.to_string(),
        reason: format!("HTTP {status}"),
    })
}

fn decode_btw_response(
    machine: &str,
    status: StatusCode,
    bytes: &[u8],
) -> Result<BtwAnswer, AgentLinkError> {
    if status.is_success() {
        let parsed: PeerBtwResponse =
            serde_json::from_slice(bytes).map_err(|err| AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: format!("invalid btw response: {err}"),
            })?;
        return Ok(BtwAnswer {
            recipient: parsed.recipient,
            recipient_id: format!("{machine}:{}", parsed.session_id),
            answer: parsed.answer,
        });
    }
    if let Ok(err) = serde_json::from_slice::<PeerErrorBody>(bytes) {
        return Err(err.into_link_error(machine));
    }
    Err(AgentLinkError::PeerUnavailable {
        machine: machine.to_string(),
        reason: format!("HTTP {status}"),
    })
}

async fn http_json(
    method: zenwave::Method,
    url: &str,
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<(StatusCode, bytes::Bytes), String> {
    with_timeout(timeout, async {
        let mut client = zenwave::client();
        let mut builder = client.method(method, url).map_err(|e| e.to_string())?;
        if let Some(payload) = body {
            builder = builder
                .header(zenwave::header::CONTENT_TYPE.as_str(), "application/json")
                .map_err(|e| e.to_string())?
                .bytes_body(payload);
        }
        let response: zenwave::Response = match builder.await {
            Ok(resp) => resp,
            Err(zenwave::Error::Http { response, .. }) => response.response,
            Err(err) => return Err(err.to_string()),
        };
        let status = StatusCode::from_u16(response.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok((status, bytes))
    })
    .await
    .map_err(|()| format!("request to {url} timed out"))?
}

async fn with_timeout<T>(
    duration: Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, ()> {
    futures_lite::future::or(async { Ok(fut.await) }, async {
        async_io::Timer::after(duration).await;
        Err(())
    })
    .await
}

fn fallback_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| "local".to_string(), |s| sanitize_machine_name(&s))
}

/// Verifies that a remote caller on `/api/peers/*` is the node's Tailscale owner
/// (or local / fronting daemon), rejecting `allowed_logins` guests with `403`.
async fn check_same_owner(state: &DaemonState, caller: &PeerCaller) -> Option<Response> {
    let PeerCaller::Tailnet { login } = caller else {
        return None;
    };
    let identity = state.remote_access_manager.tailscale_identity();
    let owner = if let Some(cached) = identity.owner() {
        Some(cached)
    } else {
        let cfg = state.config_snapshot();
        let resolved = node_owner_login(&tailscale_binary(&cfg.remote_access))
            .await
            .ok();
        if let Some(owner) = &resolved {
            identity.set_owner(Some(owner.clone()));
        }
        resolved
    };
    if owner
        .as_deref()
        .is_some_and(|owner_login| owner_login.eq_ignore_ascii_case(login))
    {
        return None;
    }
    Some(peer_error_response(
        StatusCode::FORBIDDEN,
        &PeerErrorBody {
            kind: PeerErrorKind::Forbidden,
            subject: format!(
                "tailnet caller '{login}' is not the owner of this node; cross-machine agent operations require same-owner identity"
            ),
        },
    ))
}

fn peer_error_response(status: StatusCode, body: &PeerErrorBody) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let mut response = Response::new(skyzen::Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn peer_json_response<T: serde::Serialize>(payload: &T) -> Response {
    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    let mut response = Response::new(skyzen::Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    response
}

const fn status_for_peer_error(kind: PeerErrorKind) -> StatusCode {
    match kind {
        PeerErrorKind::NoSuchAgent => StatusCode::NOT_FOUND,
        PeerErrorKind::ColdRecipient
        | PeerErrorKind::ColdBtwRecipient
        | PeerErrorKind::NoForkContext
        | PeerErrorKind::NearCompaction => StatusCode::CONFLICT,
        PeerErrorKind::Forbidden => StatusCode::FORBIDDEN,
        PeerErrorKind::Fork | PeerErrorKind::Unavailable => StatusCode::BAD_GATEWAY,
    }
}

/// `GET /api/peers/agents`: lists non-archived sessions hosted locally on this daemon.
pub async fn get_peer_agents(
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let machine = DaemonPeerFabric::new(state.clone()).local_machine();
    let messaging = super::chat::local_agent_messaging(&state);
    match messaging.directory.local_peer_entries(&machine).await {
        Ok(agents) => Ok(peer_json_response(&PeerAgentsResponse { machine, agents })),
        Err(error) => {
            let body = PeerErrorBody::from_link_error(error);
            Ok(peer_error_response(status_for_peer_error(body.kind), &body))
        }
    }
}

/// `POST /api/peers/agents/{id}/messages`: delivers a cross-machine `agent send`
/// onto local session `{id}`, refusing cold idle caches.
pub async fn post_peer_message(
    params: Params,
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let target_id = params.get("id").map_err(bad_request_error)?.to_string();
    let request: PeerSendRequest = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let messaging = super::chat::local_agent_messaging(&state);
    match messaging.deliver_from_peer(&target_id, request).await {
        Ok(recipient_name) => Ok(peer_json_response(&PeerSendResponse { recipient_name })),
        Err(error) => {
            let body = PeerErrorBody::from_link_error(error);
            Ok(peer_error_response(status_for_peer_error(body.kind), &body))
        }
    }
}

/// `POST /api/peers/agents/{id}/btw`: runs a confined read-only `btw` fork on
/// local session `{id}`'s live context and sandbox.
pub async fn post_peer_btw(
    params: Params,
    caller: PeerCaller,
    State(state): State<Arc<DaemonState>>,
    body: bytes::Bytes,
) -> SkyResult<Response> {
    if let Some(denied) = check_same_owner(&state, &caller).await {
        return Ok(denied);
    }
    let target_id = params.get("id").map_err(bad_request_error)?.to_string();
    let request: PeerBtwRequest = serde_json::from_slice(&body).map_err(bad_request_error)?;
    let messaging = super::chat::local_agent_messaging(&state);
    match messaging.answer_from_peer(&target_id, request).await {
        Ok(ans) => Ok(peer_json_response(&PeerBtwResponse {
            recipient: ans.recipient,
            session_id: ans.recipient_id,
            answer: ans.answer,
        })),
        Err(error) => {
            let body = PeerErrorBody::from_link_error(error);
            Ok(peer_error_response(status_for_peer_error(body.kind), &body))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use super::*;
    use crate::auth::{FrontingTokenIdentitySource, RequireIdentity};
    use crate::cloud::home::{CloudHome, FrontingToken};
    use crate::cloud_home::follow_peer_bridge;
    use crate::cloud_sessions::Endpoint;
    use crate::config::MayConfig;
    use crate::db::{PromptCacheRequest, RunFence};
    use crate::server::{DaemonMode, build_router};

    struct TestNode {
        _dir: tempfile::TempDir,
        state: Arc<DaemonState>,
        addr: SocketAddr,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestNode {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl TestNode {
        async fn spawn_tailnet(machine_name: &str, owner: &str, allowed: &[&str]) -> Self {
            crate::test_support::install_global_executor();
            let dir = tempfile::tempdir().expect("temp dir");
            let mut cfg = MayConfig::default();
            cfg.remote_access.machine_name = Some(machine_name.to_string());
            let state = Arc::new(
                DaemonState::for_test(dir.path(), cfg)
                    .await
                    .expect("daemon state"),
            );
            let identity = state.remote_access_manager.tailscale_identity();
            identity.set_owner(Some(owner.to_string()));
            identity.set_allowed(allowed.iter().map(|s| (*s).to_string()).collect());
            DaemonPeerFabric::install_cloud_bridge(&state);

            let router = Arc::new(build_router(
                state.clone(),
                DaemonMode::Rest,
                RequireIdentity::new(vec![Box::new(identity)]),
            ));
            let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let task = tokio::spawn(crate::serve::serve_tcp(listener, router));
            Self {
                _dir: dir,
                state,
                addr,
                task,
            }
        }

        async fn spawn_loopback_peer(machine_name: &str) -> Self {
            crate::test_support::install_global_executor();
            let dir = tempfile::tempdir().expect("temp dir");
            let mut cfg = MayConfig::default();
            cfg.remote_access.machine_name = Some(machine_name.to_string());
            let state = Arc::new(
                DaemonState::for_test(dir.path(), cfg)
                    .await
                    .expect("daemon state"),
            );
            DaemonPeerFabric::install_cloud_bridge(&state);
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
                _dir: dir,
                state,
                addr,
                task,
            }
        }

        async fn record_cache_age(&self, session_id: &str, ago: Duration) {
            let usage = aither_core::llm::Usage::new(100, 10).with_cache_tokens(0, 100);
            let at = jiff::Timestamp::now() - jiff::SignedDuration::try_from(ago).expect("fits");
            self.state
                .runtime_service
                .db()
                .observe_prompt_cache(PromptCacheRequest {
                    session_id,
                    provider_id: "anthropic",
                    model: "opus",
                    at,
                    usage: &usage,
                    prefix: aither_agent::context::PromptPrefix {
                        chain: aither_agent::context::PrefixChain::of(&[], &[]),
                        rewrites: aither_agent::context::PrefixRewrites::default(),
                        first_request: true,
                    },
                    lifetime: crate::cache_lifetime::CacheLifetime::default(),
                    published_block: None,
                })
                .await
                .expect("prompt cache recorded");
        }
    }

    #[tokio::test]
    async fn non_owner_allowed_logins_are_rejected_on_peer_routes() {
        let node = TestNode::spawn_tailnet(
            "mac-mini",
            "owner@example.com",
            &["owner@example.com", "guest@example.com"],
        )
        .await;
        node.state
            .runtime_service
            .db()
            .create_session("otter", Some("Otter"), None)
            .await
            .expect("session");

        let url = format!("http://{}/api/peers/agents", node.addr);
        // Guest login is accepted by RequireIdentity for web UI, yet MUST get 403 on `/api/peers/agents`.
        let mut client = zenwave::client();
        let guest_resp = match client
            .method(zenwave::Method::GET, &url)
            .unwrap()
            .header("Tailscale-User-Login", "guest@example.com")
            .unwrap()
            .await
        {
            Ok(resp) => resp,
            Err(zenwave::Error::Http { response, .. }) => response.response,
            Err(err) => panic!("http failed: {err}"),
        };
        assert_eq!(guest_resp.status().as_u16(), 403);

        // Owner login succeeds with 200.
        let owner_resp = client
            .method(zenwave::Method::GET, &url)
            .unwrap()
            .header("Tailscale-User-Login", "owner@example.com")
            .unwrap()
            .await
            .expect("owner 200");
        assert_eq!(owner_resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn bidirectional_peer_daemons_discover_send_btw_and_enforce_cold_boundary() {
        let macbook = TestNode::spawn_loopback_peer("macbook").await;
        let mini = TestNode::spawn_loopback_peer("mac-mini").await;

        macbook.state.update_config(|cfg| {
            cfg.remote_access
                .peer_endpoints
                .insert("mac-mini".to_string(), format!("http://{}", mini.addr));
        });
        mini.state.update_config(|cfg| {
            cfg.remote_access
                .peer_endpoints
                .insert("macbook".to_string(), format!("http://{}", macbook.addr));
        });

        macbook
            .state
            .runtime_service
            .db()
            .create_session("otter", Some("Frontend"), None)
            .await
            .unwrap();
        mini.state
            .runtime_service
            .db()
            .create_session("badger", Some("Backend"), None)
            .await
            .unwrap();
        mini.state
            .runtime_service
            .db()
            .create_session("sleepy", Some("Sleepy"), None)
            .await
            .unwrap();
        mini.record_cache_age("badger", Duration::from_secs(60))
            .await;
        mini.record_cache_age("sleepy", Duration::from_secs(7200))
            .await;

        let mb_msg = crate::chat::agent_messaging(&macbook.state);
        // Cold remote session is refused without enqueuing anything.
        let cold_err = mb_msg
            .send("otter", "mac-mini:sleepy", "ping")
            .await
            .expect_err("cold remote refused");
        assert!(matches!(cold_err, AgentLinkError::ColdRecipient(_)));

        // Remote `btw` preserves semantic ColdBtwRecipient and NoForkContext across HTTP.
        let fabric = DaemonPeerFabric::new(macbook.state.clone());
        let cold_btw = fabric
            .btw_remote(
                "mac-mini",
                "sleepy",
                PeerBtwRequest {
                    sender_name: "Frontend".to_string(),
                    sender_address: "macbook:otter".to_string(),
                    question: "what changed?".to_string(),
                },
            )
            .await
            .expect_err("cold btw refused");
        assert!(matches!(cold_btw, AgentLinkError::ColdBtwRecipient(_)));

        let no_ctx_btw = fabric
            .btw_remote(
                "mac-mini",
                "badger",
                PeerBtwRequest {
                    sender_name: "Frontend".to_string(),
                    sender_address: "macbook:otter".to_string(),
                    question: "what changed?".to_string(),
                },
            )
            .await
            .expect_err("unstarted fork refused");
        assert!(matches!(no_ctx_btw, AgentLinkError::NoForkContext(_)));

        // Warm remote session receives message framed with `macbook:otter` reply address.
        let recipient = mb_msg
            .send("otter", "Backend", "API ready — テスト / 테스트 / 测试")
            .await
            .expect("warm remote send");
        assert_eq!(recipient, "Backend");

        let claimed = mini
            .state
            .runtime_service
            .db()
            .claim_next_pending_message(
                &RunFence {
                    session_id: "badger".to_string(),
                    run_id: "r1".to_string(),
                },
                "badger",
            )
            .await
            .unwrap()
            .expect("remote message queued");
        let xml = claimed.hidden_context_xml.unwrap().into_inner();
        assert!(
            xml.contains("agent send macbook:otter"),
            "reply address carries machine prefix: {xml}"
        );
        assert!(xml.contains("テスト / 테스트 / 测试"), "{xml}");
    }

    #[tokio::test]
    async fn cloud_home_bridge_routes_discovery_and_messages_back_to_fronting_daemon() {
        crate::test_support::install_global_executor();
        let host_node = TestNode::spawn_loopback_peer("macbook").await;
        host_node
            .state
            .runtime_service
            .db()
            .create_session("otter", Some("Host Lead"), None)
            .await
            .unwrap();
        host_node
            .record_cache_age("otter", Duration::from_secs(30))
            .await;

        let cloud_dir = tempfile::tempdir().expect("cloud dir");
        let token = FrontingToken::generate();
        let home_record = CloudHome::for_token(&token, "lexoliu".to_string());
        let mut cloud_state_raw = DaemonState::for_test(cloud_dir.path(), MayConfig::default())
            .await
            .expect("cloud state");
        cloud_state_raw.cloud_home_bridge = crate::cloud_home::CloudHomeBridge::new(true);
        let cloud_state = Arc::new(cloud_state_raw);
        cloud_state
            .runtime_service
            .db()
            .create_session("lynx", Some("Cloud Runner"), None)
            .await
            .unwrap();

        let cloud_router = Arc::new(build_router(
            cloud_state.clone(),
            DaemonMode::Rest,
            RequireIdentity::new(vec![Box::new(FrontingTokenIdentitySource::new(
                home_record.digest().expect("digest"),
                "lexoliu".to_string(),
            ))]),
        ));
        let cloud_listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let cloud_addr = cloud_listener.local_addr().unwrap();
        let _cloud_server = tokio::spawn(crate::serve::serve_tcp(cloud_listener, cloud_router));

        let endpoint = Endpoint::new(cloud_addr, token.expose());
        let dispatcher = Arc::new(arc_swap::ArcSwapOption::from(Some(Arc::new(
            BridgeDispatcher::new({
                let fabric = DaemonPeerFabric::new(host_node.state.clone());
                let host_state = host_node.state.clone();
                move |origin, op| {
                    let fabric = fabric.clone();
                    let host_state = host_state.clone();
                    async move { fabric.serve_cloud_bridge(&host_state, &origin, op).await }
                }
            }),
        ))));
        let _pump =
            executor_core::spawn(follow_peer_bridge(endpoint, "lynx".to_string(), dispatcher));

        let cloud_msg = crate::chat::agent_messaging(&cloud_state);
        let listing = cloud_msg
            .directory
            .peers()
            .expect("peers")
            .list_remote("lynx")
            .await;
        assert!(
            listing
                .agents
                .iter()
                .any(|e| e.machine == "macbook" && e.status.session_id == "otter"),
            "cloud home discovers fronting daemon's session: {listing:?}"
        );

        let delivered_to = cloud_msg
            .send("lynx", "macbook:otter", "Built inside codespace!")
            .await
            .expect("cloud-to-host send");
        assert_eq!(delivered_to, "Host Lead");

        let claimed = host_node
            .state
            .runtime_service
            .db()
            .claim_next_pending_message(
                &RunFence {
                    session_id: "otter".to_string(),
                    run_id: "r-host".to_string(),
                },
                "otter",
            )
            .await
            .unwrap()
            .expect("host message queued");
        let xml = claimed.hidden_context_xml.unwrap().into_inner();
        assert!(xml.contains("agent send cloud:lynx"), "{xml}");
        assert!(xml.contains("Built inside codespace!"), "{xml}");
    }
}
