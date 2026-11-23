//! Cross-machine agent discovery, messaging, and `btw` transport abstractions.
//!
//! [`PeerFabric`] connects [`super::AgentDirectory`] and [`super::AgentMessaging`]
//! to same-owner Tailscale peers and Codespaces cloud homes without pulling
//! daemon HTTP listener types into `may-agent-core`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{AgentLinkError, AgentStatus, BtwAnswer};

/// Reserved machine prefix addressing sessions hosted on Codespaces cloud homes.
pub const CLOUD_MACHINE: &str = "cloud";

/// Degraded reachability state of a remote machine or cloud home during `agent list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerNodeState {
    /// Peer daemon did not respond within the discovery deadline or refused connection.
    Offline,
    /// Codespace VM is suspended; ordinary `agent list` / `agent send` / `btw` never wake it.
    Suspended,
}

impl std::fmt::Display for PeerNodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Offline => "offline",
            Self::Suspended => "suspended",
        })
    }
}

/// Non-fatal status note reported for an offline peer node or suspended cloud home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerNodeIssue {
    /// Machine or `<machine>:<session-id>` label.
    pub target: String,
    /// Degraded state (`offline` or `suspended`).
    pub state: PeerNodeState,
    /// Optional human-readable context (e.g. repository or failure summary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Live or suspended session discovered on another machine or cloud home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAgentEntry {
    /// Machine prefix (`mac-mini`, `cloud`, etc.).
    pub machine: String,
    /// Agent status snapshot on that machine (`session_id` is the bare id on its host).
    pub status: AgentStatus,
    /// True when this entry represents a `Suspended` cloud-home session.
    #[serde(default)]
    pub suspended: bool,
    /// Optional canonical repository binding (`github:owner/repo`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl RemoteAgentEntry {
    /// Canonical `<machine>:<session-id>` address used across machines.
    #[must_use]
    pub fn qualified_id(&self) -> String {
        format!("{}:{}", self.machine, self.status.session_id)
    }
}

/// Combined snapshot returned by a [`PeerFabric::list_remote`] fan-out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteListing {
    /// Short name of the local daemon (`macbook-pro`, `cloud`, etc.).
    pub local_machine: String,
    /// Reachable remote sessions (and suspended cloud-home placeholders).
    pub agents: Vec<RemoteAgentEntry>,
    /// Peer nodes that were unreachable or timed out during listing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<PeerNodeIssue>,
}

/// Wire payload served by `GET /api/peers/agents` on one daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAgentsResponse {
    /// Short machine name of the answering daemon.
    pub machine: String,
    /// Non-archived local sessions hosted directly on that daemon.
    pub agents: Vec<RemoteAgentEntry>,
}

/// Wire payload posted to `POST /api/peers/agents/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSendRequest {
    /// Display name of the sending session.
    pub sender_name: String,
    /// Reply-to `<machine>:<session-id>` address of the sender.
    pub sender_address: String,
    /// Plain-text body already read from the sender's sandbox and checked by `LanguageGuard`.
    pub body: String,
}

/// Wire response from `POST /api/peers/agents/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSendResponse {
    /// Recipient display name on the remote daemon.
    pub recipient_name: String,
}

/// Wire payload posted to `POST /api/peers/agents/{id}/btw`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerBtwRequest {
    /// Display name of the asking session.
    pub sender_name: String,
    /// Reply-to `<machine>:<session-id>` address of the asking session.
    pub sender_address: String,
    /// Side question text.
    pub question: String,
}

/// Wire response from `POST /api/peers/agents/{id}/btw`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerBtwResponse {
    /// Recipient display name.
    pub recipient: String,
    /// Bare session id on the remote daemon.
    pub session_id: String,
    /// Confined read-only fork answer.
    pub answer: String,
}

/// Semantic error kind returned by a remote peer daemon so callers reconstruct
/// the exact [`AgentLinkError`] variant without parsing display strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerErrorKind {
    /// Target session does not exist or is archived on that machine.
    NoSuchAgent,
    /// Target session is idle with a cold prompt cache (or its codespace is suspended).
    ColdRecipient,
    /// `btw` target is idle with a cold prompt cache (or its codespace is suspended).
    ColdBtwRecipient,
    /// `btw` target has sent no model request since startup.
    NoForkContext,
    /// `btw` target is too close to its compaction threshold.
    NearCompaction,
    /// Confined fork execution failed on the remote host.
    Fork,
    /// Remote daemon rejected the caller's identity (e.g. non-owner `allowed_logins` peer).
    Forbidden,
    /// Network or daemon error reaching the remote machine.
    Unavailable,
}

/// Structured error body serialized on non-2xx `/api/peers/*` responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerErrorBody {
    /// Machine-readable error classification.
    pub kind: PeerErrorKind,
    /// Subject name or detail string for reconstructing [`AgentLinkError`].
    pub subject: String,
}

impl PeerErrorBody {
    /// Converts a wire error back into [`AgentLinkError`].
    #[must_use]
    pub fn into_link_error(self, machine: &str) -> AgentLinkError {
        match self.kind {
            PeerErrorKind::NoSuchAgent => AgentLinkError::NoSuchAgent(self.subject),
            PeerErrorKind::ColdRecipient => AgentLinkError::ColdRecipient(self.subject),
            PeerErrorKind::ColdBtwRecipient => AgentLinkError::ColdBtwRecipient(self.subject),
            PeerErrorKind::NoForkContext => AgentLinkError::NoForkContext(self.subject),
            PeerErrorKind::NearCompaction => AgentLinkError::NearCompaction(self.subject),
            PeerErrorKind::Fork => AgentLinkError::Fork(self.subject),
            PeerErrorKind::Forbidden => AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: format!("access denied: {}", self.subject),
            },
            PeerErrorKind::Unavailable => AgentLinkError::PeerUnavailable {
                machine: machine.to_string(),
                reason: self.subject,
            },
        }
    }

    /// Encodes a local [`AgentLinkError`] produced while serving a remote peer call.
    #[must_use]
    pub fn from_link_error(error: AgentLinkError) -> Self {
        match error {
            AgentLinkError::NoSuchAgent(s) => Self {
                kind: PeerErrorKind::NoSuchAgent,
                subject: s,
            },
            AgentLinkError::ColdRecipient(s) => Self {
                kind: PeerErrorKind::ColdRecipient,
                subject: s,
            },
            AgentLinkError::ColdBtwRecipient(s) => Self {
                kind: PeerErrorKind::ColdBtwRecipient,
                subject: s,
            },
            AgentLinkError::NoForkContext(s) => Self {
                kind: PeerErrorKind::NoForkContext,
                subject: s,
            },
            AgentLinkError::NearCompaction(s) => Self {
                kind: PeerErrorKind::NearCompaction,
                subject: s,
            },
            AgentLinkError::Fork(s) => Self {
                kind: PeerErrorKind::Fork,
                subject: s,
            },
            other => Self {
                kind: PeerErrorKind::Unavailable,
                subject: other.to_string(),
            },
        }
    }
}

/// Splits `<machine>:<session-id>` when both sides are non-empty and contain no whitespace.
#[must_use]
pub fn split_qualified_target(target: &str) -> Option<(&str, &str)> {
    let (machine, session_id) = target.split_once(':')?;
    if machine.is_empty()
        || session_id.is_empty()
        || machine.chars().any(char::is_whitespace)
        || session_id.chars().any(char::is_whitespace)
        || session_id.contains(':')
    {
        return None;
    }
    Some((machine, session_id))
}

/// Normalizes a DNS label or hostname into a clean machine prefix.
#[must_use]
pub fn sanitize_machine_name(raw: &str) -> String {
    let first_label = raw
        .trim()
        .trim_end_matches('.')
        .split('.')
        .next()
        .unwrap_or("");
    let cleaned: String = first_label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "local".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Cross-machine discovery and RPC transport implemented by `may-daemon`.
pub trait PeerFabric: Send + Sync + 'static {
    /// Short name identifying this daemon in `<machine>:<session-id>` addresses.
    fn local_machine(&self) -> String;

    /// True when `session_id` in local `history.db` is a fronting shadow row for a
    /// Codespaces cloud session rather than a locally hosted agent.
    fn is_cloud_shadow(&self, session_id: &str) -> bool;

    /// Lists sessions across reachable same-owner Tailscale peers and cloud homes.
    fn list_remote(&self, caller_session_id: &str) -> impl Future<Output = RemoteListing> + Send;

    /// Sends a message to `target_session_id` on `machine`, preserving cold-cache refusal.
    fn send_remote(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerSendRequest,
    ) -> impl Future<Output = Result<String, AgentLinkError>> + Send;

    /// Runs a confined read-only `btw` fork against `target_session_id` on `machine`.
    fn btw_remote(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerBtwRequest,
    ) -> impl Future<Output = Result<BtwAnswer, AgentLinkError>> + Send;
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait PeerFabricImpl: Send + Sync + 'static {
    fn local_machine(&self) -> String;
    fn is_cloud_shadow(&self, session_id: &str) -> bool;
    fn list_remote<'a>(&'a self, caller_session_id: &'a str) -> BoxFuture<'a, RemoteListing>;
    fn send_remote<'a>(
        &'a self,
        machine: &'a str,
        target_session_id: &'a str,
        request: PeerSendRequest,
    ) -> BoxFuture<'a, Result<String, AgentLinkError>>;
    fn btw_remote<'a>(
        &'a self,
        machine: &'a str,
        target_session_id: &'a str,
        request: PeerBtwRequest,
    ) -> BoxFuture<'a, Result<BtwAnswer, AgentLinkError>>;
}

impl<T: PeerFabric> PeerFabricImpl for T {
    fn local_machine(&self) -> String {
        PeerFabric::local_machine(self)
    }

    fn is_cloud_shadow(&self, session_id: &str) -> bool {
        PeerFabric::is_cloud_shadow(self, session_id)
    }

    fn list_remote<'a>(&'a self, caller_session_id: &'a str) -> BoxFuture<'a, RemoteListing> {
        Box::pin(PeerFabric::list_remote(self, caller_session_id))
    }

    fn send_remote<'a>(
        &'a self,
        machine: &'a str,
        target_session_id: &'a str,
        request: PeerSendRequest,
    ) -> BoxFuture<'a, Result<String, AgentLinkError>> {
        Box::pin(PeerFabric::send_remote(
            self,
            machine,
            target_session_id,
            request,
        ))
    }

    fn btw_remote<'a>(
        &'a self,
        machine: &'a str,
        target_session_id: &'a str,
        request: PeerBtwRequest,
    ) -> BoxFuture<'a, Result<BtwAnswer, AgentLinkError>> {
        Box::pin(PeerFabric::btw_remote(
            self,
            machine,
            target_session_id,
            request,
        ))
    }
}

/// Type-erased [`PeerFabric`] handle stored on [`super::AgentDirectory`].
#[derive(Clone)]
pub struct AnyPeerFabric(Arc<dyn PeerFabricImpl>);

impl std::fmt::Debug for AnyPeerFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyPeerFabric")
            .field("local_machine", &self.0.local_machine())
            .finish_non_exhaustive()
    }
}

impl AnyPeerFabric {
    /// Wraps `fabric` into a cloneable handle.
    #[must_use]
    pub fn new(fabric: impl PeerFabric) -> Self {
        Self(Arc::new(fabric))
    }

    /// Short name identifying this daemon in `<machine>:<session-id>` addresses.
    #[must_use]
    pub fn local_machine(&self) -> String {
        self.0.local_machine()
    }

    /// True when `session_id` is a fronting shadow row for a cloud session.
    #[must_use]
    pub fn is_cloud_shadow(&self, session_id: &str) -> bool {
        self.0.is_cloud_shadow(session_id)
    }

    /// Lists sessions across reachable Tailscale peers and cloud homes.
    pub async fn list_remote(&self, caller_session_id: &str) -> RemoteListing {
        self.0.list_remote(caller_session_id).await
    }

    /// Sends a message to `target_session_id` on `machine`.
    ///
    /// # Errors
    /// Returns [`AgentLinkError`] when the peer refuses or cannot be reached.
    pub async fn send_remote(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerSendRequest,
    ) -> Result<String, AgentLinkError> {
        self.0
            .send_remote(machine, target_session_id, request)
            .await
    }

    /// Runs a confined read-only `btw` fork against `target_session_id` on `machine`.
    ///
    /// # Errors
    /// Returns [`AgentLinkError`] when the peer refuses or cannot be reached.
    pub async fn btw_remote(
        &self,
        machine: &str,
        target_session_id: &str,
        request: PeerBtwRequest,
    ) -> Result<BtwAnswer, AgentLinkError> {
        self.0.btw_remote(machine, target_session_id, request).await
    }
}
