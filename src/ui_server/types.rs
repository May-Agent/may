use crate::config::{LoadBalanceStrategy, PlaywrightBrowser, ProviderType, SshServerConfig};
use crate::session::{PendingUserMessage, PendingUserMessageMode, SessionRuntimeState};

// ── Typed API Errors ────────────────────────────────────────

/// Machine-readable error codes sent to the frontend, eliminating string-matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiErrorCode {
    BadRequest,
    InternalError,
    NotFound,
    ContainerPreparing,
    ContainerBuildFailed,
    ContainerRuntimeUnavailable,
    ManagedPodmanApprovalRequired,
}

/// Structured error body with a machine-readable `code` and human-readable `error` message.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ApiErrorBody {
    pub(crate) code: ApiErrorCode,
    pub(crate) error: String,
}

// ── SSE Event Payloads ──────────────────────────────────────

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SseStartEvent<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) assistant_id: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SseChunkEvent<'a> {
    pub(crate) chunk: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SseToolStartEvent<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SseToolEndEvent<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) arguments: Option<&'a str>,
    pub(crate) result: &'a Result<String, String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SseErrorEvent<'a> {
    pub(crate) message: &'a str,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SseDoneEvent {
    pub(crate) session_id: String,
    pub(crate) assistant: ApiMessage,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SseHeartbeat {
    pub(crate) ok: bool,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SseTodoUpdateEvent {
    pub(crate) items: Vec<SseTodoItem>,
    pub(crate) total: usize,
    pub(crate) completed: usize,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SseTodoItem {
    pub(crate) text: String,
    pub(crate) checked: bool,
}

impl From<&crate::todo_parser::TodoChecklist> for SseTodoUpdateEvent {
    fn from(checklist: &crate::todo_parser::TodoChecklist) -> Self {
        Self {
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "stage")]
pub(crate) enum ToolEventRecord {
    #[serde(rename = "start")]
    Start {
        id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "end")]
    End {
        id: String,
        name: String,
        result: Result<String, String>,
    },
}

// ── Config SSE Event Payload ────────────────────────────────

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigEventPayload {
    pub(crate) ui_font: String,
    pub(crate) ui_theme: String,
    pub(crate) ssh_servers: Vec<SshServerConfig>,
    pub(crate) max_iterations: usize,
}

// ── Generic OK Responses ────────────────────────────────────

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTakeoverResponse {
    pub(crate) ok: bool,
    pub(crate) session_id: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct TaskTriggerResponse {
    pub(crate) ok: bool,
    pub(crate) status: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct NotificationSentResponse {
    pub(crate) ok: bool,
    pub(crate) sent: usize,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiHealth {
    pub(crate) ok: bool,
}

pub(crate) const API_OK: ApiHealth = ApiHealth { ok: true };

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestStats {
    #[serde(default)]
    pub(crate) provider_id: Option<String>,
    #[serde(default)]
    pub(crate) prompt_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) completion_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) total_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) context_window: Option<u32>,
    #[serde(default)]
    pub(crate) elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestLogEntry {
    pub(crate) session_id: String,
    pub(crate) timestamp: String,
    pub(crate) model: String,
    pub(crate) provider_id: String,
    pub(crate) prompt_tokens: Option<u32>,
    pub(crate) completion_tokens: Option<u32>,
    pub(crate) total_tokens: Option<u32>,
    pub(crate) context_window: Option<u32>,
    pub(crate) elapsed_ms: u64,
    pub(crate) ok: bool,
    pub(crate) error: Option<String>,
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
            context_window,
            elapsed_ms: 0,
            ok: false,
            error: None,
        }
    }

    pub(crate) fn success(mut self, elapsed_ms: u64) -> Self {
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

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestLogsResponse {
    pub(crate) logs: Vec<RequestLogEntry>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryPayload {
    pub(crate) content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiAttachment {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) attachment_type: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) upload_data_url: Option<String>,
    #[serde(default)]
    pub(crate) text_content: Option<String>,
}

/// The canonical tool name used by bash execution tracking.
pub(crate) const BASH_TOOL_NAME: &str = "bash";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BashExecutionStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiBashExecution {
    pub(crate) id: String,
    pub(crate) input: String,
    #[serde(default)]
    pub(crate) output: Option<String>,
    pub(crate) status: BashExecutionStatus,
    pub(crate) mode: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BashStateSnapshot {
    #[serde(default)]
    pub(crate) shell_mode_by_id: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub(crate) executions: Vec<ApiBashExecution>,
    #[serde(default)]
    pub(crate) running_by_call_id: std::collections::HashMap<String, ApiBashExecution>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiMessage {
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
    pub(crate) bash_executions: Vec<ApiBashExecution>,
    #[serde(default)]
    pub(crate) stats: Option<RequestStats>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiSession {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) last_modified: String,
    pub(crate) preview: String,
    pub(crate) messages: Vec<ApiMessage>,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) runtime_state: SessionRuntimeState,
    #[serde(default)]
    pub(crate) queued_count: usize,
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
            .map(|t| t.to_string())
            .unwrap_or_else(|| jiff::Timestamp::now().to_string());
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
            model,
            reasoning_effort: None,
            runtime_state: summary.runtime.state,
            queued_count: summary.runtime.queued_count,
        }
    }

    /// Build a fresh [`ApiSession`] for a newly created session.
    pub(crate) fn new_empty(id: String, model: String) -> Self {
        Self {
            id,
            title: "New Chat".to_string(),
            last_modified: jiff::Timestamp::now().to_string(),
            preview: "New conversation started".to_string(),
            messages: Vec::new(),
            model,
            reasoning_effort: None,
            runtime_state: SessionRuntimeState::Idle,
            queued_count: 0,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionRequest {
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateSessionRequest {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatRequest {
    pub(crate) session_id: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) attachments: Vec<ApiAttachment>,
    #[serde(default)]
    pub(crate) hidden_context_xml: Option<String>,
    #[serde(default)]
    pub(crate) delivery_mode: PendingUserMessageMode,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatResponse {
    pub(crate) session_id: String,
    pub(crate) assistant: ApiMessage,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemovePendingMessageRequest {
    pub(crate) kind: PendingMessageKind,
    #[serde(default)]
    pub(crate) index: Option<usize>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingMessageKind {
    PendingImmediate,
    Queued,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextDebugMessage {
    pub(crate) role: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) attachments: Vec<String>,
    #[serde(default)]
    pub(crate) tool_call_id: Option<String>,
    #[serde(default)]
    pub(crate) tool_call_names: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionContextDebugResponse {
    pub(crate) session_id: String,
    pub(crate) timestamp: String,
    pub(crate) model: String,
    pub(crate) provider_id: String,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) prompt_attachments: Vec<String>,
    pub(crate) assembled_messages: Vec<ContextDebugMessage>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelInfoItem {
    pub(crate) id: String,
    pub(crate) nickname: String,
    pub(crate) context_window: Option<u32>,
    pub(crate) abilities: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelsResponse {
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) details: Vec<ModelInfoItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderModelsItem {
    pub(crate) provider_id: String,
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) details: Vec<ModelInfoItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderModelsResponse {
    pub(crate) providers: Vec<ProviderModelsItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionPollItem {
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) request: crate::agent::PermissionRequest,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionPollResponse {
    pub(crate) request: Option<PermissionPollItem>,
    pub(crate) state: Option<&'static str>,
}

impl From<crate::session::SessionRuntimeSummary> for SessionRuntimeResponse {
    fn from(summary: crate::session::SessionRuntimeSummary) -> Self {
        Self {
            state: summary.state,
            queued_count: summary.queued_count,
            pending_immediate: summary.pending_immediate,
            queued_messages: summary.queued_messages,
            waiting_permission: summary
                .waiting_permission
                .map(|request| PermissionPollItem {
                    id: request.id,
                    request: crate::agent::PermissionRequest {
                        mode: request
                            .request
                            .mode
                            .parse()
                            .expect("persisted permission request mode must be valid"),
                        script: request.request.script,
                        session_id: request.request.session_id,
                    },
                }),
            waiting_ask_user: summary.waiting_ask_user,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionRespondRequest {
    pub(crate) id: String,
    pub(crate) approved: bool,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiConfigResponse {
    pub(crate) ssh_servers: Vec<SshServerConfig>,
    pub(crate) max_iterations: usize,
    pub(crate) ui_font: String,
    pub(crate) ui_theme: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRuntimeResponse {
    pub(crate) state: SessionRuntimeState,
    #[serde(default)]
    pub(crate) queued_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_immediate: Option<PendingUserMessage>,
    #[serde(default)]
    pub(crate) queued_messages: Vec<PendingUserMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_permission: Option<PermissionPollItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_ask_user: Option<crate::session::PersistedAskUserRequest>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRuntimeEventPayload {
    pub(crate) session_id: String,
    pub(crate) runtime: SessionRuntimeResponse,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRuntimeSnapshotItem {
    pub(crate) session_id: String,
    pub(crate) runtime: SessionRuntimeResponse,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRuntimesEventPayload {
    pub(crate) sessions: Vec<SessionRuntimeSnapshotItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactEntry {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) is_html: bool,
    pub(crate) fingerprint: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionArtifactsResponse {
    pub(crate) artifacts: Vec<ArtifactEntry>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactStreamPayload {
    pub(crate) count: usize,
    pub(crate) signature: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactPreviewQuery {
    pub(crate) path: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SlidevPreviewResponse {
    pub(crate) preview_url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct IfNoneMatchHeader(pub(crate) Option<String>);

impl skyzen::extract::Extractor for IfNoneMatchHeader {
    type Error = std::convert::Infallible;

    async fn extract(request: &mut skyzen::Request) -> Result<Self, Self::Error> {
        let value = request
            .headers()
            .get(skyzen::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiProviderConfig {
    pub(crate) id: String,
    pub(crate) provider_type: ProviderType,
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiModelsConfig {
    pub(crate) advanced: Vec<String>,
    pub(crate) balanced: Vec<String>,
    pub(crate) fast: Vec<String>,
    pub(crate) image_generation: Vec<String>,
    pub(crate) model_providers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiPreferencesConfig {
    pub(crate) load_balance_strategy: LoadBalanceStrategy,
    pub(crate) warn_on_model_change: bool,
    pub(crate) show_token_usage: bool,
    pub(crate) strict_model_matching: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiAppearanceConfig {
    pub(crate) font: String,
    pub(crate) theme: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiPlaywrightConfig {
    pub(crate) enabled: bool,
    pub(crate) browser: PlaywrightBrowser,
    pub(crate) port: u16,
    pub(crate) screencast_quality: u32,
    pub(crate) screencast_max_width: u32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiFullConfig {
    pub(crate) providers: Vec<UiProviderConfig>,
    pub(crate) models: UiModelsConfig,
    pub(crate) preferences: UiPreferencesConfig,
    pub(crate) appearance: UiAppearanceConfig,
    pub(crate) last_model: Option<String>,
    pub(crate) max_iterations: usize,
    pub(crate) playwright: UiPlaywrightConfig,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SshServerRequest {
    pub(crate) name: String,
    pub(crate) target: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpAddRequest {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillInstallRequest {
    pub(crate) repo: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopilotPollRequest {
    pub(crate) device_code: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopilotStatusResponse {
    pub(crate) authenticated: bool,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopilotStartResponse {
    pub(crate) device_code: String,
    pub(crate) user_code: String,
    pub(crate) verification_uri: String,
    pub(crate) interval: u64,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopilotPollResponse {
    pub(crate) status: String,
    pub(crate) message: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillInfoDto {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) dir_name: String,
    pub(crate) source: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastModelRequest {
    pub(crate) model: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemorySaveRequest {
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) explicit_user_request: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalUrlMapRequest {
    pub(crate) url: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalUrlMapResponse {
    pub(crate) mapped_url: String,
}
