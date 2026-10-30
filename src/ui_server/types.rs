use crate::config::{
    BrowserBackendKind, CommandPriority, NativeToolsConfig, ProviderApi, ProviderType,
    RemoteAccessConfig, SshServerConfig,
};
use crate::network_policy::NetworkPolicyMode;
use crate::session::{
    AttachmentKind, PendingUserMessage, PendingUserMessageMode, SessionPlanState,
    SessionRuntimeState,
};
use crate::xml::XmlBlock;
use aither_core::llm::ToolResult;

// ── Typed API Errors ────────────────────────────────────────

/// Machine-readable error codes sent to the frontend, eliminating string-matching.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    BadRequest,
    InternalError,
    NotFound,
    BrowserTakeoverActive,
    SessionNotFound,
    ModelNotConfigured,
    ProviderUnavailable,
    ProfileConfigurationInvalid,
    ContainerPreparing,
    ContainerBuildFailed,
    ContainerStartFailed,
    ContainerRuntimeUnavailable,
    ManagedPodmanApprovalRequired,
    LocationUnavailable,
    UnknownSshServer,
}

/// Structured error body with a machine-readable `code` and human-readable `error` message.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct ApiErrorBody {
    pub(crate) code: ApiErrorCode,
    pub(crate) error: String,
}

// ── SSE Event Payloads ──────────────────────────────────────

/// Semantic variant of a tool call. Selected on `tool_start` so the
/// frontend can route to the correct presenter without re-inferring
/// from the tool name. `Generic` covers MCP and unknown tools; the
/// frontend must fall back to raw-arg partial JSON projection there.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolVariant {
    Terminal,
    TerminalInput,
    TerminalKill,
    TerminalRead,
    Browser,
    WebSearch,
    WebFetch,
    Ask,
    Session,
    Notify,
    Task,
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
            "terminal_kill" => Self::TerminalKill,
            "terminal_read" => Self::TerminalRead,
            "browser" => Self::Browser,
            "web_search" | "websearch" => Self::WebSearch,
            "web_fetch" | "webfetch" => Self::WebFetch,
            "ask" => Self::Ask,
            "session" => Self::Session,
            "notify" => Self::Notify,
            "task" => Self::Task,
            _ => Self::Generic,
        }
    }
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseStartEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) assistant_id: &'a str,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseChunkEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) chunk: &'a str,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseToolStartEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) variant: ToolVariant,
    pub(crate) arguments: &'a str,
    /// Backend-projected summary fields. The frontend reads these directly
    /// for the summary chip; no client-side partial-JSON parsing required.
    #[serde(skip_serializing_if = "crate::ui_server::tool_summary::ToolSummary::is_empty")]
    pub(crate) summary: crate::ui_server::tool_summary::ToolSummary,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseToolDeltaEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) variant: ToolVariant,
    pub(crate) arguments_fragment: &'a str,
    /// Backend-projected summary fields, recomputed from the cumulative
    /// argument buffer at every delta.
    #[serde(skip_serializing_if = "crate::ui_server::tool_summary::ToolSummary::is_empty")]
    pub(crate) summary: crate::ui_server::tool_summary::ToolSummary,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseToolEndEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) variant: ToolVariant,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) arguments: Option<&'a str>,
    #[schema(value_type = serde_json::Value)]
    #[serde(serialize_with = "crate::session::tool_result_wire::serialize")]
    pub(crate) result: &'a ToolResult,
    #[serde(skip_serializing_if = "crate::ui_server::tool_summary::ToolSummary::is_empty")]
    pub(crate) summary: crate::ui_server::tool_summary::ToolSummary,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseErrorEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) code: ApiErrorCode,
    pub(crate) message: &'a str,
    pub(crate) retryable: bool,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseDoneEvent {
    pub(crate) seq: u64,
    pub(crate) turn_id: String,
    pub(crate) session_id: String,
    pub(crate) assistant: ApiMessage,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct SseHeartbeat {
    pub(crate) ok: bool,
}

/// Chat-stream ping emitted every 10 s so proxies and the browser's
/// own idle detection don't close the connection during long tool phases.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SsePingEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
}

/// Live decode-speed sample emitted while model output streams.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseSpeedEvent<'a> {
    pub(crate) seq: u64,
    pub(crate) turn_id: &'a str,
    pub(crate) tokens_per_second: f64,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SseTodoUpdateEvent {
    pub(crate) seq: u64,
    pub(crate) turn_id: String,
    pub(crate) items: Vec<SseTodoItem>,
    pub(crate) total: usize,
    pub(crate) completed: usize,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct SseTodoItem {
    pub(crate) text: String,
    pub(crate) checked: bool,
}

impl SseTodoUpdateEvent {
    /// Build a todo-update payload with the given seq/turn context.
    /// `From<&TodoChecklist>` was removed because the stream envelope
    /// now carries ordering metadata that the checklist itself doesn't know.
    #[must_use]
    pub fn from_checklist(
        checklist: &crate::todo_parser::TodoChecklist,
        seq: u64,
        turn_id: String,
    ) -> Self {
        Self {
            seq,
            turn_id,
            items: checklist
                .items
                .iter()
                .map(|item| SseTodoItem {
                    text: item.text.clone(),
                    checked: item.checked,
                })
                .collect(),
            total: checklist.total(),
            completed: checklist.completed(),
        }
    }
}

// ── Tool Event Records (stored in DB history) ───────────────

pub use crate::session::ToolEventRecord;

// ── Config SSE Event Payload ────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatusEventPayload {
    pub(crate) enabled: bool,
    pub(crate) running: bool,
    pub(crate) cdp_endpoint: Option<String>,
    pub(crate) owner_session_id: Option<String>,
    pub(crate) takeover_session_id: Option<String>,
}

// ── Browser Lifecycle SSE Event Payload ─────────────────────

pub use crate::container::browser::BrowserLifecycleEvent;

// ── Generic OK Responses ────────────────────────────────────

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTakeoverResponse {
    pub(crate) ok: bool,
    pub(crate) session_id: String,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskTriggerStatus {
    Triggered,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct TaskTriggerResponse {
    pub(crate) ok: bool,
    pub(crate) status: TaskTriggerStatus,
    pub(crate) triggered: usize,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct NotificationSentResponse {
    pub(crate) ok: bool,
    pub(crate) sent: usize,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMailboxStatus {
    Pending,
    Delivered,
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

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationMailboxEntryResponse {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) session_id: Option<String>,
    pub(crate) status: NotificationMailboxStatus,
    pub(crate) sent_count: i32,
    pub(crate) last_error: Option<String>,
    pub(crate) delivered_at: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) has_callback: bool,
}

impl From<crate::db::entity::notification_mailbox::Model> for NotificationMailboxEntryResponse {
    fn from(value: crate::db::entity::notification_mailbox::Model) -> Self {
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
            has_callback: value.callback_context_xml.is_some() || value.callback_message.is_some(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationMailboxResponse {
    pub(crate) items: Vec<NotificationMailboxEntryResponse>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationCallbackResponse {
    pub(crate) notification_id: String,
    pub(crate) session_id: Option<String>,
    pub(crate) profile_id: Option<String>,
    pub(crate) has_callback: bool,
    pub(crate) callback_consumed: bool,
    pub(crate) callback_context_xml: Option<String>,
    pub(crate) callback_message: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAccessLogEntryResponse {
    pub(crate) id: i32,
    pub(crate) session_id: Option<String>,
    pub(crate) profile_id: Option<String>,
    pub(crate) source: crate::db::NetworkAccessSource,
    pub(crate) mode: crate::db::NetworkAccessMode,
    pub(crate) domain: String,
    pub(crate) port: i32,
    pub(crate) allowed: bool,
    pub(crate) reason: Option<String>,
    pub(crate) timestamp: String,
}

impl From<crate::db::entity::network_access_logs::Model> for NetworkAccessLogEntryResponse {
    fn from(value: crate::db::entity::network_access_logs::Model) -> Self {
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

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAccessLogsResponse {
    pub(crate) items: Vec<NetworkAccessLogEntryResponse>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAccessState {
    Disabled,
    Starting,
    Running,
    Error,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccessStatusResponse {
    pub(crate) state: RemoteAccessState,
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) public_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) recent_logs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccessEventPayload {
    pub(crate) status: RemoteAccessStatusResponse,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionTasksPayload {
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) plan_state: SessionPlanState,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionPlanStateRequest {
    pub(crate) plan_state: SessionPlanState,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionPlanStateResponse {
    pub(crate) plan_state: SessionPlanState,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiHealth {
    pub(crate) ok: bool,
}

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
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestStats {
    #[serde(default)]
    pub(crate) provider_id: Option<String>,
    #[serde(default)]
    pub(crate) prompt_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) completion_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) total_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_read_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_write_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) context_window: Option<u32>,
    #[serde(default)]
    pub(crate) elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogEntry {
    pub(crate) session_id: String,
    pub(crate) timestamp: String,
    pub(crate) model: String,
    pub(crate) provider_id: String,
    pub(crate) prompt_tokens: Option<u32>,
    pub(crate) completion_tokens: Option<u32>,
    pub(crate) total_tokens: Option<u32>,
    pub(crate) cache_read_tokens: Option<u32>,
    pub(crate) cache_write_tokens: Option<u32>,
    pub(crate) context_window: Option<u32>,
    pub(crate) elapsed_ms: u64,
    pub(crate) ok: bool,
    pub(crate) error: Option<String>,
}

impl From<crate::db::entity::request_logs::Model> for RequestLogEntry {
    fn from(value: crate::db::entity::request_logs::Model) -> Self {
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
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogCacheSummary {
    pub(crate) assistant_message_count: u64,
    pub(crate) cache_hit_message_count: u64,
    pub(crate) prompt_tokens_total: u64,
    pub(crate) cache_read_tokens_total: u64,
    pub(crate) cache_write_tokens_total: u64,
    pub(crate) cache_read_hit_rate: Option<f64>,
}

impl From<crate::db::AssistantCacheUsageSummary> for RequestLogCacheSummary {
    fn from(value: crate::db::AssistantCacheUsageSummary) -> Self {
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
    pub(crate) fn new(
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
        }
    }

    pub(crate) const fn success(mut self, elapsed_ms: u64) -> Self {
        self.ok = true;
        self.elapsed_ms = elapsed_ms;
        self
    }

    pub(crate) fn failure(mut self, elapsed_ms: u64, error: impl Into<String>) -> Self {
        self.ok = false;
        self.elapsed_ms = elapsed_ms;
        self.error = Some(error.into());
        self
    }
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogsResponse {
    pub(crate) logs: Vec<RequestLogEntry>,
    pub(crate) cache_summary: RequestLogCacheSummary,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryPayload {
    pub(crate) content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiAttachment {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) attachment_type: AttachmentKind,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) upload_data_url: Option<String>,
    #[serde(default)]
    pub(crate) text_content: Option<String>,
}

/// Native terminal command tool name.
///
/// This identifies the terminal command execution tool in the protocol.
pub const TERMINAL_TOOL_NAME: &str = "terminal";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TerminalExecutionStatus {
    Running,
    Done,
    Failed,
    Stopped,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TerminalCommandMode {
    Default,
    Sandboxed,
    Unsafe,
    Ssh,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiTerminalExecution {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) input: String,
    #[serde(default)]
    pub(crate) output: Option<String>,
    pub(crate) status: TerminalExecutionStatus,
    pub(crate) mode: TerminalCommandMode,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStateSnapshot {
    #[serde(default)]
    pub(crate) executions: Vec<ApiTerminalExecution>,
    #[serde(default)]
    pub(crate) running_by_call_id: std::collections::HashMap<String, ApiTerminalExecution>,
}

impl TerminalStateSnapshot {
    pub(crate) fn all_executions(&self) -> Vec<ApiTerminalExecution> {
        let mut executions = self.executions.clone();
        executions.extend(self.running_by_call_id.values().cloned());
        executions
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiMessage {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) timestamp: String,
    #[serde(default)]
    pub(crate) attachments: Vec<ApiAttachment>,
    #[serde(default)]
    pub(crate) thinking: Option<String>,
    #[serde(default)]
    pub(crate) is_streaming: Option<bool>,
    #[serde(default)]
    pub(crate) is_thinking: Option<bool>,
    #[serde(default)]
    pub(crate) tool_event: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) terminal_executions: Vec<ApiTerminalExecution>,
    #[serde(default)]
    pub(crate) stats: Option<RequestStats>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiSession {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) last_modified: String,
    pub(crate) preview: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) messages: Vec<ApiMessage>,
    pub(crate) profile_id: Option<String>,
    pub(crate) model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) runtime: SessionRuntimeResponse,
}

impl ApiSession {
    /// Build an [`ApiSession`] from a [`SessionSummary`] with a resolved model.
    pub(crate) fn from_summary(
        summary: crate::session::SessionSummary,
        model: String,
        messages: Vec<ApiMessage>,
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
            messages,
            profile_id: summary.profile_id,
            model,
            reasoning_effort: summary.reasoning_effort.clone(),
            runtime: summary.runtime.clone().into(),
        }
    }

    /// Build a fresh [`ApiSession`] for a newly created session.
    pub(crate) fn new_empty(id: String, model: String, profile_id: Option<String>) -> Self {
        Self {
            id,
            title: "New Chat".to_string(),
            last_modified: jiff::Timestamp::now().to_string(),
            preview: "New conversation started".to_string(),
            messages: Vec::new(),
            profile_id,
            model,
            reasoning_effort: None,
            runtime: crate::session::SessionRuntimeSummary::default().into(),
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSessionRequest {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// Tri-state reasoning-effort patch:
    /// field absent → `None` (leave DB untouched);
    /// field present + `null` → `Some(NullableStringPatch::Null)` (clear to NULL);
    /// field present + string → `Some(NullableStringPatch::Set(s))` (set).
    #[serde(default, deserialize_with = "deserialize_present_patch")]
    pub(crate) reasoning_effort: Option<NullableStringPatch>,
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

#[derive(Debug, Clone, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    pub(crate) session_id: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) attachments: Vec<ApiAttachment>,
    /// Structured hidden context fragment for the next turn.
    /// XML-like tags are allowed, but strict XML validity is not required.
    #[serde(default)]
    pub(crate) hidden_context_xml: Option<XmlBlock>,
    #[serde(default)]
    pub(crate) delivery_mode: PendingUserMessageMode,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemovePendingMessageRequest {
    pub(crate) kind: PendingMessageKind,
    #[serde(default)]
    pub(crate) index: Option<usize>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PendingMessageKind {
    PendingImmediate,
    Queued,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextDebugMessage {
    pub(crate) role: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) attachments: Vec<String>,
    #[serde(default)]
    pub(crate) tool_call_id: Option<String>,
    #[serde(default)]
    pub(crate) tool_call_names: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextDebugResponse {
    pub(crate) session_id: String,
    pub(crate) timestamp: String,
    pub(crate) model: String,
    pub(crate) provider_id: String,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) prompt_attachments: Vec<String>,
    pub(crate) assembled_messages: Vec<ContextDebugMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) window_snapshot: Option<SessionContextWindowSnapshotResponse>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextWindowSnapshotResponse {
    #[schema(value_type = String)]
    pub(crate) phase: aither_agent::ContextWindowPhase,
    pub(crate) metrics: SessionContextWindowMetricsResponse,
    pub(crate) assembled_messages: Vec<ContextDebugMessage>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextWindowMetricsResponse {
    pub(crate) usage_fraction: f32,
    pub(crate) context_window: u32,
    pub(crate) estimated_tokens: usize,
    pub(crate) system_block_count: usize,
    pub(crate) recent_message_count: usize,
    pub(crate) recent_system_message_count: usize,
    pub(crate) has_handoff: bool,
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

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfoItem {
    pub(crate) id: String,
    pub(crate) model_id: String,
    pub(crate) provider_id: String,
    pub(crate) nickname: String,
    pub(crate) context_window: Option<u32>,
    pub(crate) abilities: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelsResponse {
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) details: Vec<ModelInfoItem>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelsItem {
    pub(crate) provider_id: String,
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) details: Vec<ModelInfoItem>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelsResponse {
    pub(crate) providers: Vec<ProviderModelsItem>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionPollItem {
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) request: crate::agent::PermissionRequest,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePollItem {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractionResponse {
    pub(crate) takeover_active: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInteractionResponse {
    pub(crate) task_id: Option<String>,
    pub(crate) output_file: Option<String>,
    pub(crate) output_preview: Option<String>,
    pub(crate) input_notice: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionResponse {
    pub(crate) browser: BrowserInteractionResponse,
    pub(crate) terminal: TerminalInteractionResponse,
}

impl From<crate::session::SessionRuntimeSummary> for SessionRuntimeResponse {
    fn from(summary: crate::session::SessionRuntimeSummary) -> Self {
        Self {
            state: summary.state,
            plan_state: summary.plan_state,
            interactions: SessionInteractionResponse {
                browser: BrowserInteractionResponse {
                    takeover_active: summary.interactions.browser.takeover_active,
                },
                terminal: TerminalInteractionResponse {
                    task_id: summary.interactions.terminal.task_id,
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
                    request: crate::agent::PermissionRequest {
                        mode: request.request.mode,
                        script: request.request.script,
                        session_id: request.request.session_id,
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

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRespondRequest {
    pub(crate) id: String,
    pub(crate) approved: bool,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRespondRequest {
    pub(crate) id: String,
    pub(crate) approved: bool,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiConfigResponse {
    pub(crate) ssh_servers: Vec<SshServerConfig>,
    pub(crate) max_iterations: usize,
    pub(crate) ui_font: String,
    pub(crate) ui_theme: String,
    pub(crate) full_config: UiFullConfig,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapProfileSessionsError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<ApiErrorCode>,
    pub(crate) message: String,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiBootstrapResponse {
    pub(crate) sessions: Vec<ApiSession>,
    pub(crate) profiles: Vec<crate::profile::ProfileInfo>,
    pub(crate) default_profile_id: String,
    pub(crate) config: UiConfigResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_profile_sessions: Option<crate::profile::ProfileSessions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_profile_sessions_error: Option<BootstrapProfileSessionsError>,
    #[serde(default)]
    pub(crate) default_profile_skills: Vec<SkillInfoDto>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeResponse {
    pub(crate) state: SessionRuntimeState,
    pub(crate) plan_state: SessionPlanState,
    pub(crate) interactions: SessionInteractionResponse,
    pub(crate) queued_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_immediate: Option<PendingUserMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) queued_messages: Vec<PendingUserMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_permission: Option<PermissionPollItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_workspace: Option<WorkspacePollItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_ask_user: Option<crate::session::PersistedAskUserRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) open_questions: Vec<crate::session::PersistedAskUserRequest>,
}

impl Default for SessionRuntimeResponse {
    fn default() -> Self {
        Self {
            state: SessionRuntimeState::Idle,
            plan_state: SessionPlanState::Disabled,
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

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeEventPayload {
    pub(crate) session_id: String,
    pub(crate) runtime: SessionRuntimeResponse,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSnapshotItem {
    pub(crate) session_id: String,
    pub(crate) runtime: SessionRuntimeResponse,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimesEventPayload {
    pub(crate) sessions: Vec<SessionRuntimeSnapshotItem>,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSessionsEventPayload {
    pub(crate) profile_sessions: crate::profile::ProfileSessions,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactEntry {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) is_html: bool,
    pub(crate) fingerprint: String,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionArtifactsResponse {
    pub(crate) artifacts: Vec<ArtifactEntry>,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactStreamPayload {
    pub(crate) count: usize,
    pub(crate) signature: String,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPreviewQuery {
    pub(crate) path: String,
}

const MAX_ARTIFACT_TYPE_BYTES: usize = 64;

#[derive(Debug, Clone)]
pub struct ArtifactType(String);

impl ArtifactType {
    pub(crate) const fn as_str(&self) -> &str {
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

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRuntimePreviewQuery {
    pub(crate) path: String,
    #[schema(value_type = String)]
    pub(crate) artifact_type: ArtifactType,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SlidevPreviewResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preview_url: Option<String>,
    pub(crate) diagnostics: Vec<String>,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDiagnosticsRequest {
    pub(crate) path: String,
    #[schema(value_type = String)]
    pub(crate) artifact_type: ArtifactType,
    pub(crate) diagnostics: Vec<String>,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDiagnosticsResponse {
    pub(crate) repair_scheduled: bool,
}

#[derive(Debug, Clone)]
pub struct IfNoneMatchHeader(pub(crate) Option<String>);

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
    pub(crate) web_search: bool,
    pub(crate) web_fetch: bool,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiProviderConfig {
    pub(crate) id: String,
    pub(crate) provider_type: ProviderType,
    /// Whether an API key is stored for this provider. On save, `false`
    /// clears the stored key unless `api_key` carries new material.
    pub(crate) has_api_key: bool,
    /// New key material entered by the user. Never serialized back out: the
    /// config response only reports `has_api_key`.
    #[serde(default, skip_serializing)]
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) native_tools: UiNativeToolsConfig,
    /// Default cache lifetime of the main conversation: a duration such as
    /// `1h`, or `permanent`.
    #[serde(default)]
    #[schema(value_type = String)]
    pub(crate) cache_lifetime: crate::cache_lifetime::CacheLifetime,
    /// Wire API a `custom` provider speaks; must round-trip the config save or
    /// `api = "responses"` is silently dropped on every UI write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api: Option<ProviderApi>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiModelsConfig {
    pub(crate) advanced: Vec<String>,
    pub(crate) balanced: Vec<String>,
    pub(crate) fast: Vec<String>,
    pub(crate) image_generation: Vec<String>,
    /// Model compaction runs on instead of the session model.
    #[serde(default)]
    pub(crate) compaction_model: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiPreferencesConfig {
    pub(crate) warn_on_model_change: bool,
    pub(crate) show_token_usage: bool,
    pub(crate) show_decode_speed: bool,
    #[schema(value_type = String)]
    pub(crate) network_policy_mode: NetworkPolicyMode,
    pub(crate) network_policy_hosts: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiAppearanceConfig {
    pub(crate) font: String,
    pub(crate) theme: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiBrowserConfig {
    pub(crate) enabled: bool,
    pub(crate) backend: BrowserBackendKind,
    pub(crate) port: u16,
    pub(crate) remote_cdp_endpoint: Option<String>,
    pub(crate) screencast_jpeg_quality: u32,
    pub(crate) encoder_initial_bitrate_kbps: u32,
    pub(crate) screencast_max_width: u32,
    pub(crate) screencast_max_height: u32,
    pub(crate) every_nth_frame: u32,
    pub(crate) stealth_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_profile: Option<UiSystemProfileConfig>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSystemProfileConfig {
    pub(crate) path: String,
    pub(crate) copy_on_first_use: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiRemoteAccessConfig {
    pub(crate) enabled: bool,
    pub(crate) cloudflared_binary: String,
    pub(crate) tunnel_token: Option<String>,
    pub(crate) public_url: Option<String>,
    pub(crate) allow_local_environment: bool,
}

impl From<&RemoteAccessConfig> for UiRemoteAccessConfig {
    fn from(value: &RemoteAccessConfig) -> Self {
        Self {
            enabled: value.enabled,
            cloudflared_binary: value.cloudflared_binary.clone(),
            tunnel_token: value.tunnel_token.clone(),
            public_url: value.public_url.clone(),
            allow_local_environment: value.allow_local_environment,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSandboxConfig {
    #[schema(value_type = String)]
    pub(crate) command_priority: CommandPriority,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiFullConfig {
    pub(crate) providers: Vec<UiProviderConfig>,
    pub(crate) models: UiModelsConfig,
    pub(crate) preferences: UiPreferencesConfig,
    pub(crate) appearance: UiAppearanceConfig,
    pub(crate) last_model: Option<String>,
    pub(crate) max_iterations: usize,
    pub(crate) browser: UiBrowserConfig,
    pub(crate) remote_access: UiRemoteAccessConfig,
    pub(crate) sandbox: UiSandboxConfig,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SshServerRequest {
    pub(crate) name: String,
    pub(crate) target: String,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpCreateRequest {
    Process {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    },
    Url {
        name: String,
        url: String,
    },
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillInstallRequest {
    pub(crate) repo: String,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStatusResponse {
    pub(crate) authenticated: bool,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStartResponse {
    pub(crate) device_code: String,
    pub(crate) user_code: String,
    pub(crate) verification_uri: String,
    pub(crate) interval: u64,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopilotAuthEvent {
    pub(crate) status: CopilotAuthStatus,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CopilotAuthStatus {
    Pending,
    Success,
    Expired,
    Denied,
    Error,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillInfoDto {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) dir_name: String,
    pub(crate) source: crate::skill::SkillSource,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LastModelRequest {
    pub(crate) model: String,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySaveRequest {
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) explicit_user_request: bool,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalUrlMapRequest {
    pub(crate) url: String,
}

#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalUrlMapResponse {
    pub(crate) mapped_url: String,
}

#[cfg(test)]
mod tests {
    use super::{NullableStringPatch, UpdateSessionRequest};

    fn reasoning_effort(body: &str) -> Option<NullableStringPatch> {
        serde_json::from_str::<UpdateSessionRequest>(body)
            .expect("update request parses")
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
