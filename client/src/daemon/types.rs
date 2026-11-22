use crate::session::{
    AttachmentKind, PendingUserMessage, PendingUserMessageMode, SessionPlanState,
    SessionRuntimeState,
};
use aither_core::llm::ToolResult;
use aither_sandbox::network_policy::NetworkPolicyMode;
use may_config::config::{
    BrowserBackendKind, CommandPriority, NativeToolsConfig, ProviderApi, ProviderType,
    RemoteAccessConfig, SshServerConfig,
};
use may_protocol::xml::XmlBlock;

// ── Typed API Errors ────────────────────────────────────────

/// Machine-readable error codes sent to the frontend, eliminating string-matching.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    /// The request body or parameters could not be read (400).
    BadRequest,
    /// The daemon failed while serving the request (500).
    InternalError,
    /// The named resource does not exist (404).
    NotFound,
    /// A browser takeover is in progress for the session.
    BrowserTakeoverActive,
    /// The session id names no session.
    SessionNotFound,
    /// No model is configured for the session.
    ModelNotConfigured,
    /// The provider the request needs is not reachable.
    ProviderUnavailable,
    /// The profile's configuration is invalid.
    ProfileConfigurationInvalid,
    /// The session's container is still being prepared.
    ContainerPreparing,
    /// Pulling the container image failed.
    ContainerPullFailed,
    /// Starting the container failed.
    ContainerStartFailed,
    /// No container runtime is available.
    ContainerRuntimeUnavailable,
    /// The managed Podman install needs the user's approval.
    ManagedPodmanApprovalRequired,
    /// The named location is not available.
    LocationUnavailable,
    /// The SSH server names no configured server.
    UnknownSshServer,
    /// The host has no usable GitHub login.
    GithubUnauthenticated,
    /// The cloud provider account `/cloud` set up cannot create machines:
    /// its login is missing, lacks a permission, or the provider refused it.
    CloudAccountUnusable,
    /// The user declined, at the session's repository question, to work where
    /// the session starts. The turn did not run; nothing failed.
    RepoDeclined,
    /// The daemon ended the turn because the agent kept looping after it was
    /// alerted; the message says which loop. Nothing failed, and the session
    /// takes the next message.
    LoopStopped,
    /// The cloud session's codespace is suspended; only a message starts
    /// it. Nothing failed.
    SessionSuspended,
}

/// Structured error body with a machine-readable `code` and human-readable `error` message.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct ApiErrorBody {
    /// The machine-readable error code.
    pub code: ApiErrorCode,
    /// The human-readable error message.
    pub error: String,
}

// ── SSE Event Payloads ──────────────────────────────────────

/// Semantic variant of a tool call, selected on `tool_start`.
///
/// The frontend routes on it without re-inferring from the tool name.
/// `Generic` covers MCP and unknown tools; the frontend falls back to
/// raw-arg partial JSON projection there.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolVariant {
    /// The `terminal` tool.
    Terminal,
    /// The `terminal_input` tool.
    TerminalInput,
    /// The `terminal_read` tool.
    TerminalRead,
    /// The `browser` tool.
    Browser,
    /// The `web_search` tool.
    WebSearch,
    /// The `web_fetch` tool.
    WebFetch,
    /// The `ask_user` command.
    Ask,
    /// The `session` command.
    Session,
    /// The `task` command.
    Task,
    /// The `compact` tool.
    Compact,
    /// Any other tool; the frontend renders raw arguments.
    Generic,
}

impl ToolVariant {
    /// Map a tool name to its rendering variant. Unknown tools fall
    /// back to `Generic` so MCP and user-defined CLIs still stream.
    #[must_use]
    pub fn infer(tool_name: &str) -> Self {
        match tool_name {
            "terminal" => Self::Terminal,
            "terminal_input" => Self::TerminalInput,
            "terminal_read" => Self::TerminalRead,
            "browser" => Self::Browser,
            "web_search" | "websearch" => Self::WebSearch,
            "web_fetch" | "webfetch" => Self::WebFetch,
            "ask" => Self::Ask,
            "session" => Self::Session,
            "task" => Self::Task,
            "compact" => Self::Compact,
            _ => Self::Generic,
        }
    }
}

/// The `session_turn` start frame.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseStartEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The session the turn belongs to.
    pub session_id: &'a str,
    /// Id assigned to the assistant reply in progress.
    pub assistant_id: &'a str,
}

/// A streamed assistant text chunk of `session_turn`.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseChunkEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The streamed text chunk.
    pub chunk: &'a str,
}

/// A tool call's start frame of `session_turn`.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseToolStartEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The tool call's id.
    pub id: &'a str,
    /// The tool's name.
    pub name: &'a str,
    /// The tool's rendering variant.
    pub variant: ToolVariant,
    /// The call's arguments, as raw JSON.
    pub arguments: &'a str,
    /// Backend-projected summary fields. The frontend reads these directly
    /// for the summary chip; no client-side partial-JSON parsing required.
    #[serde(skip_serializing_if = "crate::daemon::tool_summary::ToolSummary::is_empty")]
    pub summary: crate::daemon::tool_summary::ToolSummary,
}

/// An arguments fragment of a streaming tool call.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseToolDeltaEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The tool call's id.
    pub id: &'a str,
    /// The tool's name.
    pub name: &'a str,
    /// The tool's rendering variant.
    pub variant: ToolVariant,
    /// The next fragment of the arguments JSON.
    pub arguments_fragment: &'a str,
    /// Backend-projected summary fields, recomputed from the cumulative
    /// argument buffer at every delta.
    #[serde(skip_serializing_if = "crate::daemon::tool_summary::ToolSummary::is_empty")]
    pub summary: crate::daemon::tool_summary::ToolSummary,
}

/// A tool call's end frame of `session_turn`.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseToolEndEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The tool call's id.
    pub id: &'a str,
    /// The tool's name.
    pub name: &'a str,
    /// The tool's rendering variant.
    pub variant: ToolVariant,
    /// The call's final arguments, when re-sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<&'a str>,
    /// The call's result.
    #[schema(value_type = serde_json::Value)]
    #[serde(serialize_with = "crate::session::tool_result_wire::serialize")]
    pub result: &'a ToolResult,
    /// True when the call ended in a refusal — an unchanged re-read the
    /// policy rejected — not a failure; the task it read stays running.
    #[serde(default)]
    pub refused: bool,
    /// Backend-projected summary fields, as the start frame carries.
    #[serde(skip_serializing_if = "crate::daemon::tool_summary::ToolSummary::is_empty")]
    pub summary: crate::daemon::tool_summary::ToolSummary,
}

/// A `session_turn` error frame.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseErrorEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The machine-readable error code.
    pub code: ApiErrorCode,
    /// The human-readable error message.
    pub message: &'a str,
    /// Whether retrying the same request may succeed.
    pub retryable: bool,
}

/// The `session_turn` completion frame.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseDoneEvent {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: String,
    /// The session the turn belongs to.
    pub session_id: String,
    /// The stored user message the turn answers, named as REST names it.
    pub user_message_id: Option<String>,
    /// Every assistant message the turn stored, named as REST names them;
    /// `assistant` carries the first.
    pub message_ids: Vec<String>,
    /// The assistant reply the turn stored.
    pub assistant: ApiMessage,
}

/// A turn a browser takeover paused. It carries the reply only when the
/// turn stored one before the pause.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SsePausedEvent {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: String,
    /// The session the item belongs to.
    pub session_id: String,
    /// The stored user message the turn answers, named as REST names it.
    pub user_message_id: Option<String>,
    /// Every assistant message the turn stored.
    pub message_ids: Vec<String>,
    /// The stored reply, when the turn stored one before the pause.
    pub assistant: Option<ApiMessage>,
}

/// A keep-alive frame.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct SseHeartbeat {
    /// Always `true`.
    pub ok: bool,
}

/// Live decode-speed sample emitted while model output streams.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseSpeedEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// The current decode speed.
    pub tokens_per_second: f64,
}

/// A `session_turn` frame parking the turn for `reason`.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseHoldEvent<'a> {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: &'a str,
    /// Why the turn is held.
    #[serde(flatten)]
    pub reason: crate::client::HoldReason,
    /// The hold's display text.
    pub message: &'a str,
}

/// A `session_tasks` frame carrying the current `tasks.md` rows.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseTodoUpdateEvent {
    /// Event sequence number in the stream.
    pub seq: u64,
    /// The turn the event belongs to.
    pub turn_id: String,
    /// The rows of `tasks.md`'s row sections; empty once every row is gone.
    pub rows: Vec<may_protocol::task_rows::TaskRow>,
}

// ── Tool Event Records (stored in DB history) ───────────────

pub use may_protocol::session_messages::ToolEventRecord;

// ── Config SSE Event Payload ────────────────────────────────

/// The `browser_status` event payload.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatusEventPayload {
    /// Whether browser automation is enabled.
    pub enabled: bool,
    /// Whether the managed browser is running.
    pub running: bool,
    /// The browser's CDP endpoint, while running.
    pub cdp_endpoint: Option<String>,
    /// The session owning the browser.
    pub owner_session_id: Option<String>,
    /// The session holding the takeover, while one does.
    pub takeover_session_id: Option<String>,
}

// ── Browser Lifecycle SSE Event Payload ─────────────────────

pub use may_protocol::cloud::BrowserLifecycleEvent;

// ── Generic OK Responses ────────────────────────────────────

/// Answer of the browser takeover endpoints.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTakeoverResponse {
    /// Whether the takeover state changed as asked.
    pub ok: bool,
    /// The session the takeover belongs to.
    pub session_id: String,
}

/// The outcome of a task trigger request.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskTriggerStatus {
    /// The due tasks were triggered.
    Triggered,
}

/// Answer of `POST /api/tasks/trigger`.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct TaskTriggerResponse {
    /// Whether the trigger ran.
    pub ok: bool,
    /// What the trigger did.
    pub status: TaskTriggerStatus,
    /// How many tasks fired.
    pub triggered: usize,
}

/// Answer of the notification send endpoint.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct NotificationSentResponse {
    /// Whether the send was attempted.
    pub ok: bool,
    /// How many notifications were sent.
    pub sent: usize,
}

/// Where a mailbox notification is.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMailboxStatus {
    /// Waiting to be delivered.
    Pending,
    /// Delivered.
    Delivered,
    /// Delivery failed.
    Failed,
}

impl NotificationMailboxStatus {
    fn from_stored(raw: &str) -> Self {
        match raw {
            "pending" => Self::Pending,
            "delivered" => Self::Delivered,
            "failed" => Self::Failed,
            other => panic!("unknown notification mailbox status: {other}"),
        }
    }
}

/// One mailbox notification row over the wire.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationMailboxEntryResponse {
    /// Mailbox row id.
    pub id: String,
    /// Notification title.
    pub title: String,
    /// Notification body.
    pub body: String,
    /// Session the notification belongs to, when any.
    pub session_id: Option<String>,
    /// Where the notification is.
    pub status: NotificationMailboxStatus,
    /// How many times delivery was attempted.
    pub sent_count: i32,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
    /// Delivery time.
    pub delivered_at: Option<String>,
    /// Creation time.
    pub created_at: String,
    /// Last update time.
    pub updated_at: String,
}

impl From<may_db::entity::notification_mailbox::Model> for NotificationMailboxEntryResponse {
    fn from(value: may_db::entity::notification_mailbox::Model) -> Self {
        Self {
            id: value.id,
            title: value.title,
            body: value.body,
            session_id: value.session_id,
            status: NotificationMailboxStatus::from_stored(&value.status),
            sent_count: value.sent_count,
            last_error: value.last_error,
            delivered_at: value.delivered_at,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// The mailbox's notifications.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationMailboxResponse {
    /// The entries, newest first.
    pub items: Vec<NotificationMailboxEntryResponse>,
}

/// One network access log row over the wire.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAccessLogEntryResponse {
    /// Log row id.
    pub id: i32,
    /// Session the request came from, when any.
    pub session_id: Option<String>,
    /// Profile the request came from, when any.
    pub profile_id: Option<String>,
    /// What made the request.
    pub source: may_db::NetworkAccessSource,
    /// The policy mode in force.
    pub mode: may_db::NetworkAccessMode,
    /// The requested domain.
    pub domain: String,
    /// The requested port.
    pub port: i32,
    /// Whether the request was allowed.
    pub allowed: bool,
    /// Why it was refused, when it was.
    pub reason: Option<String>,
    /// When the request was made.
    pub timestamp: String,
}

impl From<may_db::entity::network_access_logs::Model> for NetworkAccessLogEntryResponse {
    fn from(value: may_db::entity::network_access_logs::Model) -> Self {
        Self {
            id: value.id,
            session_id: value.session_id,
            profile_id: value.profile_id,
            source: value
                .source
                .parse()
                .expect("network access log source must be valid"),
            mode: value
                .mode
                .parse()
                .expect("network access log mode must be valid"),
            domain: value.domain,
            port: value.port,
            allowed: value.allowed,
            reason: value.reason,
            timestamp: value.timestamp,
        }
    }
}

/// The network access log entries.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAccessLogsResponse {
    /// The entries, newest first.
    pub items: Vec<NetworkAccessLogEntryResponse>,
}

/// Where remote access is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
pub enum RemoteAccessState {
    /// Remote access is off.
    Disabled,
    /// Remote access is coming up.
    Starting,
    /// Remote access is up.
    Running,
    /// Remote access failed.
    #[serde(rename_all = "snake_case")]
    Error,
}

/// The login owning this node, as `tailscale status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerLogin {
    /// The owner login was resolved.
    Resolved {
        /// The login.
        login: String,
    },
    /// It could not be resolved.
    Unresolved {
        /// Why it could not be resolved.
        reason: String,
    },
}

/// Who remote access accepts right now: the node owner (when resolved) plus
/// the listed logins.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedLogins {
    /// The login owning this node.
    pub owner: OwnerLogin,
    /// The configured allowed logins.
    pub listed: Vec<String>,
}

impl AcceptedLogins {
    /// The accepted logins as the identity sources' allow-list.
    #[must_use]
    pub fn allow_list(&self) -> std::collections::HashSet<String> {
        let owner = match &self.owner {
            OwnerLogin::Resolved { login } => Some(login.clone()),
            OwnerLogin::Unresolved { .. } => None,
        };
        owner
            .into_iter()
            .chain(self.listed.iter().cloned())
            .collect()
    }
}

/// The remote access status the API reports.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccessStatusResponse {
    /// Where remote access is.
    pub state: RemoteAccessState,
    /// Whether remote access is configured on.
    pub enabled: bool,
    /// The tailnet URL the daemon serves at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// The internet-facing base URL of the webhook routes, while Funnel is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funnel_url: Option<String>,
    /// Why it last failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Recent remote-access log lines.
    #[serde(default)]
    pub recent_logs: Vec<String>,
    /// Who is accepted now; absent until the allow-list is first resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<AcceptedLogins>,
}

/// The `remote_access` event payload.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccessEventPayload {
    /// The current status.
    pub status: RemoteAccessStatusResponse,
}

/// The session's `tasks.md` over the wire.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionTasksPayload {
    /// The file's text.
    pub content: String,
    /// The session's plan-mode state.
    #[serde(default)]
    pub plan_state: SessionPlanState,
    /// The parsed rows of `content`; set by the daemon, ignored on save.
    #[serde(default)]
    pub rows: Vec<may_protocol::task_rows::TaskRow>,
}

/// One `prs` row over the wire — the session's PR list entry.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiPr {
    /// Global database PR row id (used by `POST /api/prs/{id}/merge` and `/close`).
    pub id: i64,
    /// Repository identity key (local checkout identity or GitHub URL).
    pub repo_key: String,
    /// Owning session id when opened (or last adopted).
    pub session_id: String,
    /// Whether the owning session no longer exists.
    pub orphaned: bool,
    /// Whether repository policy requires user review before landing.
    pub require_review: bool,
    /// Per-repository PR number.
    pub number: i64,
    /// PR title.
    pub title: String,
    /// Where the PR stands.
    pub state: may_db::PrState,
    /// `may`, `github` or `gitlab`.
    pub backend: String,
    /// Associated issue number, when linked.
    pub issue_number: Option<i64>,
    /// Whether the PR merges itself once checks pass.
    pub auto_merge: bool,
    /// Base commit SHA at open.
    pub base_sha: String,
    /// Pinned head commit SHA, if set.
    pub head_sha: Option<String>,
    /// Merge commit SHA when merged locally.
    pub merge_commit_sha: Option<String>,
    /// UTC timestamps.
    pub created_at: String,
    /// Latest update.
    pub updated_at: String,
    /// Merge timestamp, when merged.
    pub merged_at: Option<String>,
    /// Live check rollup of the PR's head, queried from GitHub for an open
    /// GitHub PR; `None` for other backends and landed states.
    pub checks: Option<ApiPrChecks>,
    /// Why the last read of the checks failed, when it did.
    pub checks_error: Option<String>,
    /// Recent transition events, oldest first.
    pub events: Vec<ApiPrEvent>,
}

/// How many of a PR's checks sit in each state — the rollup GitHub shows.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiPrChecks {
    /// Still running or not yet reported.
    pub pending: u32,
    /// Concluded successfully (neutral and skipped included).
    pub passed: u32,
    /// Failed, timed out, cancelled or errored.
    pub failed: u32,
    /// Each check with its state, in GitHub's order.
    pub items: Vec<ApiPrCheck>,
}

/// One check of a PR's head commit.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiPrCheck {
    /// Check run or status context name.
    pub name: String,
    /// `queued`, `running`, `passed` or `failed`.
    pub state: String,
    /// When it started, RFC 3339; absent while queued.
    pub started_at: Option<String>,
    /// When it concluded, RFC 3339.
    pub finished_at: Option<String>,
    /// Where GitHub shows the check (its detail and log).
    pub url: Option<String>,
}

/// What the daemon last read of a PR's checks: the report, or why GitHub
/// could not be read.
pub type PrChecksRead = Result<may_protocol::pr_checks::ChecksReport, String>;

impl ApiPrChecks {
    /// The wire form of `report`.
    #[must_use]
    pub fn of(report: &may_protocol::pr_checks::ChecksReport) -> Self {
        let summary = report.summary();
        Self {
            pending: summary.pending,
            passed: summary.passed,
            failed: summary.failed,
            items: report
                .checks
                .iter()
                .map(|check| ApiPrCheck {
                    name: check.name.clone(),
                    state: check.state.as_str().to_string(),
                    started_at: check.started().map(|t| t.to_string()),
                    finished_at: check.finished().map(|t| t.to_string()),
                    url: check.url.clone(),
                })
                .collect(),
        }
    }
}

/// One `pr_events` row over the wire.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiPrEvent {
    /// Session recording the transition.
    pub session_id: String,
    /// Actor attribution (`main`, `user`, or subagent id).
    pub actor: String,
    /// Associated message id in `messages`, if any.
    pub message_id: Option<i64>,
    /// Transition kind (`opened`, `queued`, `merged`, ...).
    pub kind: String,
    /// Event payload (failure excerpt, merge sha, conflict list).
    pub detail: Option<String>,
    /// UTC time.
    pub created_at: String,
}

/// The health probe answer.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiHealth {
    /// Always `true`.
    pub ok: bool,
}

/// The health probe's answer.
pub const API_OK: ApiHealth = ApiHealth { ok: true };

/// `GET /api/health`: the daemon is up, and which build it runs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonHealth {
    /// Always `true`.
    pub ok: bool,
    /// Daemon process id.
    pub pid: u32,
    /// Build id of the daemon executable.
    pub build: String,
    /// Search indexer state.
    pub search_index: crate::repo_search::indexer::IndexerSnapshot,
    /// The loopback TCP listener the web UI and API are bound to, when one
    /// serves (`rest`/`gui`), as `host:port`.
    #[serde(default)]
    pub local_listener: Option<String>,
}

/// `PUT /api/daemon/local-listener` body: the loopback port the web
/// UI/API listener is ensured on; `null` uses `[daemon] port` or a fresh
/// pick the daemon pins.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalListenerRequest {
    /// The port to bind.
    #[serde(default)]
    pub port: Option<u16>,
}

/// Its answer: the port the local listener serves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalListenerResponse {
    /// The port the local listener serves.
    pub port: u16,
}

/// Token and timing stats of a provider request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestStats {
    /// The provider the request went to.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// Prompt tokens billed.
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    /// Completion tokens billed.
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    /// Total tokens billed.
    #[serde(default)]
    pub total_tokens: Option<u32>,
    /// Prompt tokens read from cache.
    #[serde(default)]
    pub cache_read_tokens: Option<u32>,
    /// Prompt tokens written to cache.
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
    /// The model's context window.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Request wall time in milliseconds.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

/// One request log row over the wire.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogEntry {
    /// The session the request belonged to.
    pub session_id: String,
    /// When the request was made.
    pub timestamp: String,
    /// The model the request ran on.
    pub model: String,
    /// The provider the request went to.
    pub provider_id: String,
    /// Prompt tokens billed.
    pub prompt_tokens: Option<u32>,
    /// Completion tokens billed.
    pub completion_tokens: Option<u32>,
    /// Total tokens billed.
    pub total_tokens: Option<u32>,
    /// Prompt tokens read from cache.
    pub cache_read_tokens: Option<u32>,
    /// Prompt tokens written to cache.
    pub cache_write_tokens: Option<u32>,
    /// The model's context window.
    pub context_window: Option<u32>,
    /// Request wall time in milliseconds.
    pub elapsed_ms: u64,
    /// Whether the request succeeded.
    pub ok: bool,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// Whether the request only refreshed the prompt cache.
    pub keep_warm: bool,
}

impl From<may_db::entity::request_logs::Model> for RequestLogEntry {
    fn from(value: may_db::entity::request_logs::Model) -> Self {
        Self {
            session_id: value.session_id,
            timestamp: value.timestamp,
            model: value.model,
            provider_id: value.provider_id,
            prompt_tokens: value
                .prompt_tokens
                .and_then(|entry| u32::try_from(entry).ok()),
            completion_tokens: value
                .completion_tokens
                .and_then(|entry| u32::try_from(entry).ok()),
            total_tokens: value
                .total_tokens
                .and_then(|entry| u32::try_from(entry).ok()),
            cache_read_tokens: value
                .cache_read_tokens
                .and_then(|entry| u32::try_from(entry).ok()),
            cache_write_tokens: value
                .cache_write_tokens
                .and_then(|entry| u32::try_from(entry).ok()),
            context_window: value
                .context_window
                .and_then(|entry| u32::try_from(entry).ok()),
            elapsed_ms: u64::try_from(value.elapsed_ms).unwrap_or(0),
            ok: value.ok,
            error: value.error,
            keep_warm: value.keep_warm,
        }
    }
}

/// Prompt-cache usage across a session's requests.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogCacheSummary {
    /// Assistant messages counted.
    pub assistant_message_count: u64,
    /// Messages whose prompt was served from cache.
    pub cache_hit_message_count: u64,
    /// Prompt tokens billed in all.
    pub prompt_tokens_total: u64,
    /// Tokens read from cache in all.
    pub cache_read_tokens_total: u64,
    /// Tokens written to cache in all.
    pub cache_write_tokens_total: u64,
    /// Cache-read share of prompt tokens; absent when no prompt tokens.
    pub cache_read_hit_rate: Option<f64>,
}

impl From<may_db::AssistantCacheUsageSummary> for RequestLogCacheSummary {
    fn from(value: may_db::AssistantCacheUsageSummary) -> Self {
        let cache_read_hit_rate = if value.prompt_tokens_total == 0 {
            None
        } else {
            Some(value.cache_read_tokens_total as f64 / value.prompt_tokens_total as f64)
        };
        Self {
            assistant_message_count: value.assistant_message_count,
            cache_hit_message_count: value.cache_hit_message_count,
            prompt_tokens_total: value.prompt_tokens_total,
            cache_read_tokens_total: value.cache_read_tokens_total,
            cache_write_tokens_total: value.cache_write_tokens_total,
            cache_read_hit_rate,
        }
    }
}

impl RequestLogEntry {
    /// Create a base request-log entry; finalize via `success`/`failure`.
    #[must_use]
    pub fn new(
        session_id: &str,
        model: &str,
        provider_id: &str,
        usage: &aither_core::llm::Usage,
        context_window: Option<u32>,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            timestamp: jiff::Timestamp::now().to_string(),
            model: model.to_string(),
            provider_id: provider_id.to_string(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens.or_else(|| {
                usage
                    .prompt_tokens
                    .zip(usage.completion_tokens)
                    .map(|(p, c)| p + c)
            }),
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            context_window,
            elapsed_ms: 0,
            ok: false,
            error: None,
            keep_warm: false,
        }
    }

    /// Marks the entry as a request that only refreshed the prompt cache.
    #[must_use]
    pub const fn keep_warm(mut self) -> Self {
        self.keep_warm = true;
        self
    }

    /// The row this entry is stored as.
    #[must_use]
    pub fn as_params(&self) -> may_db::RequestLogParams<'_> {
        may_db::RequestLogParams {
            session_id: self.session_id.as_str(),
            timestamp: self.timestamp.as_str(),
            model: self.model.as_str(),
            provider_id: self.provider_id.as_str(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            context_window: self.context_window,
            elapsed_ms: self.elapsed_ms,
            ok: self.ok,
            error: self.error.as_deref(),
            keep_warm: self.keep_warm,
        }
    }

    /// Marks the entry as a request that succeeded in `elapsed_ms`.
    #[must_use]
    pub const fn success(mut self, elapsed_ms: u64) -> Self {
        self.ok = true;
        self.elapsed_ms = elapsed_ms;
        self
    }

    /// Marks the entry as failed in `elapsed_ms`.
    #[must_use]
    pub fn failure(mut self, elapsed_ms: u64, error: impl Into<String>) -> Self {
        self.ok = false;
        self.elapsed_ms = elapsed_ms;
        self.error = Some(error.into());
        self
    }
}

/// A session's request log entries with their cache rollup.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogsResponse {
    /// The entries, newest first.
    pub logs: Vec<RequestLogEntry>,
    /// Prompt-cache usage across them.
    pub cache_summary: RequestLogCacheSummary,
}

/// A memory's text.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryPayload {
    /// The memory text.
    pub content: String,
}

/// An attachment over the wire.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiAttachment {
    /// Attachment id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// What kind of attachment.
    #[serde(rename = "type")]
    pub attachment_type: AttachmentKind,
    /// Where the content is served from.
    pub url: String,
    /// Text content, when the attachment carries it inline.
    #[serde(default)]
    pub text_content: Option<String>,
}

/// Native terminal command tool name.
///
/// This identifies the terminal command execution tool in the protocol.
pub const TERMINAL_TOOL_NAME: &str = "terminal";

/// Where a terminal execution is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
pub enum TerminalExecutionStatus {
    /// Still running.
    Running,
    /// Finished successfully.
    Done,
    /// Failed.
    Failed,
    /// Stopped before finishing.
    #[serde(rename_all = "lowercase")]
    Stopped,
}

/// How a terminal command runs.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
pub enum TerminalCommandMode {
    /// In the sandbox.
    Default,
    /// On the host, outside the sandbox.
    #[serde(rename_all = "lowercase")]
    Unsafe,
}

/// One terminal execution over the wire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiTerminalExecution {
    /// Execution id.
    pub id: String,
    /// What the run does.
    pub description: String,
    /// The script run.
    pub input: String,
    /// Its output so far, when captured.
    #[serde(default)]
    pub output: Option<String>,
    /// Where the execution is.
    pub status: TerminalExecutionStatus,
    /// How it runs.
    pub mode: TerminalCommandMode,
}

/// A session's terminal executions, past and running.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStateSnapshot {
    /// Every execution, oldest first.
    #[serde(default)]
    pub executions: Vec<ApiTerminalExecution>,
    /// Running executions by tool call id.
    #[serde(default)]
    pub running_by_call_id: std::collections::HashMap<String, ApiTerminalExecution>,
}

/// A chat message over the wire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiMessage {
    /// Message id.
    pub id: String,
    /// The sender's role.
    pub role: String,
    /// Message text.
    pub content: String,
    /// When it was sent.
    pub timestamp: String,
    /// The message's attachments.
    #[serde(default)]
    pub attachments: Vec<ApiAttachment>,
    /// Reasoning text streamed before the reply.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Whether the message is still streaming.
    #[serde(default)]
    pub is_streaming: Option<bool>,
    /// Whether the message is still in reasoning.
    #[serde(default)]
    pub is_thinking: Option<bool>,
    /// The tool event payload, for a tool message.
    #[serde(default)]
    pub tool_event: Option<serde_json::Value>,
    /// Usage stats of an assistant message.
    #[serde(default)]
    pub stats: Option<RequestStats>,
    /// The display translation of `content`, when the reply was not in the
    /// communication language; `content` stays the original.
    #[serde(default)]
    pub translation: Option<ApiTranslation>,
}

/// A reply translated into the communication language for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiTranslation {
    /// The translated reply.
    pub text: String,
    /// The communication language's ISO 639-3 code.
    pub language: String,
    /// The communication language's display name.
    pub language_name: String,
}

/// A session's list row over the wire.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiSession {
    /// Session id.
    pub id: String,
    /// Session title.
    pub title: String,
    /// When the session last changed.
    pub last_modified: String,
    /// Content preview.
    pub preview: String,
    /// Profile the session belongs to.
    pub profile_id: Option<String>,
    /// Model bound to the session.
    pub model: String,
    /// Per-session reasoning effort override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Runtime summary for refresh-safe UI state.
    pub runtime: SessionRuntimeResponse,
    /// Repository the session is bound to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `GET /api/sessions/{id}` only: the `?after=` cursor for
    /// `/api/sessions/{id}/events`, read before the session itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The cloud machine the session lives on; absent for this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SessionLocation>,
}

impl ApiSession {
    /// Build an [`ApiSession`] from a [`SessionSummary`] with a resolved model.
    #[must_use]
    pub fn from_summary(
        summary: may_protocol::session_summary::SessionSummary,
        model: String,
    ) -> Self {
        let last_modified = summary
            .updated_at
            .map_or_else(|| jiff::Timestamp::now().to_string(), |t| t.to_string());
        let preview = if summary.content_preview.is_empty() {
            "New conversation started".to_string()
        } else {
            summary.content_preview
        };
        Self {
            id: summary.id.as_str().to_string(),
            title: summary.title.unwrap_or_else(|| "New Chat".to_string()),
            last_modified,
            preview,
            profile_id: summary.profile_id,
            model,
            reasoning_effort: summary.reasoning_effort.clone(),
            runtime: summary.runtime.clone().into(),
            repo: summary.repo.clone(),
            cursor: None,
            location: None,
        }
    }

    /// Build a fresh [`ApiSession`] for a newly created session.
    #[must_use]
    pub fn new_empty(id: String, model: String, profile_id: Option<String>) -> Self {
        Self {
            id,
            title: "New Chat".to_string(),
            last_modified: jiff::Timestamp::now().to_string(),
            preview: "New conversation started".to_string(),
            profile_id,
            model,
            reasoning_effort: None,
            runtime: may_protocol::session_runtime::SessionRuntimeSummary::default().into(),
            repo: None,
            cursor: None,
            location: None,
        }
    }
}

/// A session name the daemon issued.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct IssuedSessionName {
    /// A name no session was ever named by.
    pub name: String,
}

/// `POST /api/sessions` body.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    /// Session name `/api/session-names` issued the caller; a fresh one is
    /// issued otherwise.
    #[serde(default)]
    pub id: Option<String>,
    /// Model to bind; absent uses the profile's default.
    #[serde(default)]
    pub model: Option<String>,
    /// Profile the session belongs to; absent uses the default.
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Host directory the session works in (the TUI's launch directory).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Repository to bind: `owner/repo`, a local path, or absent for a
    /// repo-less chat.
    #[serde(default)]
    pub repo: Option<String>,
    /// Runs the session entirely on a cloud machine of the profile's
    /// provider, on the machine type named; absent for a session on this
    /// machine.
    #[serde(default)]
    pub cloud: Option<CloudSessionRequest>,
}

/// The cloud machine a new session runs on.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloudSessionRequest {
    /// The catalog's machine type name.
    pub machine_type: String,
}

/// Where a session's home environment is, when it is not this machine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionLocation {
    /// The cloud provider.
    pub provider: CloudProviderName,
    /// The provider's name of the machine.
    pub machine: String,
    /// The machine type it was created on.
    pub machine_type: String,
}

/// A cloud provider, as the API names it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CloudProviderName {
    /// GitHub Codespaces.
    Codespaces,
}

/// `POST /api/profiles/{id}/cloud/codespaces/catalog` request body.
#[derive(Debug, Clone, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloudCatalogRequest {
    /// The repository the session binds, `owner/repo`; absent for none.
    #[serde(default)]
    pub repo: Option<String>,
}

/// The machine types a cloud session may start on.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloudCatalogResponse {
    /// The provider's catalog for the repository's codespaces.
    pub machines: Vec<may_protocol::cloud::MachineType>,
}

/// One step of bringing a cloud session's machine to serve it, pushed as
/// the `cloud_session` event kind for the clients' loading indicator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloudSessionEvent {
    /// The session.
    pub session_id: String,
    /// What is happening.
    pub phase: CloudSessionPhase,
    /// The machine's state as the provider reported it, while it settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_state: Option<may_protocol::cloud::MachineState>,
    /// Why it failed, for [`CloudSessionPhase::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The steps of bringing a cloud session's machine to serve it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CloudSessionPhase {
    /// The machine is being created.
    Creating,
    /// The machine is starting or settling.
    Starting,
    /// Checking which May the machine has.
    Checking,
    /// Downloading the May the machine needs.
    Downloading,
    /// Installing May on the machine.
    Installing,
    /// Cloning the session's repository onto the machine.
    Cloning,
    /// Starting the machine's May daemon.
    StartingDaemon,
    /// Connecting to the machine's May daemon.
    Connecting,
    /// The machine serves the session.
    Ready,
    /// Bringing the machine up failed.
    Failed,
}

impl From<may_protocol::cloud::BootstrapStep> for CloudSessionPhase {
    fn from(step: may_protocol::cloud::BootstrapStep) -> Self {
        use may_protocol::cloud::BootstrapStep;
        match step {
            BootstrapStep::Checking => Self::Checking,
            BootstrapStep::Downloading => Self::Downloading,
            BootstrapStep::Installing => Self::Installing,
            BootstrapStep::Cloning => Self::Cloning,
            BootstrapStep::StartingDaemon => Self::StartingDaemon,
        }
    }
}

/// Request to truncate a session's history at a stored row.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RewindRequest {
    /// Transcript entry id (a `user` entry) where the cut starts; hidden
    /// context directly before it is cut too.
    pub entry_id: String,
}

/// `PATCH /api/sessions/{id}` body: every field optional, all validated
/// before anything is applied.
#[derive(Debug, Default, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionPatch {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New model binding.
    #[serde(default)]
    pub model: Option<String>,
    /// Tri-state like `reasoning_effort` below: absent leaves, `null` clears.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Tri-state reasoning-effort patch.
    #[serde(default, deserialize_with = "deserialize_present_patch")]
    pub reasoning_effort: Option<NullableStringPatch>,
    /// Tri-state repo binding patch.
    #[serde(default, deserialize_with = "deserialize_present_patch")]
    pub repo: Option<NullableStringPatch>,
    /// One-shot/non-interactive flag.
    #[serde(default)]
    pub non_interactive: Option<bool>,
    /// Pin the session in its profile.
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Archive the session.
    #[serde(default)]
    pub archived: Option<bool>,
    /// Tri-state folder patch: absent leaves, `null` moves out of folders.
    #[serde(default, deserialize_with = "deserialize_present_patch")]
    pub folder_id: Option<NullableStringPatch>,
    /// Toggle unattended approvals.
    #[serde(default)]
    pub yolo: Option<bool>,
    /// One-way memory disable toggle (`true` disables memory for the rest of the session).
    #[serde(default)]
    pub memory_off: Option<bool>,
    /// New plan-mode state.
    #[serde(default)]
    pub plan_state: Option<SessionPlanState>,
}

/// Deserialize a field that is present in the body, keeping an explicit `null`
/// as [`NullableStringPatch::Null`]; serde's `Option` impl would otherwise
/// collapse it into `None`, which means "absent".
fn deserialize_present_patch<'de, D>(
    deserializer: D,
) -> Result<Option<NullableStringPatch>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <NullableStringPatch as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// Tri-state payload for a nullable request field that is present in the JSON
/// body: either an explicit `null` or a value.
#[derive(Debug, Clone, serde::Deserialize, skyzen::ToSchema)]
#[serde(untagged)]
pub enum NullableStringPatch {
    /// Explicit `null` — clear the stored value.
    Null,
    /// A concrete value — set the stored value.
    Set(String),
}

/// `POST /api/chat` body.
#[derive(Debug, Clone, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    /// The session to run.
    pub session_id: String,
    /// The message text.
    pub content: String,
    /// Attachments to the message.
    #[serde(default)]
    pub attachments: Vec<ApiAttachment>,
    /// Structured hidden context fragment for the next turn.
    /// XML-like tags are allowed, but strict XML validity is not required.
    #[serde(default)]
    pub hidden_context_xml: Option<XmlBlock>,
    /// How the message is delivered.
    #[serde(default)]
    pub delivery_mode: PendingUserMessageMode,
    /// Queue entry claimed for this run; committed with the user append.
    #[serde(default)]
    pub pending_entry_id: Option<i64>,
}

/// `POST /api/sessions/{id}/turns` body: one message, delivered the way
/// `mode` says.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnRequest {
    /// The message text.
    pub content: String,
    /// Attachments to the message.
    #[serde(default)]
    pub attachments: Vec<ApiAttachment>,
    /// Structured hidden context fragment for the next turn.
    #[serde(default)]
    pub hidden_context_xml: Option<XmlBlock>,
    /// `start` runs the message now (the default); `steer` interrupts the
    /// running turn; `queue` runs it after.
    #[serde(default)]
    pub mode: TurnMode,
    /// Queue entry claimed for this run; committed with the user append.
    #[serde(default)]
    pub pending_entry_id: Option<i64>,
}

/// How `POST /api/sessions/{id}/turns` delivers its message.
#[derive(Debug, Default, Clone, Copy, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    /// Run the message now.
    #[default]
    Start,
    /// Interrupt the running turn with it.
    Steer,
    /// Run it after the current turn.
    Queue,
}

/// `GET /api/sessions/{id}/queue` response: the session's pending user
/// messages, immediate first then queued, oldest first.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionQueueResponse {
    /// The pending messages.
    #[serde(default)]
    pub entries: Vec<PendingUserMessage>,
}

/// `POST /api/sessions/{id}/bash` payload.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BashRequest {
    /// The `!` command line to run on the user's behalf.
    pub command: String,
}

/// `POST /api/sessions/{id}/bash` response — the masked run outcome.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BashResponse {
    /// The command run.
    pub command: String,
    /// Its standard output.
    pub stdout: String,
    /// Its standard error.
    pub stderr: String,
    /// Its exit code.
    pub exit_code: i32,
    /// Whether the output masked a stored secret.
    pub had_secret: bool,
    /// Why the run itself failed, when it did.
    pub error: Option<String>,
}

/// One message of the assembled model context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextDebugMessage {
    /// The sender's role.
    pub role: String,
    /// Message text.
    pub content: String,
    /// The message's attachment names.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// The tool call the message answers, when it is one.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Names of the tool calls the message carries.
    #[serde(default)]
    pub tool_call_names: Vec<String>,
}

/// The session's assembled context, for `/context`.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextDebugResponse {
    /// The session dumped.
    pub session_id: String,
    /// When the dump was taken.
    pub timestamp: String,
    /// The model bound to the session.
    pub model: String,
    /// The provider serving the model.
    pub provider_id: String,
    /// The assembled system prompt.
    pub prompt: String,
    /// Attachments the prompt carries.
    #[serde(default)]
    pub prompt_attachments: Vec<String>,
    /// The messages sent with the request, in order.
    pub assembled_messages: Vec<ContextDebugMessage>,
    /// The context window snapshot, when one was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_snapshot: Option<SessionContextWindowSnapshotResponse>,
}

/// The context window's state when the dump was taken.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextWindowSnapshotResponse {
    /// The window's phase.
    #[schema(value_type = String)]
    pub phase: aither_agent::ContextWindowPhase,
    /// The window's measurements.
    pub metrics: SessionContextWindowMetricsResponse,
    /// The messages the request assembled.
    pub assembled_messages: Vec<ContextDebugMessage>,
}

/// The context window's measurements.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextWindowMetricsResponse {
    /// Share of the window in use.
    pub usage_fraction: f32,
    /// The model's context window in tokens.
    pub context_window: u32,
    /// Estimated tokens the request carries.
    pub estimated_tokens: usize,
    /// System blocks in the request.
    pub system_block_count: usize,
    /// Recent messages in the request.
    pub recent_message_count: usize,
    /// System messages among the recent messages.
    pub recent_system_message_count: usize,
    /// Whether the request carries a handoff.
    pub has_handoff: bool,
}

impl From<aither_agent::ContextWindowMetrics> for SessionContextWindowMetricsResponse {
    fn from(value: aither_agent::ContextWindowMetrics) -> Self {
        Self {
            usage_fraction: value.usage_fraction,
            context_window: value.context_window,
            estimated_tokens: value.estimated_tokens,
            system_block_count: value.system_block_count,
            recent_message_count: value.recent_message_count,
            recent_system_message_count: value.recent_system_message_count,
            has_handoff: value.has_handoff,
        }
    }
}

/// One selectable model's description.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfoItem {
    /// The model's id as configured.
    pub id: String,
    /// The provider's name of the model.
    pub model_id: String,
    /// The provider serving it.
    pub provider_id: String,
    /// Display name.
    pub nickname: String,
    /// The model's context window.
    pub context_window: Option<u32>,
    /// What the model can do.
    pub abilities: Vec<String>,
    /// The reasoning-effort levels the model accepts, lowest first; empty
    /// when the model has no effort control.
    pub effort_ladder: Vec<String>,
    /// Retired by its provider or superseded by a newer generation of its
    /// line; pickers hide it by default.
    pub outdated: bool,
}

/// The selectable models.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelsResponse {
    /// Model ids.
    pub models: Vec<String>,
    /// Each model's description.
    #[serde(default)]
    pub details: Vec<ModelInfoItem>,
}

/// One provider's selectable models.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelsItem {
    /// The provider.
    pub provider_id: String,
    /// Model ids.
    pub models: Vec<String>,
    /// Each model's description.
    #[serde(default)]
    pub details: Vec<ModelInfoItem>,
}

/// The providers' selectable models.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelsResponse {
    /// Each provider with its models.
    pub providers: Vec<ProviderModelsItem>,
}

/// A pending permission request.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionPollItem {
    /// The request's id.
    pub id: String,
    /// What is asked.
    #[serde(flatten)]
    pub request: may_protocol::permission::PermissionRequest,
}

/// A pending workspace request.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePollItem {
    /// The request's id.
    pub id: String,
    /// The directory asked for.
    pub path: String,
    /// Why the agent wants it.
    pub reason: String,
    /// The session asking, when the request carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// The session's browser interaction state.
#[derive(Debug, Clone, Default, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractionResponse {
    /// Whether a browser takeover is in progress.
    pub takeover_active: bool,
}

/// The session's terminal interaction state.
#[derive(Debug, Clone, Default, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInteractionResponse {
    /// The foreground process id, while one runs.
    pub pid: Option<u32>,
    /// The file its output goes to.
    pub output_file: Option<String>,
    /// The head of its output.
    pub output_preview: Option<String>,
    /// The notice pending for its input.
    pub input_notice: Option<String>,
}

/// The session's live interactions.
#[derive(Debug, Clone, Default, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionResponse {
    /// The browser interaction state.
    pub browser: BrowserInteractionResponse,
    /// The terminal interaction state.
    pub terminal: TerminalInteractionResponse,
}

impl From<may_protocol::session_runtime::SessionRuntimeSummary> for SessionRuntimeResponse {
    fn from(summary: may_protocol::session_runtime::SessionRuntimeSummary) -> Self {
        Self {
            state: summary.state,
            plan_state: summary.plan_state,
            yolo: summary.yolo,
            memory_off: summary.memory_off,
            interactions: SessionInteractionResponse {
                browser: BrowserInteractionResponse {
                    takeover_active: summary.interactions.browser.takeover_active,
                },
                terminal: TerminalInteractionResponse {
                    pid: summary.interactions.terminal.pid,
                    output_file: summary.interactions.terminal.output_file,
                    output_preview: summary.interactions.terminal.output_preview,
                    input_notice: summary.interactions.terminal.input_notice,
                },
            },
            queued_count: summary.queued_count,
            pending_immediate: summary.pending_immediate,
            queued_messages: summary.queued_messages,
            waiting_permission: summary
                .waiting_permission
                .map(|request| PermissionPollItem {
                    id: request.id,
                    request: may_protocol::permission::PermissionRequest {
                        mode: request.request.mode,
                        script: request.request.script,
                        session_id: request.request.session_id,
                        agent_id: request.request.agent_id,
                        reading: request.request.reading,
                        interpreter: request.request.interpreter,
                    },
                }),
            waiting_workspace: summary.waiting_workspace.map(|request| WorkspacePollItem {
                id: request.id,
                path: request.request.path,
                reason: request.request.reason,
                session_id: request.request.session_id,
            }),
            waiting_ask_user: summary.waiting_ask_user,
            open_questions: summary.open_questions,
        }
    }
}

/// `POST /api/interactions/{id}` body: which pending interaction the id
/// names and its answer.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InteractionRespondRequest {
    /// A permission request's answer.
    Permission {
        /// Whether the request is approved.
        approved: bool,
    },
    /// A workspace request's answer.
    Workspace {
        /// Whether the directory is granted.
        approved: bool,
    },
    /// An ask-user question's answers.
    #[serde(rename_all = "camelCase")]
    AskUser {
        /// The answers to its questions.
        answers: Vec<crate::daemon::types::UiQuestionAnswer>,
    },
}

/// The configuration as the UI edits it.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiConfigResponse {
    /// The configured SSH servers.
    pub ssh_servers: Vec<SshServerConfig>,
    /// The connected ACP agents.
    pub acp_agents: Vec<crate::client::acp::ConnectedAcpAgent>,
    /// The turn's iteration cap.
    pub max_iterations: usize,
    /// The UI font.
    pub ui_font: String,
    /// The UI theme.
    pub ui_theme: String,
    /// IANA name of the user's timezone (`[time] timezone`, else the host's).
    pub timezone: String,
    /// The full configuration.
    pub full_config: UiFullConfig,
}

/// Why the default profile's sessions could not be listed.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapProfileSessionsError {
    /// The machine-readable error code, when one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ApiErrorCode>,
    /// The human-readable error message.
    pub message: String,
}

/// What a client bootstraps from.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiBootstrapResponse {
    /// All sessions.
    pub sessions: Vec<ApiSession>,
    /// All profiles.
    pub profiles: Vec<may_protocol::profile::ProfileInfo>,
    /// The profile new sessions land on.
    pub default_profile_id: String,
    /// The configuration as the UI edits it.
    pub config: UiConfigResponse,
    /// The default profile's organized sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile_sessions: Option<may_protocol::profile::ProfileSessions>,
    /// Why the default profile's sessions could not be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile_sessions_error: Option<BootstrapProfileSessionsError>,
    /// The default profile's skills.
    #[serde(default)]
    pub default_profile_skills: Vec<SkillInfoDto>,
}

/// A session's runtime state over the wire.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeResponse {
    /// Whether the session is idle or running.
    pub state: SessionRuntimeState,
    /// The session's plan-mode state.
    pub plan_state: SessionPlanState,
    /// Whether unattended approvals are on.
    pub yolo: bool,
    /// Whether memory is disabled for the session.
    pub memory_off: bool,
    /// The session's live interactions.
    pub interactions: SessionInteractionResponse,
    /// How many messages wait to run.
    pub queued_count: usize,
    /// The immediate message waiting to interrupt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_immediate: Option<PendingUserMessage>,
    /// The queued messages, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<PendingUserMessage>,
    /// The permission request waiting on the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_permission: Option<PermissionPollItem>,
    /// The workspace request waiting on the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_workspace: Option<WorkspacePollItem>,
    /// The blocking ask-user question waiting on the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_ask_user: Option<may_protocol::session_runtime::PersistedAskUserRequest>,
    /// The session's open ask-user questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<may_protocol::session_runtime::PersistedAskUserRequest>,
}

impl Default for SessionRuntimeResponse {
    fn default() -> Self {
        Self {
            state: SessionRuntimeState::Idle,
            plan_state: SessionPlanState::Disabled,
            yolo: false,
            memory_off: false,
            interactions: SessionInteractionResponse::default(),
            queued_count: 0,
            pending_immediate: None,
            queued_messages: Vec::new(),
            waiting_permission: None,
            waiting_workspace: None,
            waiting_ask_user: None,
            open_questions: Vec::new(),
        }
    }
}

/// One session's runtime in the snapshot.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSnapshotItem {
    /// The session the runtime belongs to.
    pub session_id: String,
    /// Its runtime state.
    #[schema(value_type = SessionRuntimeResponse)]
    pub runtime: SnapshotRuntime,
}

/// A session's runtime in a snapshot: this daemon's own, or a cloud
/// session's as its codespace daemon reports it, which has the same shape.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum SnapshotRuntime {
    /// A session this daemon runs.
    Local(Box<SessionRuntimeResponse>),
    /// A cloud session, as its codespace daemon serialized it.
    Codespace(serde_json::Value),
}

/// The `session_runtimes` event payload.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimesEventPayload {
    /// Every session's runtime.
    pub sessions: Vec<SessionRuntimeSnapshotItem>,
}

/// The `profile_sessions` event payload.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSessionsEventPayload {
    /// The profile's organized sessions.
    pub profile_sessions: may_protocol::profile::ProfileSessions,
}

/// One artifact of the session's `artifacts/` directory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactEntry {
    /// Its path relative to the directory.
    pub path: String,
    /// Its file name.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Whether it renders as HTML.
    pub is_html: bool,
    /// Its content fingerprint.
    pub fingerprint: String,
}

/// The session's artifacts.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionArtifactsResponse {
    /// The entries.
    pub artifacts: Vec<ArtifactEntry>,
}

/// The artifact preview's query.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPreviewQuery {
    /// The artifact's path.
    pub path: String,
}

const MAX_ARTIFACT_TYPE_BYTES: usize = 64;

/// A validated artifact type name.
#[derive(Debug, Clone)]
pub struct ArtifactType(String);

impl ArtifactType {
    /// The artifact type's name.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> serde::Deserialize<'de> for ArtifactType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        if value.is_empty() || value.len() > MAX_ARTIFACT_TYPE_BYTES {
            return Err(serde::de::Error::custom(
                "artifact type must contain between 1 and 64 bytes",
            ));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(serde::de::Error::custom(
                "artifact type must contain only lowercase ASCII letters, digits, and hyphens",
            ));
        }
        Ok(Self(value))
    }
}

/// The runtime preview's query.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRuntimePreviewQuery {
    /// The artifact's path.
    pub path: String,
    /// The previewer's type.
    #[schema(value_type = String)]
    pub artifact_type: ArtifactType,
}

/// The Slidev preview's answer.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SlidevPreviewResponse {
    /// Where the preview serves, while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
    /// Problems the previewer reported.
    pub diagnostics: Vec<String>,
}

/// Diagnostics a client reports for an artifact.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDiagnosticsRequest {
    /// The artifact's path.
    pub path: String,
    /// The reporter's type.
    #[schema(value_type = String)]
    pub artifact_type: ArtifactType,
    /// The problems seen.
    pub diagnostics: Vec<String>,
}

/// Whether a repair was scheduled.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDiagnosticsResponse {
    /// Whether a repair was scheduled.
    pub repair_scheduled: bool,
}

/// The request's `If-None-Match` header value.
#[derive(Debug, Clone)]
pub struct IfNoneMatchHeader(pub Option<String>);

impl skyzen::extract::Extractor for IfNoneMatchHeader {
    type Error = std::convert::Infallible;

    fn extract(
        request: &mut skyzen::Request,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send {
        let value = request
            .headers()
            .get(skyzen::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        std::future::ready(Ok(Self(value)))
    }
}

/// Provider-native hosted tool toggles as exposed to the web UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiNativeToolsConfig {
    /// Whether the hosted web search tool is on.
    pub web_search: bool,
    /// Whether the hosted web fetch tool is on.
    pub web_fetch: bool,
}

impl From<&NativeToolsConfig> for UiNativeToolsConfig {
    fn from(value: &NativeToolsConfig) -> Self {
        Self {
            web_search: value.web_search,
            web_fetch: value.web_fetch,
        }
    }
}

impl From<&UiNativeToolsConfig> for NativeToolsConfig {
    fn from(value: &UiNativeToolsConfig) -> Self {
        Self {
            web_search: value.web_search,
            web_fetch: value.web_fetch,
        }
    }
}

/// What a config update does with a stored secret: keep it as stored,
/// replace it with new material, or remove it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum SecretUpdate {
    /// Leave the stored secret untouched.
    Keep,
    /// Replace it with new material.
    Replace(String),
    /// Remove the stored secret.
    Remove,
}

/// One provider's configuration as the UI edits it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiProviderConfig {
    /// The provider's id.
    pub id: String,
    /// The provider's backend kind.
    pub provider_type: ProviderType,
    /// Whether an API key is stored for this provider. The response only
    /// reports this, never key material.
    pub has_api_key: bool,
    /// What to do with the stored key on save: keep it, replace it with new
    /// material, or remove it. Never serialized back out.
    #[serde(default, skip_serializing)]
    pub api_key: Option<SecretUpdate>,
    /// The endpoint the provider is reached at.
    pub base_url: Option<String>,
    /// The provider's display name.
    pub display_name: Option<String>,
    /// Whether the provider is enabled.
    pub enabled: bool,
    /// The provider's hosted tool toggles.
    pub native_tools: UiNativeToolsConfig,
    /// Default cache lifetime of the main conversation: a duration such as
    /// `1h`, or `permanent`.
    #[serde(default)]
    #[schema(value_type = String)]
    pub cache_lifetime: may_protocol::cache_lifetime::CacheLifetime,
    /// Wire API a `custom` provider speaks; must round-trip the config save or
    /// `api = "responses"` is silently dropped on every UI write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ProviderApi>,
}

/// The model tiers' selections.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiModelsConfig {
    /// Models of the advanced tier.
    pub advanced: Vec<String>,
    /// Models of the balanced tier.
    pub balanced: Vec<String>,
    /// Models of the fast tier.
    pub fast: Vec<String>,
    /// Models that generate images.
    pub image_generation: Vec<String>,
    /// Model compaction runs on instead of the session model.
    #[serde(default)]
    pub compaction_model: Option<String>,
}

/// The user's preferences.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiPreferencesConfig {
    /// Whether changing the model asks first.
    pub warn_on_model_change: bool,
    /// Whether token usage is shown.
    pub show_token_usage: bool,
    /// Whether decode speed is shown.
    pub show_decode_speed: bool,
    /// The network policy mode.
    #[schema(value_type = String)]
    pub network_policy_mode: NetworkPolicyMode,
    /// Hosts the policy allows.
    pub network_policy_hosts: Vec<String>,
}

/// The UI's appearance settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiAppearanceConfig {
    /// The UI font.
    pub font: String,
    /// The UI theme.
    pub theme: String,
}

/// The managed browser's settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiBrowserConfig {
    /// Whether browser automation is enabled.
    pub enabled: bool,
    /// The browser backend.
    pub backend: BrowserBackendKind,
    /// The port the browser's CDP endpoint binds.
    pub port: u16,
    /// An external CDP endpoint to drive instead.
    pub remote_cdp_endpoint: Option<String>,
    /// JPEG quality of the screencast frames.
    pub screencast_jpeg_quality: u32,
    /// The video encoder's initial bitrate.
    pub encoder_initial_bitrate_kbps: u32,
    /// The screencast's maximum width.
    pub screencast_max_width: u32,
    /// The screencast's maximum height.
    pub screencast_max_height: u32,
    /// Every nth frame is sent.
    pub every_nth_frame: u32,
    /// Whether the browser hides its automation.
    pub stealth_enabled: bool,
    /// The system profile the browser runs with, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_profile: Option<UiSystemProfileConfig>,
}

/// The system profile the browser runs with.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSystemProfileConfig {
    /// The profile's directory.
    pub path: String,
    /// Whether the profile is copied before first use.
    pub copy_on_first_use: bool,
}

/// The remote access settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiRemoteAccessConfig {
    /// Whether remote access is enabled.
    pub enabled: bool,
    /// The `tailscale` binary to run.
    pub tailscale_binary: String,
    /// The HTTPS port remote access serves.
    pub https_port: u16,
    /// Whether the local environment is reachable remotely.
    pub allow_local_environment: bool,
    /// Tailscale logins accepted for remote access besides the login owning
    /// this node, which is always accepted.
    pub allowed_logins: Vec<String>,
    /// Whether Tailscale Funnel exposes the webhook routes to the internet.
    #[serde(default)]
    pub funnel: bool,
    /// Funnel HTTPS port: 443, 8443 or 10000.
    #[serde(default = "default_funnel_port")]
    pub funnel_port: u16,
    /// Largest webhook delivery body in bytes; larger ones get 413.
    #[serde(default = "may_config::config::default_webhook_max_body_bytes")]
    pub webhook_max_body_bytes: u64,
}

const fn default_funnel_port() -> u16 {
    443
}

impl From<&RemoteAccessConfig> for UiRemoteAccessConfig {
    fn from(value: &RemoteAccessConfig) -> Self {
        Self {
            enabled: value.enabled,
            tailscale_binary: value.tailscale_binary.clone(),
            https_port: value.https_port,
            allow_local_environment: value.allow_local_environment,
            allowed_logins: value.allowed_logins.clone(),
            funnel: value.funnel,
            funnel_port: value.funnel_port,
            webhook_max_body_bytes: value.webhook_max_body_bytes,
        }
    }
}

/// The screen stream's settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiStreamConfig {
    /// The codec the stream encodes with.
    #[schema(value_type = String)]
    pub codec: may_config::config::StreamCodec,
}

/// The sandbox's settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSandboxConfig {
    /// The priority commands run at.
    #[schema(value_type = String)]
    pub command_priority: CommandPriority,
    /// Jobserver tokens shared by every terminal; `None` is the host's
    /// available parallelism.
    #[serde(default)]
    pub job_pool_size: Option<usize>,
}

/// The full configuration as the UI edits it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiFullConfig {
    /// The configured providers.
    pub providers: Vec<UiProviderConfig>,
    /// The model tiers' selections.
    pub models: UiModelsConfig,
    /// The user's preferences.
    pub preferences: UiPreferencesConfig,
    /// The UI's appearance settings.
    pub appearance: UiAppearanceConfig,
    /// The model last selected.
    pub last_model: Option<String>,
    /// The turn's iteration cap.
    pub max_iterations: usize,
    /// The managed browser's settings.
    pub browser: UiBrowserConfig,
    /// The remote access settings.
    pub remote_access: UiRemoteAccessConfig,
    /// The sandbox's settings.
    pub sandbox: UiSandboxConfig,
    /// The screen stream's settings.
    pub stream: UiStreamConfig,
    /// A save without the section leaves the setting as it is.
    #[serde(default)]
    pub suggestions: Option<UiSuggestionsConfig>,
    /// A save without the section leaves the setting as it is.
    #[serde(default)]
    pub recap: Option<UiRecapConfig>,
    /// A save without the section leaves the setting as it is.
    #[serde(default)]
    pub sessions: Option<UiSessionsConfig>,
}

/// Session-listing settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSessionsConfig {
    /// How far back "recent" sessions reach.
    pub recent_hours: u64,
}

/// Reply-suggestion settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSuggestionsConfig {
    /// Whether suggestions are generated.
    pub enabled: bool,
}

/// Recap settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiRecapConfig {
    /// Whether recaps generate automatically.
    pub auto: bool,
    /// How long a session idles before a recap.
    pub idle_minutes: u64,
}

/// An SSH server's registration.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SshServerRequest {
    /// The server's name.
    pub name: String,
    /// The SSH target string.
    pub target: String,
}

/// An MCP server's registration.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpCreateRequest {
    /// A local command serving MCP.
    Process {
        /// The server's name.
        name: String,
        /// The command run.
        command: String,
        /// Its arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Its environment.
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    },
    /// A remote MCP endpoint.
    Url {
        /// The server's name.
        name: String,
        /// The endpoint's URL.
        url: String,
    },
}

/// A skill install request.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillInstallRequest {
    /// The repository the skill installs from.
    pub repo: String,
}

/// Whether Copilot is signed in.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStatusResponse {
    /// Whether Copilot is signed in.
    pub authenticated: bool,
}

/// The device-flow codes to sign in with.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStartResponse {
    /// The flow's device code.
    pub device_code: String,
    /// The code the user enters.
    pub user_code: String,
    /// Where the user enters it.
    pub verification_uri: String,
    /// How often to poll, in seconds.
    pub interval: u64,
}

/// A `copilot_auth` event.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopilotAuthEvent {
    /// Where the sign-in is.
    pub status: CopilotAuthStatus,
    /// What happened, when the status carries one.
    pub message: Option<String>,
}

/// Where a Copilot sign-in is.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CopilotAuthStatus {
    /// Waiting on the user.
    Pending,
    /// Signed in.
    Success,
    /// The device code expired.
    Expired,
    /// The user refused.
    Denied,
    /// The flow failed.
    Error,
}

/// One installed skill.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillInfoDto {
    /// The skill's name.
    pub name: String,
    /// What the skill does.
    pub description: String,
    /// Its directory name.
    pub dir_name: String,
    /// The skill's origin as its canonical binding (`github:owner/repo`,
    /// `gitlab:<host>/<path>`, `bundled`, `local:<path>`).
    pub source: String,
}

/// The model to remember as last selected.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LastModelRequest {
    /// The model id.
    pub model: String,
}

/// Body of `PUT /api/config/update-channel`: the channel `may update` and the
/// daemon follow.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateChannelRequest {
    /// The update channel.
    pub channel: may_config::update::Channel,
}

/// Body of `PUT /api/config/job-pool`: the machine-wide jobserver pool size,
/// `None` for the host's available parallelism.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobPoolRequest {
    /// The pool size.
    pub size: Option<usize>,
}

/// A memory to store.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySaveRequest {
    /// The memory text.
    pub content: String,
    /// Whether the user asked for the save.
    #[serde(default)]
    pub explicit_user_request: bool,
}

/// A URL to map to its local form.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalUrlMapRequest {
    /// The URL to map.
    pub url: String,
}

/// The mapped URL.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalUrlMapResponse {
    /// The local form.
    pub mapped_url: String,
}

#[cfg(test)]
mod tests {
    use super::{NullableStringPatch, SessionPatch};

    fn reasoning_effort(body: &str) -> Option<NullableStringPatch> {
        serde_json::from_str::<SessionPatch>(body)
            .expect("patch parses")
            .reasoning_effort
    }

    #[test]
    fn update_session_reasoning_effort_keeps_absent_null_and_value_apart() {
        assert!(reasoning_effort("{}").is_none());
        assert!(matches!(
            reasoning_effort(r#"{"reasoningEffort":null}"#),
            Some(NullableStringPatch::Null)
        ));
        assert!(matches!(
            reasoning_effort(r#"{"reasoningEffort":"high"}"#),
            Some(NullableStringPatch::Set(value)) if value == "high"
        ));
    }
}

/// A tunnel's registration.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct TunnelRegisterRequest {
    /// The session the tunnel belongs to.
    pub session_id: String,
    /// The public URL the tunnel exposes.
    pub url: String,
}

/// The registered tunnel.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct TunnelRegisterResponse {
    /// The tunnel's id.
    pub id: String,
    /// The public URL the tunnel exposes.
    pub url: String,
    /// The path it forwards.
    pub path: String,
    /// The local URL it forwards to.
    pub local_url: String,
}

/// One registered tunnel.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct TunnelInfoResponse {
    /// The tunnel's id.
    pub id: String,
    /// What it forwards to.
    pub target: String,
}

// ── Forge repository picker ────────────────────────────────

/// One forge repository a new chat can be bound to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiGithubRepo {
    /// `owner/repo` on `github.com`, `<host>/<path>` elsewhere.
    pub full_name: String,
    /// Whether a local checkout of it was used recently.
    pub recent: bool,
    /// The canonical session binding (`github:owner/repo`,
    /// `gitlab:<host>/<path>`).
    #[serde(default)]
    pub binding: String,
    /// The forge kind: `github` or `gitlab`.
    #[serde(default)]
    pub forge: String,
    /// The forge host (`github.com`, `gitlab.com`, a self-hosted name).
    #[serde(default)]
    pub host: String,
}

/// The picker's list: recently used repositories first.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
pub struct ApiGithubRepos {
    /// The repositories, most recent first.
    pub repos: Vec<ApiGithubRepo>,
}

/// One answer to an ask-user question.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiQuestionAnswer {
    /// The question the answer belongs to.
    pub section: String,
    /// The answer given.
    pub answer: UiAnswerInput,
}

/// What a client sends for one question. For a secret question `custom`
/// carries the secret value and `declined` refuses to give it; neither value
/// nor request body is ever printed.
#[derive(serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAnswerInput {
    /// The option(s) picked.
    Selected {
        /// The picked options' values.
        values: Vec<String>,
    },
    /// A custom answer's text.
    Custom {
        /// The answer's text.
        text: String,
    },
    /// The question was declined.
    Declined,
}

impl std::fmt::Debug for UiAnswerInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Selected { .. } => "UiAnswerInput::Selected",
            Self::Custom { .. } => "UiAnswerInput::Custom",
            Self::Declined => "UiAnswerInput::Declined",
        })
    }
}

/// Body of `POST /api/sessions/{id}/compact`.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactRequest {
    /// What the summary should keep in focus.
    #[serde(default)]
    pub focus: Option<String>,
}

/// Answer of `POST /api/sessions/{id}/compact`.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactResponse {
    /// Whether anything was compacted.
    pub compacted: bool,
    /// Messages folded into the summary.
    pub messages_compacted: usize,
    /// Messages kept verbatim.
    pub messages_remaining: usize,
    /// The summary.
    pub summary: Option<String>,
}

/// Answer of `POST /api/sessions/{id}/context-dump`.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextDumpResponse {
    /// Where the dump was written.
    pub path: String,
    /// Messages in the dump.
    pub message_count: usize,
}

/// One message of a `/btw` request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BtwMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// Message text.
    pub content: String,
}

/// Body of `POST /api/sessions/{id}/btw`.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BtwRequest {
    /// The side question with the context it is asked in, oldest first.
    pub messages: Vec<BtwMessage>,
}

/// Answer of `POST /api/sessions/{id}/btw`.
#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BtwResponse {
    /// The answer.
    pub answer: String,
}

impl TryFrom<UiAnswerInput> for may_protocol::ask_user::AnswerValue {
    type Error = UiAnswerInputError;

    fn try_from(value: UiAnswerInput) -> Result<Self, Self::Error> {
        match value {
            UiAnswerInput::Selected { values } => {
                if values.is_empty() {
                    return Err(UiAnswerInputError::MissingSelection);
                }
                if values.iter().any(|value| value.trim().is_empty()) {
                    return Err(UiAnswerInputError::BlankSelectedOption);
                }
                Ok(Self::Selected(values))
            }
            UiAnswerInput::Custom { text } => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Err(UiAnswerInputError::BlankCustom);
                }
                Ok(Self::Custom(trimmed.to_string()))
            }
            UiAnswerInput::Declined => Err(UiAnswerInputError::DeclinedPlainQuestion),
        }
    }
}

/// Why an ask-user answer was refused.
#[derive(Debug, thiserror::Error)]
pub enum UiAnswerInputError {
    /// A selected answer picked no option.
    #[error("ask-user selected answer requires at least one option")]
    MissingSelection,
    /// A selected option was blank.
    #[error("ask-user selected answers must not contain empty options")]
    BlankSelectedOption,
    /// A custom answer was empty.
    #[error("ask-user custom answer must not be empty")]
    BlankCustom,
    /// A non-secret question was declined.
    #[error("only a secret question can be declined")]
    DeclinedPlainQuestion,
    /// A secret question was answered with a selection.
    #[error("a secret question takes the value as text, not a selection")]
    SelectedSecret,
}
