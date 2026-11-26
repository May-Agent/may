//! Cross-machine and cross-worktree swarm federation primitives.
//!
//! Defines repository-first swarm grouping (`github:owner/repo` / `gitlab:...`
//! with `<machine>:<worktree_root>` fallback), deterministic single-writer
//! Board Host election, `/api/events` SSE board summary payloads, and the
//! [`SwarmTransport`] / [`AnySwarmTransport`] twin.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::SwarmError;
use crate::agent_link::{Activity, AgentStatus, CLOUD_MACHINE};
use crate::db::{SwarmMessageKind, SwarmMessageRow, SwarmReadMode, SwarmReadResult};
use crate::session::{RepoBinding, SessionDirectory};
use crate::trust::RepoKind;

/// Member of an active local or cross-machine swarm board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmMember {
    /// Machine hosting the member (`""` when running without peer transport,
    /// otherwise `"macbook"`, `"mac-mini"`, `"cloud"`, etc.).
    pub machine: String,
    /// Bare session id on `machine` (`"otter"`).
    pub session_id: String,
    /// Address shown in listings and `@mention` disambiguation (`"otter"` for
    /// purely local boards or `"mac-mini:otter"` across machines).
    pub address: String,
    /// Human-readable session title (or bare `session_id` before titling).
    pub name: String,
    /// Runtime activity on its host.
    pub activity: Activity,
    /// True when hosted on a currently `Suspended` Codespaces VM.
    #[serde(default)]
    pub suspended: bool,
    /// Active root items from `tasks.md`.
    #[serde(default)]
    pub roots: Vec<String>,
}

impl SwarmMember {
    /// Converts to [`AgentStatus`] with `session_id` populated from `address`
    /// so existing Askama templates render machine-qualified ids seamlessly.
    #[must_use]
    pub fn as_agent_status(&self) -> AgentStatus {
        AgentStatus {
            session_id: self.address.clone(),
            name: self.name.clone(),
            cwd: std::path::PathBuf::new(),
            activity: self.activity,
            roots: self.roots.clone(),
        }
    }
}

/// Lightweight board summary streamed over `/api/events?topics=swarm` and
/// cached in peer memory for zero-RTT `SwarmConditions` and `CommandSet` checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmBoardSummary {
    /// Canonical swarm group key (`github:owner/repo` or `<machine>:<worktree>`).
    pub group_key: String,
    /// Machine name of the elected authoritative Board Host.
    pub host_machine: String,
    /// Current monotonic board generation.
    pub board_epoch: u64,
    /// Highest message sequence number on this board (`0` when empty).
    pub max_seq: u64,
    /// Active working members across all participating machines.
    pub members: Vec<SwarmMember>,
}

/// SSE frame payload emitted on `/api/events` under event kind `"swarm"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmBoardsSsePayload {
    /// Machine emitting this authoritative board set.
    pub host_machine: String,
    /// Active boards for which `host_machine` is the single-writer Board Host.
    pub boards: Vec<SwarmBoardSummary>,
}

/// Working session reported by a peer daemon or fronted cloud home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkingSession {
    /// Machine hosting the session (`"mac-mini"`, `"cloud"`, etc.).
    pub machine: String,
    /// Machine that can host a board on this session's behalf (for `"cloud"`
    /// sessions fronted by `"macbook"`, `voter_machine` is `"macbook"` so the
    /// fronting daemon always outranks its cloud homes).
    pub voter_machine: String,
    /// Bare session id on `machine`.
    pub session_id: String,
    /// Session display title.
    pub name: String,
    /// Runtime activity.
    pub activity: Activity,
    /// True when the backing Codespace VM is currently `Suspended`.
    #[serde(default)]
    pub suspended: bool,
    /// Active root items from `tasks.md`.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Canonical swarm group key (`github:owner/repo` or `<machine>:<worktree>`).
    pub group_key: String,
}

/// Snapshot of reachable peer swarm participants and mirrored remote boards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerSwarmWorld {
    /// Normalized machine name of this daemon.
    pub local_machine: String,
    /// True when this daemon runs inside a fronted cloud home.
    pub is_cloud_home: bool,
    /// Remote working sessions across online tailnet peers and cloud homes.
    pub remote_sessions: Vec<RemoteWorkingSession>,
    /// Authoritative boards hosted by remote daemons (from SSE mirrors + state poll).
    pub remote_boards: Vec<SwarmBoardSummary>,
}

/// `GET /api/peers/swarm/state` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmStateResponse {
    /// Responder's machine name.
    pub machine: String,
    /// Working sessions hosted (or fronted as `cloud`) by this daemon.
    pub sessions: Vec<RemoteWorkingSession>,
    /// Boards currently hosted authoritatively on this daemon.
    pub hosted_boards: Vec<SwarmBoardSummary>,
}

/// Wire representation of [`SwarmReadMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "since", rename_all = "snake_case")]
pub enum WireSwarmReadMode {
    /// Read entries strictly after the caller's cursor on the Board Host.
    Unread,
    /// Read entries with `seq >= since`.
    Since(u64),
    /// Read entries from `seq = 1`.
    All,
}

impl From<SwarmReadMode> for WireSwarmReadMode {
    fn from(value: SwarmReadMode) -> Self {
        match value {
            SwarmReadMode::Unread => Self::Unread,
            SwarmReadMode::Since(s) => Self::Since(s),
            SwarmReadMode::All => Self::All,
        }
    }
}

impl From<WireSwarmReadMode> for SwarmReadMode {
    fn from(value: WireSwarmReadMode) -> Self {
        match value {
            WireSwarmReadMode::Unread => Self::Unread,
            WireSwarmReadMode::Since(s) => Self::Since(s),
            WireSwarmReadMode::All => Self::All,
        }
    }
}

/// Wire message entry returned from a remote Board Host read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSwarmMessage {
    /// Board generation.
    pub board_epoch: u64,
    /// Monotonic sequence number.
    pub seq: u64,
    /// Optional author address (`<machine>:<session-id>` or `<session-id>`).
    pub session_id: Option<String>,
    /// Display sender name.
    pub sender: String,
    /// `"post"`, `"lifecycle"`, or `"pr"`.
    pub kind: String,
    /// UTF-8 body.
    pub body: String,
    /// Timestamp.
    pub created_at: String,
}

impl WireSwarmMessage {
    /// Converts from [`SwarmMessageRow`].
    #[must_use]
    pub fn from_row(row: SwarmMessageRow) -> Self {
        Self {
            board_epoch: row.board_epoch,
            seq: row.seq,
            session_id: row.session_id,
            sender: row.sender,
            kind: row.kind.as_str().to_string(),
            body: row.body,
            created_at: row.created_at,
        }
    }

    /// Converts back into [`SwarmMessageRow`] for local template rendering.
    #[must_use]
    pub fn into_row(self, group_key: &str) -> SwarmMessageRow {
        let kind = match self.kind.as_str() {
            "lifecycle" => SwarmMessageKind::Lifecycle,
            "pr" => SwarmMessageKind::Pr,
            _ => SwarmMessageKind::Post,
        };
        SwarmMessageRow {
            workdir: group_key.to_string(),
            board_epoch: self.board_epoch,
            seq: self.seq,
            session_id: self.session_id,
            sender: self.sender,
            kind,
            body: self.body,
            created_at: self.created_at,
        }
    }
}

/// `POST /api/peers/swarm/read` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmReadRequest {
    /// Canonical swarm group key.
    pub group_key: String,
    /// Machine-qualified reader key (`<machine>:<session-id>`) for cursor tracking.
    pub reader_key: String,
    /// Read slice mode.
    pub mode: WireSwarmReadMode,
    /// Page size limit.
    pub limit: usize,
}

/// `POST /api/peers/swarm/read` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmReadResponse {
    /// Board generation.
    pub board_epoch: u64,
    /// Entries returned.
    pub messages: Vec<WireSwarmMessage>,
    /// Next unread sequence when truncated.
    pub next_seq: Option<u64>,
    /// Current highest sequence on the board.
    pub max_seq: u64,
    /// Reader's updated cursor on the Board Host.
    pub cursor: u64,
}

impl PeerSwarmReadResponse {
    /// Converts into [`SwarmReadResult`].
    #[must_use]
    pub fn into_read_result(self, group_key: &str) -> (SwarmReadResult, u64) {
        let cursor = self.cursor;
        (
            SwarmReadResult {
                board_epoch: self.board_epoch,
                messages: self
                    .messages
                    .into_iter()
                    .map(|m| m.into_row(group_key))
                    .collect(),
                next_seq: self.next_seq,
                max_seq: self.max_seq,
            },
            cursor,
        )
    }
}

/// `POST /api/peers/swarm/post` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmPostRequest {
    /// Canonical swarm group key.
    pub group_key: String,
    /// Sender's machine prefix.
    pub sender_machine: String,
    /// Sender's bare session id.
    pub sender_session_id: String,
    /// Sender's display title.
    pub sender_name: String,
    /// Plain-text post body.
    pub body: String,
}

/// `POST /api/peers/swarm/post` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmPostResponse {
    /// Board generation.
    pub board_epoch: u64,
    /// Assigned monotonic sequence number.
    pub seq: u64,
    /// Display names of woken `@mention` recipients.
    pub woken: Vec<String>,
    /// Display names of `@mention` recipients whose wake cap fired.
    pub queued_capped: Vec<String>,
}

/// `POST /api/peers/swarm/pr` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmPrRequest {
    /// Canonical swarm group key (`github:owner/repo`, etc.).
    pub group_key: String,
    /// Actor's machine prefix.
    pub actor_machine: String,
    /// Actor's bare session id.
    pub actor_session_id: String,
    /// Actor's display name.
    pub actor_name: String,
    /// PR backend (`"github"`, `"gitlab"`, `"may"`).
    pub backend: String,
    /// Upstream PR/MR number.
    pub number: i64,
    /// PR title.
    pub title: String,
    /// Short or full head SHA.
    pub head_sha: Option<String>,
    /// Short or full merge SHA.
    pub merge_commit_sha: Option<String>,
    /// Event kind (`"opened"`, `"queued"`, `"merged"`, `"closed"`, etc.).
    pub kind: String,
    /// Optional event detail.
    pub detail: Option<String>,
    /// Changed file paths.
    pub changed_files: Vec<String>,
}

/// Push delivery sent from the Board Host to a member's daemon via `POST /api/peers/swarm/deliver`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum PeerSwarmDeliverRequest {
    /// Direct `@mention` steer: wakes cold sessions (compacting first when configured)
    /// and boots `Suspended` codespaces subject to the per-session wake cap.
    Mention {
        /// Canonical swarm group key.
        group_key: String,
        /// Board generation.
        board_epoch: u64,
        /// Sequence number.
        seq: u64,
        /// Recipient bare session id on the target machine.
        target_session_id: String,
        /// Sender display name.
        sender_name: String,
        /// Sender `<machine>:<session-id>` reply address.
        sender_address: String,
        /// Post body.
        body: String,
    },
    /// PR or lifecycle notice: wakes warm online sessions, queues silently on cold
    /// sessions, and buffers on the fronting daemon without booting `Suspended` codespaces.
    Notice {
        /// Canonical swarm group key.
        group_key: String,
        /// Board generation.
        board_epoch: u64,
        /// Sequence number.
        seq: u64,
        /// Recipient bare session id on the target machine.
        target_session_id: String,
        /// Notice kind label (`"PR"`).
        kind: String,
        /// Rendered notice body.
        body: String,
    },
    /// Cleans up unconsumed pending notices / marks unconsumed mentions dissolved
    /// when a board dissolves.
    Dissolved {
        /// Canonical swarm group key.
        group_key: String,
        /// Dissolved board generation.
        board_epoch: u64,
    },
}

/// `POST /api/peers/swarm/deliver` response body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerSwarmDeliverResponse {
    /// True when the target session was woken immediately.
    pub woken: bool,
    /// True when `@mention` wake was suppressed by the unattended loop cap.
    pub capped: bool,
}

/// Structured error returned by `/api/peers/swarm/*` routes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSwarmErrorBody {
    /// `"no_active_board"`, `"ambiguous_mention"`, or `"error"`.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl PeerSwarmErrorBody {
    /// Encodes a [`SwarmError`] for HTTP transmission.
    #[must_use]
    pub fn from_swarm_error(err: SwarmError) -> Self {
        match err {
            SwarmError::NoActiveBoard => Self {
                code: "no_active_board".to_string(),
                message: SwarmError::NoActiveBoard.to_string(),
            },
            SwarmError::AmbiguousMention(msg) => Self {
                code: "ambiguous_mention".to_string(),
                message: msg,
            },
            other => Self {
                code: "error".to_string(),
                message: other.to_string(),
            },
        }
    }

    /// Reconstructs a [`SwarmError`] on the caller daemon.
    #[must_use]
    pub fn into_swarm_error(self) -> SwarmError {
        match self.code.as_str() {
            "no_active_board" => SwarmError::NoActiveBoard,
            "ambiguous_mention" => SwarmError::AmbiguousMention(self.message),
            _ => SwarmError::Remote(self.message),
        }
    }
}

/// Candidate host currently claiming ownership of a board alongside its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncumbentBoardHost<'a> {
    /// Normalized machine name of the daemon currently hosting the board.
    pub machine: &'a str,
    /// Board generation currently hosted by `machine`.
    pub board_epoch: u64,
}

/// Deterministically elects the single-writer Board Host for `group_key` from
/// the voter machines of its reachable members.
///
/// - Local-only groups (`<machine>:<path>`, i.e. not `github:` / `gitlab:`) are
///   always hosted by `<machine>`.
/// - Forge groups (`github:...` / `gitlab:...`) prefer non-`cloud` daemons
///   (fronting daemons vote on behalf of their cloud homes) ordered
///   lexicographically by normalized machine name.
#[must_use]
pub fn elect_board_host<'a>(
    group_key: &str,
    voter_machines: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    if !is_forge_group_key(group_key)
        && let Some((machine, _)) = group_key.split_once(':')
        && !machine.is_empty()
        && !machine.starts_with('/')
    {
        return Some(machine.to_string());
    }
    let mut non_cloud: Vec<&str> = Vec::new();
    let mut any_cloud = false;
    for m in voter_machines {
        if m.is_empty() {
            continue;
        }
        if m == CLOUD_MACHINE {
            any_cloud = true;
        } else {
            non_cloud.push(m);
        }
    }
    non_cloud.sort_unstable();
    non_cloud.dedup();
    non_cloud.first().map_or_else(
        || any_cloud.then(|| CLOUD_MACHINE.to_string()),
        |winner| Some((*winner).to_string()),
    )
}

/// Resolves the authoritative Board Host for `group_key`, retaining an eligible
/// incumbent host across reconciliations and falling back to [`elect_board_host`]
/// only when no eligible incumbent remains.
///
/// Callers pass only incumbents that remain online, are not `draining`, and
/// still have at least one active member or fronting voter in the group. When
/// multiple incumbents claim the same group (e.g. after a transient partition),
/// the highest `board_epoch` wins, breaking ties via [`elect_board_host`].
#[must_use]
pub fn resolve_board_host<'a>(
    group_key: &str,
    eligible_incumbents: impl IntoIterator<Item = IncumbentBoardHost<'a>>,
    voter_machines: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    if !is_forge_group_key(group_key)
        && let Some((machine, _)) = group_key.split_once(':')
        && !machine.is_empty()
        && !machine.starts_with('/')
    {
        return Some(machine.to_string());
    }
    let incumbents: Vec<IncumbentBoardHost<'a>> = eligible_incumbents
        .into_iter()
        .filter(|incumbent| !incumbent.machine.is_empty())
        .collect();
    if let Some(max_epoch) = incumbents.iter().map(|i| i.board_epoch).max() {
        let mut tied: Vec<&str> = incumbents
            .iter()
            .filter(|i| i.board_epoch == max_epoch)
            .map(|i| i.machine)
            .collect();
        tied.sort_unstable();
        tied.dedup();
        if let [single] = tied.as_slice() {
            return Some((*single).to_string());
        }
        return elect_board_host(group_key, tied);
    }
    elect_board_host(group_key, voter_machines)
}

/// True when `group_key` names a canonical forge repository (`github:...` or `gitlab:...`).
#[must_use]
pub fn is_forge_group_key(group_key: &str) -> bool {
    group_key.starts_with("github:") || group_key.starts_with("gitlab:")
}

/// Human-facing label for `group_key` in templates: keeps `github:owner/repo`
/// intact and strips `<machine>:` from local-only `<machine>:<path>` keys when
/// rendering paths.
#[must_use]
pub fn display_group_key(group_key: &str) -> String {
    if is_forge_group_key(group_key) {
        return group_key.to_string();
    }
    if let Some((_, path)) = group_key.split_once(':')
        && path.starts_with('/')
    {
        return crate::fs_utils::display_path(Path::new(path));
    }
    crate::fs_utils::display_path(Path::new(group_key))
}

/// Resolves the repository-first swarm grouping key for a local session row:
/// 1. Stored `RepoBinding::Forge(repo)` -> `repo.canonical_binding()`
/// 2. Checkout's `origin` remote (`RepoKind::Forge { repo }`) -> `repo.canonical_binding()`
/// 3. Local-only checkout -> `<local_machine>:<normalized_worktree_root>` (or
///    bare `<normalized_worktree_root>` when no cross-machine transport is attached).
#[must_use]
pub fn resolve_session_group_key(
    row: &crate::db::entity::sessions::Model,
    may_dir: &Path,
    local_machine: Option<&str>,
) -> Option<String> {
    if let Some(raw_binding) = row.repo.as_deref()
        && let Ok(RepoBinding::Forge(repo)) = RepoBinding::parse(raw_binding)
    {
        return Some(repo.canonical_binding());
    }
    let SessionDirectory::Checkout(checkout) = SessionDirectory::of(row, may_dir) else {
        return None;
    };
    if let RepoKind::Forge { repo } = crate::trust::repo_kind(&checkout) {
        return Some(repo.canonical_binding());
    }
    let norm = super::SwarmService::normalize_checkout(&checkout);
    match local_machine.filter(|m| !m.is_empty()) {
        Some(machine) => Some(format!("{machine}:{norm}")),
        None => Some(norm),
    }
}

///Resolves the canonical swarm group key of a PR repository (`repo_key` + checkout fallback).
#[must_use]
pub fn group_matches_pr_repo(
    group_key: &str,
    repo_key: &str,
    actor_checkout: Option<&Path>,
    workspace_config: &crate::workspace::WorkspaceConfig,
    may_config: &crate::config::MayConfig,
) -> bool {
    if let Some(forge_rest) = group_key.strip_prefix("github:")
        && forge_rest.eq_ignore_ascii_case(repo_key)
    {
        return true;
    }
    if let Some(forge_rest) = group_key.strip_prefix("gitlab:")
        && forge_rest.eq_ignore_ascii_case(repo_key)
    {
        return true;
    }
    let raw_path = if !is_forge_group_key(group_key)
        && let Some((_, path)) = group_key.split_once(':')
        && path.starts_with('/')
    {
        Path::new(path)
    } else if !is_forge_group_key(group_key) {
        Path::new(group_key)
    } else if let Some(checkout) = actor_checkout {
        checkout
    } else {
        return false;
    };
    super::checkout_matches_repo_key(raw_path, repo_key, workspace_config, may_config)
}

/// Cross-daemon transport for federated swarm discovery, SSE mirroring, and Board Host RPCs.
pub trait SwarmTransport: Send + Sync + 'static {
    /// Normalized machine name of this daemon.
    fn local_machine(&self) -> String;

    /// Whether this daemon is currently draining for shutdown/restart and must
    /// step down from hosting boards.
    fn is_draining(&self) -> bool {
        false
    }

    /// Collects reachable remote working sessions and mirrored remote board summaries.
    fn poll_world(&self) -> impl Future<Output = PeerSwarmWorld> + Send;

    /// Notifies SSE subscribers that authoritative boards hosted on this daemon changed.
    fn notify_hosted_boards_changed(&self);

    /// Routes `board read` to remote `host_machine`.
    fn read_remote_board(
        &self,
        host_machine: &str,
        request: PeerSwarmReadRequest,
    ) -> impl Future<Output = Result<PeerSwarmReadResponse, SwarmError>> + Send;

    /// Routes `board post` to remote `host_machine`.
    fn post_remote_board(
        &self,
        host_machine: &str,
        request: PeerSwarmPostRequest,
    ) -> impl Future<Output = Result<PeerSwarmPostResponse, SwarmError>> + Send;

    /// Routes a PR transition announcement to remote `host_machine`.
    fn announce_remote_pr(
        &self,
        host_machine: &str,
        request: PeerSwarmPrRequest,
    ) -> impl Future<Output = Result<(), SwarmError>> + Send;

    /// Delivers a `@mention` steer, PR notice, or dissolution cleanup from this
    /// Board Host to a member on `target_machine` (including `cloud` homes).
    fn deliver_to_peer(
        &self,
        target_machine: &str,
        request: PeerSwarmDeliverRequest,
    ) -> impl Future<Output = Result<PeerSwarmDeliverResponse, SwarmError>> + Send;
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait SwarmTransportImpl: Send + Sync + 'static {
    fn local_machine(&self) -> String;
    fn is_draining(&self) -> bool;
    fn poll_world(&self) -> BoxFuture<'_, PeerSwarmWorld>;
    fn notify_hosted_boards_changed(&self);
    fn read_remote_board<'a>(
        &'a self,
        host_machine: &'a str,
        request: PeerSwarmReadRequest,
    ) -> BoxFuture<'a, Result<PeerSwarmReadResponse, SwarmError>>;
    fn post_remote_board<'a>(
        &'a self,
        host_machine: &'a str,
        request: PeerSwarmPostRequest,
    ) -> BoxFuture<'a, Result<PeerSwarmPostResponse, SwarmError>>;
    fn announce_remote_pr<'a>(
        &'a self,
        host_machine: &'a str,
        request: PeerSwarmPrRequest,
    ) -> BoxFuture<'a, Result<(), SwarmError>>;
    fn deliver_to_peer<'a>(
        &'a self,
        target_machine: &'a str,
        request: PeerSwarmDeliverRequest,
    ) -> BoxFuture<'a, Result<PeerSwarmDeliverResponse, SwarmError>>;
}

impl<T: SwarmTransport> SwarmTransportImpl for T {
    fn local_machine(&self) -> String {
        SwarmTransport::local_machine(self)
    }

    fn is_draining(&self) -> bool {
        SwarmTransport::is_draining(self)
    }

    fn poll_world(&self) -> BoxFuture<'_, PeerSwarmWorld> {
        Box::pin(SwarmTransport::poll_world(self))
    }

    fn notify_hosted_boards_changed(&self) {
        SwarmTransport::notify_hosted_boards_changed(self);
    }

    fn read_remote_board<'a>(
        &'a self,
        host_machine: &'a str,
        request: PeerSwarmReadRequest,
    ) -> BoxFuture<'a, Result<PeerSwarmReadResponse, SwarmError>> {
        Box::pin(SwarmTransport::read_remote_board(
            self,
            host_machine,
            request,
        ))
    }

    fn post_remote_board<'a>(
        &'a self,
        host_machine: &'a str,
        request: PeerSwarmPostRequest,
    ) -> BoxFuture<'a, Result<PeerSwarmPostResponse, SwarmError>> {
        Box::pin(SwarmTransport::post_remote_board(
            self,
            host_machine,
            request,
        ))
    }

    fn announce_remote_pr<'a>(
        &'a self,
        host_machine: &'a str,
        request: PeerSwarmPrRequest,
    ) -> BoxFuture<'a, Result<(), SwarmError>> {
        Box::pin(SwarmTransport::announce_remote_pr(
            self,
            host_machine,
            request,
        ))
    }

    fn deliver_to_peer<'a>(
        &'a self,
        target_machine: &'a str,
        request: PeerSwarmDeliverRequest,
    ) -> BoxFuture<'a, Result<PeerSwarmDeliverResponse, SwarmError>> {
        Box::pin(SwarmTransport::deliver_to_peer(
            self,
            target_machine,
            request,
        ))
    }
}

/// Type-erased [`SwarmTransport`] stored on [`super::SwarmService`].
#[derive(Clone)]
pub struct AnySwarmTransport(Arc<dyn SwarmTransportImpl>);

impl std::fmt::Debug for AnySwarmTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnySwarmTransport").finish_non_exhaustive()
    }
}

impl AnySwarmTransport {
    /// Wraps a concrete [`SwarmTransport`].
    #[must_use]
    pub fn new(inner: impl SwarmTransport) -> Self {
        Self(Arc::new(inner))
    }

    /// Normalized machine name of this daemon.
    #[must_use]
    pub fn local_machine(&self) -> String {
        self.0.local_machine()
    }

    /// Whether this daemon is currently draining for shutdown/restart.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.0.is_draining()
    }

    /// Polls remote working sessions and mirrored boards.
    pub async fn poll_world(&self) -> PeerSwarmWorld {
        self.0.poll_world().await
    }

    /// Emits `UiEventTopics::SWARM` when hosted boards change.
    pub fn notify_hosted_boards_changed(&self) {
        self.0.notify_hosted_boards_changed();
    }

    /// Routes `board read` to `host_machine`.
    ///
    /// # Errors
    /// Propagates [`SwarmError`].
    pub async fn read_remote_board(
        &self,
        host_machine: &str,
        request: PeerSwarmReadRequest,
    ) -> Result<PeerSwarmReadResponse, SwarmError> {
        self.0.read_remote_board(host_machine, request).await
    }

    /// Routes `board post` to `host_machine`.
    ///
    /// # Errors
    /// Propagates [`SwarmError`].
    pub async fn post_remote_board(
        &self,
        host_machine: &str,
        request: PeerSwarmPostRequest,
    ) -> Result<PeerSwarmPostResponse, SwarmError> {
        self.0.post_remote_board(host_machine, request).await
    }

    /// Routes a PR announcement to `host_machine`.
    ///
    /// # Errors
    /// Propagates [`SwarmError`].
    pub async fn announce_remote_pr(
        &self,
        host_machine: &str,
        request: PeerSwarmPrRequest,
    ) -> Result<(), SwarmError> {
        self.0.announce_remote_pr(host_machine, request).await
    }

    /// Delivers a steer, notice, or dissolution signal to `target_machine`.
    ///
    /// # Errors
    /// Propagates [`SwarmError`].
    pub async fn deliver_to_peer(
        &self,
        target_machine: &str,
        request: PeerSwarmDeliverRequest,
    ) -> Result<PeerSwarmDeliverResponse, SwarmError> {
        self.0.deliver_to_peer(target_machine, request).await
    }
}
