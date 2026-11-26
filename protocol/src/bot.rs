//! Wire and domain types for May Bots.
//!
//! May Bots are always-on non-bash-first bridge agents with durable inboxes,
//! approval triage, delegated workers, schedules, and git-backed hierarchical memory.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Formats a Unix-millisecond UTC timestamp into the canonical bot message stamp.
///
/// - Whole-second aligned (`ts_ms % 1000 == 0`): `YYYY-MM-DDTHH:MM:SSZ`
/// - Sub-second (`ts_ms % 1000 != 0`): `YYYY-MM-DDTHH:MM:SS.sssZ`
///
/// # Panics
/// Panics if `ts_ms` falls outside `jiff::Timestamp`'s representable range.
#[must_use]
pub fn format_monotonic_utc(ts_ms: i64) -> String {
    let ts = jiff::Timestamp::from_millisecond(ts_ms)
        .expect("monotonic bot timestamp must lie in jiff::Timestamp range");
    let whole_secs = ts.as_second();
    let millis = ts.subsec_nanosecond() / 1_000_000;
    let base = jiff::Timestamp::from_second(whole_secs)
        .expect("whole-second timestamp must lie in range")
        .strftime("%Y-%m-%dT%H:%M:%SZ");
    if millis == 0 {
        base.to_string()
    } else {
        let base_str = base.to_string();
        let prefix = base_str.strip_suffix('Z').unwrap_or(&base_str);
        format!("{prefix}.{millis:03}Z")
    }
}

/// Computes the next strictly monotonic per-bot UTC millisecond key.
///
/// Given wall clock `now` and the bot's previous `last_ts_ms`:
/// - Aligns to whole seconds (`now.as_second() * 1000`) whenever strictly greater than `last_ts_ms`.
/// - Advances by `+1ms` (`last_ts_ms + 1`) on same-second bursts or clock step-backs.
#[must_use]
pub fn next_monotonic_ts_ms(now: jiff::Timestamp, last_ts_ms: i64) -> i64 {
    let now_sec_ms = now.as_second().saturating_mul(1000);
    if now_sec_ms > last_ts_ms {
        now_sec_ms
    } else {
        last_ts_ms.saturating_add(1)
    }
}

/// Parses an RFC 3339 / ISO 8601 UTC timestamp string into Unix milliseconds.
///
/// # Errors
/// Returns an error message when `value` is not a valid timestamp.
pub fn parse_utc_to_ms(value: &str) -> Result<i64, String> {
    let trimmed = value.trim();
    let ts = trimmed
        .parse::<jiff::Timestamp>()
        .map_err(|error| format!("invalid UTC timestamp \"{trimmed}\": {error}"))?;
    Ok(ts.as_millisecond())
}

/// Policy for how a bot handles a permission gate from a delegated worker or
/// supervised session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GatePolicy {
    /// Automatically approve without waking the bot or interrupting the user.
    AutoApprove,
    /// Park under `ApprovalPause`, route to the bot's inbox for `decide_approval`,
    /// and escalate to the user only when the bot calls `Escalate`.
    #[default]
    BotTriage,
    /// Escalate immediately to the user as a `UserWait` approval card.
    AlwaysEscalate,
}

/// Live-configurable per-bot approval policy across all four permission gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotApprovalPolicy {
    /// Policy for `unsafe` (unsandboxed terminal script) requests.
    #[serde(default)]
    pub unsafe_cmd: GatePolicy,
    /// Policy for SSH connection requests.
    #[serde(default)]
    pub ssh: GatePolicy,
    /// Policy for external/gated publication (issues, PRs, comments).
    #[serde(default)]
    pub publish: GatePolicy,
    /// Policy for `request_workspace` host directory write grants.
    #[serde(default)]
    pub workspace: GatePolicy,
}

impl Default for BotApprovalPolicy {
    fn default() -> Self {
        Self {
            unsafe_cmd: GatePolicy::BotTriage,
            ssh: GatePolicy::BotTriage,
            publish: GatePolicy::BotTriage,
            workspace: GatePolicy::BotTriage,
        }
    }
}

impl BotApprovalPolicy {
    /// Returns the configured [`GatePolicy`] for `gate`.
    #[must_use]
    pub const fn for_gate(&self, gate: BotApprovalGate) -> GatePolicy {
        match gate {
            BotApprovalGate::Unsafe => self.unsafe_cmd,
            BotApprovalGate::Ssh => self.ssh,
            BotApprovalGate::Publish => self.publish,
            BotApprovalGate::Workspace => self.workspace,
        }
    }
}

/// Live execution state of a May Bot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BotRunState {
    /// Enabled and waiting for inbox events.
    #[default]
    Idle,
    /// Actively processing inbox events.
    Running,
    /// Disabled / paused by user configuration.
    Disabled,
    /// Last run ended with an error.
    Error,
}

impl BotRunState {
    /// Stable string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Disabled => "disabled",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for BotRunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of event queued into a bot's durable `bot_inbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BotInboxKind {
    /// Direct message from the human user.
    UserMessage,
    /// Message from another session, subagent, or bot via `AgentDirectory`.
    AgentMessage,
    /// A delegated worker finished successfully.
    DelegateCompleted,
    /// A delegated worker failed or was stopped.
    DelegateFailed,
    /// A supervised session emitted a lifecycle event or completion.
    SessionEvent,
    /// A delegated worker or supervised session requested permission triage.
    ApprovalRequest,
    /// A bot `once` or `cron` schedule fired.
    ScheduleWake,
    /// A `TaskScheduler` script invoked `wake` targeting this bot.
    TaskWake,
    /// A webhook trigger woke this bot.
    WebhookWake,
}

impl BotInboxKind {
    /// Stable database / wire string name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AgentMessage => "agent_message",
            Self::DelegateCompleted => "delegate_completed",
            Self::DelegateFailed => "delegate_failed",
            Self::SessionEvent => "session_event",
            Self::ApprovalRequest => "approval_request",
            Self::ScheduleWake => "schedule_wake",
            Self::TaskWake => "task_wake",
            Self::WebhookWake => "webhook_wake",
        }
    }
}

impl fmt::Display for BotInboxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BotInboxKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user_message" => Ok(Self::UserMessage),
            "agent_message" => Ok(Self::AgentMessage),
            "delegate_completed" => Ok(Self::DelegateCompleted),
            "delegate_failed" => Ok(Self::DelegateFailed),
            "session_event" => Ok(Self::SessionEvent),
            "approval_request" => Ok(Self::ApprovalRequest),
            "schedule_wake" => Ok(Self::ScheduleWake),
            "task_wake" => Ok(Self::TaskWake),
            "webhook_wake" => Ok(Self::WebhookWake),
            other => Err(format!("unknown bot inbox kind: {other}")),
        }
    }
}

/// Which permission gate a delegated worker or supervised session hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BotApprovalGate {
    /// Unsandboxed `terminal` execution (`unsafe`).
    Unsafe,
    /// Connecting to or running on an SSH server.
    Ssh,
    /// Publishing an issue, pull request, or comment.
    Publish,
    /// Granting write access to a host directory (`request_workspace`).
    Workspace,
}

impl BotApprovalGate {
    /// Stable string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsafe => "unsafe",
            Self::Ssh => "ssh",
            Self::Publish => "publish",
            Self::Workspace => "workspace",
        }
    }
}

impl fmt::Display for BotApprovalGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BotApprovalGate {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unsafe" => Ok(Self::Unsafe),
            "ssh" => Ok(Self::Ssh),
            "publish" => Ok(Self::Publish),
            "workspace" => Ok(Self::Workspace),
            other => Err(format!("unknown bot approval gate: {other}")),
        }
    }
}

/// Resolution status of a bot-routed permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BotApprovalStatus {
    /// Held under `ApprovalPause` while the bot triages it (no `UserWait` yet).
    PendingBot,
    /// Promoted by the bot via `Escalate { note }` to a human `UserWait` card.
    Escalated,
    /// Approved by the bot or by the human after escalation.
    Approved,
    /// Denied by the bot or by the human after escalation.
    Denied,
}

impl BotApprovalStatus {
    /// Stable string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingBot => "pending_bot",
            Self::Escalated => "escalated",
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

impl fmt::Display for BotApprovalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BotApprovalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending_bot" => Ok(Self::PendingBot),
            "escalated" => Ok(Self::Escalated),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            other => Err(format!("unknown bot approval status: {other}")),
        }
    }
}

/// Decision passed to the bot's `decide_approval` tool.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, skyzen::ToSchema,
)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BotApprovalDecision {
    /// Approve the request and resume the parked requester.
    Approve,
    /// Deny the request with an explanation returned to the requester.
    Deny {
        /// Why the request was denied.
        reason: String,
    },
    /// Escalate the request to the human user, emitting `UserWait` and a
    /// push notification carrying `note`.
    Escalate {
        /// Context and recommendation shown to the user on the approval card.
        note: String,
    },
}

/// Lifecycle state of a bot's delegated bash-first worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BotDelegateStatus {
    /// Currently running.
    Running,
    /// Stopped via `stop_agent` with a persisted checkpoint.
    Stopped,
    /// Finished its task with a persisted checkpoint.
    Completed,
    /// Failed with a persisted checkpoint.
    Failed,
}

impl BotDelegateStatus {
    /// Stable string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for BotDelegateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BotDelegateStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Self::Running),
            "stopped" => Ok(Self::Stopped),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown bot delegate status: {other}")),
        }
    }
}

/// Summary of one May Bot, returned by `GET /api/bots` and SSE snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotSummary {
    /// Bot primary identifier.
    pub id: String,
    /// Stable `word-word-word` `AgentDirectory` identifier.
    pub agent_id: String,
    /// User-visible display name.
    pub name: String,
    /// Optional persona instructions.
    pub persona: Option<String>,
    /// Explicit model selector (`provider::model`), or `None` to use the default.
    pub model: Option<String>,
    /// Optional profile id this bot is associated with.
    pub profile_id: Option<String>,
    /// Whether the bot's resident inbox loop is enabled.
    pub enabled: bool,
    /// Current runtime state.
    pub run_state: BotRunState,
    /// Live per-gate approval policy.
    pub approval_policy: BotApprovalPolicy,
    /// Number of unconsumed inbox entries.
    pub pending_inbox: usize,
    /// Number of pending or escalated approval requests.
    pub pending_approvals: usize,
    /// Number of currently running delegated workers.
    pub active_delegates: usize,
    /// Creation timestamp (RFC 3339 UTC).
    pub created_at: String,
    /// Last update timestamp (RFC 3339 UTC).
    pub updated_at: String,
}

/// Request body for `POST /api/bots`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateBotRequest {
    /// Display name for the bot.
    pub name: String,
    /// Optional persona prompt.
    #[serde(default)]
    pub persona: Option<String>,
    /// Optional model selector (`provider::model`).
    #[serde(default)]
    pub model: Option<String>,
    /// Optional profile id.
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Whether the bot starts enabled (defaults to `true`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional approval policy overrides.
    #[serde(default)]
    pub approval_policy: BotApprovalPolicy,
}

const fn default_true() -> bool {
    true
}

/// Request body for `PATCH /api/bots/{id}` (all fields live-applied without restart).
#[derive(Debug, Clone, Default, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateBotRequest {
    /// Updated display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Updated persona (`Some(None)` clears persona).
    #[serde(default)]
    pub persona: Option<Option<String>>,
    /// Updated model selector (`Some(None)` reverts to default).
    #[serde(default)]
    pub model: Option<Option<String>>,
    /// Toggle enabled state live.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Updated approval policy live.
    #[serde(default)]
    pub approval_policy: Option<BotApprovalPolicy>,
}

/// Request body for `POST /api/bots/{id}/messages`.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendBotMessageRequest {
    /// User message body.
    pub content: String,
}

/// Request body for `POST /api/bots/{id}/approvals/{requestId}`.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolveBotApprovalRequest {
    /// Whether the human approved the escalated request.
    pub approved: bool,
    /// Optional denial reason.
    #[serde(default)]
    pub reason: Option<String>,
}

/// One user <-> bot chat message (only user inputs and `send_message(to="user")` outputs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotChatMessage {
    /// Row id.
    pub id: i64,
    /// Owning bot id.
    pub bot_id: String,
    /// `"user"` or `"bot"`.
    pub role: String,
    /// Message text.
    pub content: String,
    /// Monotonic UTC millisecond key.
    pub ts_ms: i64,
    /// Formatted UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ` or `.sssZ`).
    pub ts_utc: String,
}

/// One internal run trace entry (plain `AgentEvent::Text`, tool calls, inbox drains).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotTraceEntry {
    /// Row id.
    pub id: i64,
    /// Owning bot id.
    pub bot_id: String,
    /// Entry kind (`text`, `tool_call`, `tool_result`, `inbox_drain`, `system`).
    pub kind: String,
    /// Trace body.
    pub content: String,
    /// Formatted UTC timestamp.
    pub ts_utc: String,
}

/// One durable `bot_inbox` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotInboxEntry {
    /// Row id.
    pub id: i64,
    /// Target bot id.
    pub bot_id: String,
    /// Strictly monotonic per-bot UTC millisecond key.
    pub ts_ms: i64,
    /// Formatted UTC timestamp.
    pub ts_utc: String,
    /// Event kind.
    pub kind: BotInboxKind,
    /// Originating agent/session/user id, if any.
    pub sender_id: Option<String>,
    /// Human-readable sender name, if any.
    pub sender_name: Option<String>,
    /// Verbatim event payload.
    pub body: String,
    /// Optional structured metadata JSON.
    pub meta_json: Option<String>,
    /// When the bot consumed this entry (`None` while queued).
    pub consumed_at: Option<String>,
}

/// One delegated worker summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotDelegateSummary {
    /// Stable `word-word-word` agent id.
    pub agent_id: String,
    /// Owning bot id.
    pub bot_id: String,
    /// Display name / label for this worker.
    pub name: String,
    /// Subagent definition name (e.g. `general`).
    pub definition: String,
    /// Working directory on disk.
    pub work_dir: String,
    /// Granted worktree paths.
    pub granted_worktrees: Vec<String>,
    /// Current status.
    pub status: BotDelegateStatus,
    /// Active shell command, if running one.
    pub active_command: Option<String>,
    /// Accumulated runtime in milliseconds.
    pub elapsed_ms: i64,
    /// Prompt tokens consumed.
    pub prompt_tokens: i64,
    /// Completion tokens consumed.
    pub completion_tokens: i64,
    /// Final or latest output summary.
    pub last_output: Option<String>,
    /// Creation time (UTC).
    pub created_at: String,
    /// Last update time (UTC).
    pub updated_at: String,
}

/// One bot-routed permission request (pending bot triage, escalated to user, or resolved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotApprovalItem {
    /// Request identifier.
    pub request_id: String,
    /// Owning bot id.
    pub bot_id: String,
    /// `"delegate"` or `"session"`.
    pub requester_kind: String,
    /// Delegated worker `agent_id` or supervised `session_id`.
    pub requester_id: String,
    /// Which gate was hit.
    pub gate: BotApprovalGate,
    /// Script, SSH host, path, or publication text being requested.
    pub subject: String,
    /// Optional reason supplied by the requester.
    pub reason: Option<String>,
    /// Current status.
    pub status: BotApprovalStatus,
    /// Bot's escalation note or denial reason.
    pub decision_note: Option<String>,
    /// Creation timestamp (UTC).
    pub created_at: String,
    /// Resolution timestamp (UTC), if resolved.
    pub resolved_at: Option<String>,
}

/// One proactive wake schedule belonging to a bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotScheduleItem {
    /// Schedule identifier.
    pub id: String,
    /// Owning bot id.
    pub bot_id: String,
    /// Human-readable schedule name.
    pub name: String,
    /// `"once"` or `"cron"`.
    pub kind: String,
    /// Cron expression (UTC) for `cron` schedules.
    pub cron_expression: Option<String>,
    /// RFC 3339 UTC fire time for `once` schedules.
    pub run_at: Option<String>,
    /// Prompt / wake message enqueued into `bot_inbox`.
    pub prompt: String,
    /// Whether active.
    pub enabled: bool,
    /// Next scheduled fire time (UTC).
    pub next_fire_at: Option<String>,
    /// Last fired time (UTC).
    pub last_fired_at: Option<String>,
}

/// One node in a bot's hierarchical memory tree (`~/.may/bots/<bot_id>/memory/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotMemoryNode {
    /// Path relative to `memory/` (POSIX slashes; `""` for root).
    pub path: String,
    /// File or directory name.
    pub name: String,
    /// Whether this node is a directory.
    pub is_dir: bool,
    /// File contents for Markdown files.
    pub content: Option<String>,
    /// Direct children when `is_dir` is `true`.
    pub children: Vec<Self>,
}

/// One git commit from `~/.may/bots/<bot_id>/memory.git`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotMemoryCommit {
    /// Commit SHA.
    pub sha: String,
    /// Commit subject line.
    pub subject: String,
    /// Full commit message (containing fact, verbatim quote, source agent id, and UTC timestamp).
    pub message: String,
    /// Author name.
    pub author: String,
    /// Commit timestamp (UTC).
    pub timestamp: String,
}

/// Deduplication record persisted alongside a bot's context snapshot so the
/// uncompacted window never injects duplicate leaf memory files or chunks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenMemory {
    /// `blake3` hex digest of the full leaf file content when last seen.
    pub file_hash: String,
    /// `blake3` hex digests of individual excerpts/chunks injected for this file.
    #[serde(default)]
    pub chunk_hashes: Vec<String>,
}

/// Full deduplication ledger persisted in `bot_context_snapshots.seen_memory_json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenMemoryLedger {
    /// Map from normalized leaf relative path (e.g. `preference/food.md`) to last seen hashes.
    #[serde(default)]
    pub seen_in_window: HashMap<String, SeenMemory>,
}

/// Live bot event multiplexed onto `/api/events` under `bot_*` event kinds.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(tag = "eventKind", rename_all = "snake_case")]
pub enum BotSseEvent {
    /// Bot list / runtime status changed (`bot_state`).
    BotState {
        /// Current list of bots.
        bots: Vec<BotSummary>,
    },
    /// New user <-> bot chat message (`bot_chat`).
    BotChat {
        /// The new chat message.
        message: BotChatMessage,
    },
    /// New internal run trace entry (`bot_trace`).
    BotTrace {
        /// The new trace entry.
        entry: BotTraceEntry,
    },
    /// Approval created, escalated, or resolved (`bot_approval`).
    BotApproval {
        /// The updated approval item.
        approval: BotApprovalItem,
    },
    /// Memory tree or `memory.git` updated (`bot_memory`).
    #[serde(rename_all = "camelCase")]
    BotMemory {
        /// Bot whose memory changed.
        bot_id: String,
        /// Latest commit SHA, if any.
        commit_sha: Option<String>,
    },
}

impl BotSseEvent {
    /// SSE event name emitted on `/api/events`.
    #[must_use]
    pub const fn sse_event_name(&self) -> &'static str {
        match self {
            Self::BotState { .. } => "bot_state",
            Self::BotChat { .. } => "bot_chat",
            Self::BotTrace { .. } => "bot_trace",
            Self::BotApproval { .. } => "bot_approval",
            Self::BotMemory { .. } => "bot_memory",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_utc_aligns_to_whole_second_and_advances_by_one_ms_on_burst_or_step_back() {
        let t0: jiff::Timestamp = "2026-06-15T12:34:56.789Z".parse().expect("ts");
        let first = next_monotonic_ts_ms(t0, 0);
        assert_eq!(format_monotonic_utc(first), "2026-06-15T12:34:56Z");

        // Same second burst -> +1ms
        let second = next_monotonic_ts_ms(t0, first);
        assert_eq!(second, first + 1);
        assert_eq!(format_monotonic_utc(second), "2026-06-15T12:34:56.001Z");

        // Clock step-back -> +1ms
        let earlier: jiff::Timestamp = "2026-06-15T12:30:00Z".parse().expect("ts");
        let third = next_monotonic_ts_ms(earlier, second);
        assert_eq!(third, second + 1);
        assert_eq!(format_monotonic_utc(third), "2026-06-15T12:34:56.002Z");

        // Next whole second -> aligns to whole second
        let t1: jiff::Timestamp = "2026-06-15T12:34:57.200Z".parse().expect("ts");
        let fourth = next_monotonic_ts_ms(t1, third);
        assert_eq!(format_monotonic_utc(fourth), "2026-06-15T12:34:57Z");
    }
}
