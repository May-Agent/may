use crate::config::{LoadBalanceStrategy, ProviderType, SshServerConfig};

#[derive(Debug, serde::Serialize)]
pub(crate) struct ApiHealth {
    pub(crate) ok: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
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

#[derive(Debug, serde::Serialize)]
pub(crate) struct RequestLogsResponse {
    pub(crate) logs: Vec<RequestLogEntry>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct MemoryPayload {
    pub(crate) content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ApiBashExecution {
    pub(crate) id: String,
    pub(crate) input: String,
    #[serde(default)]
    pub(crate) output: Option<String>,
    pub(crate) status: String,
    pub(crate) mode: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct BashStateSnapshot {
    #[serde(default)]
    pub(crate) shell_mode_by_id: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub(crate) executions: Vec<ApiBashExecution>,
    #[serde(default)]
    pub(crate) running_by_call_id: std::collections::HashMap<String, ApiBashExecution>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
pub(crate) struct ApiSession {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) last_modified: String,
    pub(crate) preview: String,
    pub(crate) messages: Vec<ApiMessage>,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CreateSessionRequest {
    #[serde(default)]
    pub(crate) model: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UpdateSessionRequest {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ChatRequest {
    pub(crate) session_id: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) attachments: Vec<ApiAttachment>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ChatResponse {
    pub(crate) session_id: String,
    pub(crate) assistant: ApiMessage,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ModelInfoItem {
    pub(crate) id: String,
    pub(crate) nickname: String,
    pub(crate) context_window: Option<u32>,
    pub(crate) abilities: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ModelsResponse {
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) details: Vec<ModelInfoItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ProviderModelsItem {
    pub(crate) provider_id: String,
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) details: Vec<ModelInfoItem>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ProviderModelsResponse {
    pub(crate) providers: Vec<ProviderModelsItem>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PermissionPollQuery {
    pub(crate) session_id: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct PermissionPollResponse {
    pub(crate) request: Option<crate::agent::PermissionRequest>,
    pub(crate) state: Option<&'static str>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PermissionRespondRequest {
    pub(crate) id: String,
    pub(crate) approved: bool,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct UiConfigResponse {
    pub(crate) ssh_servers: Vec<SshServerConfig>,
    pub(crate) max_iterations: usize,
    pub(crate) ui_font: String,
    pub(crate) ui_theme: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SessionRuntimeResponse {
    pub(crate) state: &'static str,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ArtifactEntry {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) is_html: bool,
    pub(crate) fingerprint: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SessionArtifactsResponse {
    pub(crate) artifacts: Vec<ArtifactEntry>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ArtifactStreamPayload {
    pub(crate) count: usize,
    pub(crate) signature: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ArtifactPreviewQuery {
    pub(crate) path: String,
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
pub(crate) struct UiProviderConfig {
    pub(crate) id: String,
    pub(crate) provider_type: ProviderType,
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UiModelsConfig {
    pub(crate) advanced: Vec<String>,
    pub(crate) balanced: Vec<String>,
    pub(crate) fast: Vec<String>,
    pub(crate) image_generation: Vec<String>,
    pub(crate) model_providers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UiPreferencesConfig {
    pub(crate) load_balance_strategy: LoadBalanceStrategy,
    pub(crate) warn_on_model_change: bool,
    pub(crate) show_token_usage: bool,
    pub(crate) strict_model_matching: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UiAppearanceConfig {
    pub(crate) font: String,
    pub(crate) theme: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct UiFullConfig {
    pub(crate) providers: Vec<UiProviderConfig>,
    pub(crate) models: UiModelsConfig,
    pub(crate) preferences: UiPreferencesConfig,
    pub(crate) appearance: UiAppearanceConfig,
    pub(crate) last_model: Option<String>,
    pub(crate) max_iterations: usize,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SshServerRequest {
    pub(crate) name: String,
    pub(crate) target: String,
}

#[derive(Debug, serde::Deserialize)]
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
pub(crate) struct SkillInstallRequest {
    pub(crate) repo: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CopilotPollRequest {
    pub(crate) device_code: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CopilotStatusResponse {
    pub(crate) authenticated: bool,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CopilotStartResponse {
    pub(crate) device_code: String,
    pub(crate) user_code: String,
    pub(crate) verification_uri: String,
    pub(crate) interval: u64,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CopilotPollResponse {
    pub(crate) status: String,
    pub(crate) message: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SkillInfoDto {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) dir_name: String,
    pub(crate) source: String,
}
