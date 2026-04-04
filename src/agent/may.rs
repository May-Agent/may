//! May agent wrapper around aither-agent.

use std::{borrow::Cow, collections::HashSet, path::PathBuf, sync::Arc};

use aither_agent::{
    Agent, AgentError, AgentEvent, AgentKind, BrowserBackend, CheckpointContext, CompactResult,
    HCons, Hook, SubagentDefinition, TerminalAgentBuilder, TurnBoundaryAction, TurnBoundaryContext,
};
use aither_cloud::{
    CloudProvider, claude::Claude, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_core::LanguageModel;
use aither_core::image::{ImageGenerator, Prompt, Size};
use aither_core::llm::{
    Event as LlmEvent, LLMRequest, Message as LlmMessage, Tool, ToolResult,
    model::{Ability, Parameters, Profile as ModelProfile, ReasoningEffort},
};
use aither_sandbox::{
    ContainerExecHandle, ContainerShellRuntime, ShellRuntimeAvailability, SshServer, TerminalMode,
    TerminalTool,
    permission::{PermissionError, PermissionHandler},
};
use askama::Template;
use executor_core::DefaultExecutor;
use futures_lite::{Stream, StreamExt};

use crate::agent::browser_tool::BrowserBroker;
use crate::agent::register_tool::RegisterBroker;
use crate::agent::task_tool::TaskRuntimeKind;
use crate::ask_user::{AskUserBroker, AskUserTool};
use crate::config::{MatchMode, MayConfig, ProviderInstance, ProviderType, model_ids_match};
use crate::copilot::clear_copilot_credentials;
use crate::embedding_provider::{EmbeddingModelResolution, resolve_embedding_model};
use crate::live_response::LiveResponseRegistry;
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyMode};
use crate::session::{MessageRole, SessionId, SessionPaths, SessionPlanState};
use crate::skill::SkillManager;
use crate::subagent::SubagentManager;
use crate::terminal_commands::{SharedTerminalCommandConfig, register_shared_terminal_commands};
use crate::terminal_permission_handler::TerminalPermissionHandler;
use crate::workspace::{Workspace as MayWorkspace, WorkspaceDir as MayWorkspaceDir};
use crate::workspace_request::WorkspaceRequestBroker;

// Error message templates
const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");

fn join_text(parts: &[&str]) -> String {
    let capacity = parts.iter().map(|part| part.len()).sum();
    let mut output = String::with_capacity(capacity);
    for part in parts {
        output.push_str(part);
    }
    output
}

fn quoted_name_error(prefix: &str, name: &str, suffix: &str) -> String {
    join_text(&[prefix, "'", name, "'", suffix])
}

fn prefixed_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&[prefix, error_text.as_str()])
}

fn err_no_provider(model: &str) -> String {
    quoted_name_error(
        "No provider available for model ",
        model,
        ". Use /config to add providers.",
    )
}

fn err_api_key_required(provider: &str) -> String {
    quoted_name_error("API key required for provider ", provider, "")
}

fn ordered_provider_ids_for_model(config: &MayConfig, model: &str) -> Vec<String> {
    let strict = config.preferences.strict_model_matching;
    let mapped_provider = config
        .models
        .provider_for_model(model, strict)
        .map(str::to_string);

    let mut explicit = Vec::new();
    let mut generic = Vec::new();

    for (id, instance) in &config.provider {
        if !instance.can_serve_model(id, model) {
            continue;
        }
        if instance.is_generic_model_proxy(id) {
            generic.push(id.clone());
        } else {
            explicit.push(id.clone());
        }
    }

    explicit.sort_unstable();
    generic.sort_unstable();

    let mut ordered = explicit;
    ordered.extend(generic);

    if let Some(mapped) = mapped_provider
        && let Some(index) = ordered.iter().position(|id| id == &mapped)
    {
        let preferred = ordered.remove(index);
        ordered.insert(0, preferred);
    }

    ordered
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderCandidate {
    pub model: String,
    pub provider_id: String,
}

#[derive(Clone)]
struct RequestParameterizedModel<M> {
    inner: M,
    parameters: Parameters,
}

impl<M> RequestParameterizedModel<M> {
    const fn new(inner: M, parameters: Parameters) -> Self {
        Self { inner, parameters }
    }
}

impl<M> LanguageModel for RequestParameterizedModel<M>
where
    M: LanguageModel + Clone,
{
    type Error = M::Error;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<LlmEvent, Self::Error>> + Send {
        self.inner
            .respond(request.with_parameters(self.parameters.clone()))
    }

    async fn profile(&self) -> ModelProfile {
        self.inner.profile().await
    }
}

pub fn provider_candidates_for_model(config: &MayConfig, model: &str) -> Vec<ProviderCandidate> {
    ordered_provider_ids_for_model(config, model)
        .into_iter()
        .map(|provider_id| ProviderCandidate {
            model: model.to_string(),
            provider_id,
        })
        .collect()
}

pub fn provider_candidates_for_tiers(
    config: &MayConfig,
    tiers: &[crate::config::ModelTier],
) -> Vec<ProviderCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::<(String, String)>::new();

    for tier in tiers {
        for model in config.models.models_for_tier(*tier) {
            for candidate in provider_candidates_for_model(config, model) {
                if seen.insert((candidate.model.clone(), candidate.provider_id.clone())) {
                    candidates.push(candidate);
                }
            }
        }
    }

    candidates
}

#[must_use]
pub fn provider_candidate_count(config: &MayConfig, model: &str) -> usize {
    provider_candidates_for_model(config, model).len()
}

pub struct ResolvedProviderRuntime {
    pub provider_id: String,
    pub llm: CloudProvider,
    pub search_embedder: EmbeddingModelResolution,
}

pub async fn resolve_provider_runtime(
    may_config: &MayConfig,
    model: &str,
    provider_attempt: usize,
) -> Result<ResolvedProviderRuntime, MayAgentError> {
    let provider_candidates = provider_candidates_for_model(may_config, model);
    let candidate_count = provider_candidates.len();
    if candidate_count == 0 {
        return Err(MayAgentError::MissingApiKey(err_no_provider(model)));
    }
    let start_index = provider_attempt % candidate_count;
    let mut last_provider_error: Option<MayAgentError> = None;

    let mut selected_provider: Option<(String, CloudProvider)> = None;
    for offset in 0..candidate_count {
        let index = (start_index + offset) % candidate_count;
        let provider_id = provider_candidates[index].provider_id.clone();
        let Some(provider_instance) = may_config.provider.get(&provider_id).cloned() else {
            continue;
        };

        if let Err(error) = validate_generic_provider_model_support(
            may_config,
            &provider_id,
            &provider_instance,
            model,
        )
        .await
        {
            tracing::warn!(
                model,
                provider_id,
                error = %error,
                "provider candidate rejected during model support validation"
            );
            last_provider_error = Some(error);
            continue;
        }

        match build_cloud_provider_from_instance(&provider_id, &provider_instance, model).await {
            Ok(llm) => {
                selected_provider = Some((provider_id, llm));
                break;
            }
            Err(error) => {
                tracing::warn!(
                    model,
                    provider_id,
                    error = %error,
                    "failed to initialize provider candidate for model"
                );
                last_provider_error = Some(error);
            }
        }
    }

    let (provider_id, llm) = if let Some(selected) = selected_provider {
        selected
    } else {
        return Err(last_provider_error
            .unwrap_or_else(|| MayAgentError::MissingApiKey(err_no_provider(model))));
    };

    let search_embedder =
        EmbeddingModelResolution::from_result(resolve_embedding_model(may_config).await);
    match &search_embedder {
        EmbeddingModelResolution::Ready(resolved) => {
            tracing::debug!(
                embedding_model = resolved.model_id(),
                provider_ids = %resolved.provider_ids().join(", "),
                "resolved embedding provider for semantic search"
            );
        }
        EmbeddingModelResolution::Disabled => {}
        EmbeddingModelResolution::Failed(error) => {
            tracing::debug!(
                error = %error,
                "semantic search embedder resolution failed"
            );
        }
    }

    Ok(ResolvedProviderRuntime {
        provider_id,
        llm,
        search_embedder,
    })
}

async fn validate_generic_provider_model_support(
    config: &MayConfig,
    provider_id: &str,
    instance: &ProviderInstance,
    model: &str,
) -> Result<(), MayAgentError> {
    if !instance.is_generic_model_proxy(provider_id) {
        return Ok(());
    }

    let fetched = crate::models::fetch_models_for_provider_instance(provider_id, instance)
        .await
        .map_err(|error| {
            let error_text = error.to_string();
            MayAgentError::Llm(join_text(&[
                "Failed to validate model '",
                model,
                "' for provider '",
                provider_id,
                "' via /models: ",
                error_text.as_str(),
            ]))
        })?;

    let mode = MatchMode::from_strict_flag(config.preferences.strict_model_matching);
    let has_match = fetched
        .iter()
        .any(|candidate| model_ids_match(candidate.id.as_str(), model, mode));

    if !has_match {
        return Err(MayAgentError::Llm(join_text(&[
            "Provider '",
            provider_id,
            "' does not advertise model '",
            model,
            "' in /models",
        ])));
    }

    Ok(())
}

/// May agent error.
#[derive(Debug, thiserror::Error)]
pub enum MayAgentError {
    #[error("Config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Sandbox error: {0}")]
    Sandbox(String),
    #[error("Agent error: {0}")]
    Agent(#[from] AgentError),
    #[error("Missing API key: {0}")]
    MissingApiKey(String),
    #[error("Copilot token error: {0}")]
    CopilotToken(#[from] crate::copilot::CopilotTokenError),
    #[error("Workspace config error: {0}")]
    Workspace(#[from] crate::workspace::WorkspaceConfigError),
    #[error("MCP error: {0}")]
    Mcp(#[from] crate::mcp::McpConnectionLoadError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission request mode: {value}")]
pub struct PermissionRequestModeParseError {
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRequestMode {
    Default,
    Unsafe,
    Ssh,
}

impl PermissionRequestMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Unsafe => "unsafe",
            Self::Ssh => "ssh",
        }
    }
}

impl std::fmt::Display for PermissionRequestMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for PermissionRequestMode {
    type Err = PermissionRequestModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Default),
            "unsafe" => Ok(Self::Unsafe),
            "ssh" => Ok(Self::Ssh),
            other => Err(PermissionRequestModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

impl From<TerminalMode> for PermissionRequestMode {
    fn from(value: TerminalMode) -> Self {
        match value {
            TerminalMode::Sandboxed | TerminalMode::Network => Self::Default,
            TerminalMode::Unsafe => Self::Unsafe,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionHistoryMode {
    Sandboxed,
    Network,
    Unsafe,
}

impl PermissionHistoryMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandboxed => "sandboxed",
            Self::Network => "network",
            Self::Unsafe => "unsafe",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission history mode: {value}")]
pub struct PermissionHistoryModeParseError {
    value: String,
}

impl std::fmt::Display for PermissionHistoryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for PermissionHistoryMode {
    type Err = PermissionHistoryModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sandboxed" => Ok(Self::Sandboxed),
            "network" => Ok(Self::Network),
            "unsafe" => Ok(Self::Unsafe),
            other => Err(PermissionHistoryModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

impl From<TerminalMode> for PermissionHistoryMode {
    fn from(value: TerminalMode) -> Self {
        match value {
            TerminalMode::Sandboxed => Self::Sandboxed,
            TerminalMode::Network => Self::Network,
            TerminalMode::Unsafe => Self::Unsafe,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub mode: PermissionRequestMode,
    pub script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalMode {
    Interactive,
    Unavailable { context: &'static str },
}

#[derive(Debug, Clone)]
pub struct PermissionApprover {
    live_responses: Arc<LiveResponseRegistry<bool>>,
    session_id: Option<String>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    approval_mode: ApprovalMode,
}

impl PermissionApprover {
    #[must_use]
    pub fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            session_id: None,
            runtime_service: None,
            approval_mode: ApprovalMode::Interactive,
        }
    }

    pub fn for_session(&self, session_id: impl Into<String>) -> Self {
        Self {
            live_responses: self.live_responses.clone(),
            session_id: Some(session_id.into()),
            runtime_service: self.runtime_service.clone(),
            approval_mode: self.approval_mode,
        }
    }

    #[must_use]
    pub fn with_runtime_service(
        mut self,
        runtime_service: Arc<crate::session::SessionRuntimeService>,
    ) -> Self {
        self.runtime_service = Some(runtime_service);
        self
    }

    #[must_use]
    pub fn runtime_service(&self) -> Option<Arc<crate::session::SessionRuntimeService>> {
        self.runtime_service.clone()
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    #[must_use]
    pub fn without_interactive_approval(mut self, context: &'static str) -> Self {
        self.approval_mode = ApprovalMode::Unavailable { context };
        self
    }

    fn unavailable_error(mode: TerminalMode, context: &'static str) -> PermissionError {
        PermissionError::Denied(join_text(&[
            "sandbox blocked ",
            mode.description(),
            " execution; approval is required to escalate this command, but approval is unavailable in ",
            context,
        ]))
    }

    fn persisted_request(
        request_id: String,
        payload: PermissionRequest,
    ) -> crate::session::PersistedPermissionRequest {
        crate::session::PersistedPermissionRequest {
            id: request_id,
            request: crate::session::PersistedPermissionRequestPayload {
                mode: payload.mode,
                script: payload.script,
                session_id: payload.session_id,
            },
        }
    }

    async fn append_request_history(
        db: &crate::db::ChatDb,
        session_id: &str,
        request_id: &str,
        mode: TerminalMode,
        script: &str,
    ) -> Result<(), PermissionError> {
        let content = serde_json::to_string(&crate::session::history::PermissionRequestData {
            id: request_id.to_string(),
            mode: PermissionHistoryMode::from(mode),
            script: script.to_string(),
        })
        .map_err(anyhow::Error::from)
        .map_err(PermissionError::Internal)?;

        db.append_message(
            session_id,
            MessageRole::PermissionRequest,
            &content,
            None,
            None,
            None,
        )
        .await
        .map_err(anyhow::Error::from)
        .map_err(PermissionError::Internal)?;
        Ok(())
    }

    pub async fn request_and_wait(
        &self,
        mode: TerminalMode,
        script: String,
    ) -> Result<bool, PermissionError> {
        if let ApprovalMode::Unavailable { context } = self.approval_mode {
            return Err(Self::unavailable_error(mode, context));
        }

        let Some(session_id) = self.session_id.clone() else {
            return Err(PermissionError::Internal(anyhow::anyhow!(
                "permission approval requires a bound session id"
            )));
        };

        if mode == TerminalMode::Network
            && let Some(runtime_service) = &self.runtime_service
            && runtime_service
                .db()
                .session_has_approved_permission_mode(
                    &session_id,
                    PermissionHistoryMode::from(mode),
                )
                .await
                .map_err(anyhow::Error::from)
                .map_err(PermissionError::Internal)?
        {
            return Ok(true);
        }

        let payload = PermissionRequest {
            mode: mode.into(),
            script,
            session_id: Some(session_id.clone()),
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let rx = self.live_responses.open(request_id.clone());

        if let Some(runtime_service) = self.runtime_service.clone() {
            runtime_service
                .set_waiting_permission(
                    &session_id,
                    Some(Self::persisted_request(request_id.clone(), payload.clone())),
                )
                .await
                .map_err(anyhow::Error::from)
                .map_err(PermissionError::Internal)?;
            let db = runtime_service.db();
            Self::append_request_history(&db, &session_id, &request_id, mode, &payload.script)
                .await?;
        }

        rx.recv().await.map_err(|_| PermissionError::Interrupted)
    }

    #[must_use]
    pub fn respond(&self, request_id: &str, approved: bool) -> bool {
        self.live_responses.resolve(request_id, approved)
    }
}

impl Default for PermissionApprover {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionHandler for PermissionApprover {
    async fn check(&self, mode: TerminalMode, script: &str) -> Result<bool, PermissionError> {
        match mode {
            TerminalMode::Sandboxed | TerminalMode::Network => Ok(true),
            TerminalMode::Unsafe => {
                let approved = self.request_and_wait(mode, script.to_string()).await?;
                if approved {
                    Ok(true)
                } else {
                    Err(PermissionError::Denied(join_text(&[
                        mode.description(),
                        " mode denied for script",
                    ])))
                }
            }
        }
    }

    async fn check_domain(&self, _domain: &str, _port: u16) -> bool {
        true
    }

    async fn will_wait_for_approval(&self, mode: TerminalMode, _script: &str) -> bool {
        if matches!(self.approval_mode, ApprovalMode::Unavailable { .. }) {
            return false;
        }
        match mode {
            TerminalMode::Sandboxed | TerminalMode::Network => false,
            TerminalMode::Unsafe => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingSteerHook {
    db: Option<crate::db::ChatDb>,
    session_id: String,
}

impl PendingSteerHook {
    fn new(db: Option<crate::db::ChatDb>, session_id: &SessionId) -> Self {
        Self {
            db,
            session_id: session_id.as_str().to_string(),
        }
    }
}

impl Hook for PendingSteerHook {
    async fn on_turn_boundary(&self, _ctx: &TurnBoundaryContext<'_>) -> TurnBoundaryAction {
        let Some(db) = &self.db else {
            return TurnBoundaryAction::Continue;
        };
        let Ok(runtime) = db.get_session_runtime(&self.session_id).await else {
            tracing::warn!(session_id = %self.session_id, "failed to inspect runtime for steer boundary");
            return TurnBoundaryAction::Continue;
        };
        if runtime.is_some_and(|summary| summary.has_pending_immediate()) {
            TurnBoundaryAction::EndTurn
        } else {
            TurnBoundaryAction::Continue
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextSnapshotHook {
    db: Option<crate::db::ChatDb>,
    session_id: String,
}

impl ContextSnapshotHook {
    fn new(db: Option<crate::db::ChatDb>, session_id: &SessionId) -> Self {
        Self {
            db,
            session_id: session_id.as_str().to_string(),
        }
    }
}

impl Hook for ContextSnapshotHook {
    async fn on_checkpoint(&self, ctx: &CheckpointContext<'_>) -> Option<String> {
        let Some(db) = &self.db else {
            return None;
        };
        let record = match crate::session::ContextSnapshotRecord::from_checkpoint(ctx) {
            Ok(record) => match db.get_session_runtime(&self.session_id).await {
                Ok(Some(runtime)) => record.with_terminal_runtime(&runtime),
                Ok(None) => record,
                Err(error) => return Some(error.to_string()),
            },
            Err(error) => return Some(error.to_string()),
        };
        match crate::session::persist_context_snapshot_record(db, &self.session_id, record).await {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        }
    }
}

/// Execution environment for a `MayAgent`.
pub enum AgentEnvironment {
    /// Native macOS sandbox via heel.
    Local,
    /// Local home environment with one designated remote SSH target.
    Remote {
        /// The configured SSH target bound to this profile.
        ssh_server: crate::config::SshServerConfig,
    },
    /// Docker container-based isolation.
    Container {
        /// Container ID from Docker.
        container_id: String,
        /// Hostname reachable from the container for IPC back to the host.
        container_ipc_host: String,
        /// Executor for running commands inside the container.
        container_exec: ContainerExecHandle,
        /// Optional browser backend that can provide a CDP endpoint.
        browser_backend: Option<Arc<dyn BrowserBackend>>,
    },
}

/// Configuration for creating a `MayAgent`.
pub struct MayAgentConfig {
    /// The LLM provider.
    pub llm: CloudProvider,
    /// The concrete model identifier selected for this request.
    pub model_id: String,
    /// Provider ID selected for this request.
    pub provider_id: String,
    /// Optional embedding provider used for semantic retrieval tools.
    pub search_embedder: EmbeddingModelResolution,
    /// Session sandbox directory (`TerminalTool` creates working dir inside).
    pub sandbox_dir: PathBuf,
    /// May directory (~/.may/).
    pub may_dir: PathBuf,
    /// Current working directory (where May was launched).
    pub cwd: PathBuf,
    /// Session ID.
    pub session_id: SessionId,
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    /// Maximum iterations.
    pub max_iterations: usize,
    /// Channel for workspace access requests.
    pub workspace_request_broker: WorkspaceRequestBroker,
    /// Channel for ask user requests.
    pub ask_user_broker: AskUserBroker,
    /// Image generation provider and model.
    pub image_generator: Option<ImageGeneratorConfig>,
    /// Preconfigured SSH servers available for `terminal` ssh mode.
    pub ssh_servers: Vec<crate::config::SshServerConfig>,
    /// Optional web UI permission approver.
    pub permission_approver: Option<PermissionApprover>,
    /// Execution environment (local heel sandbox or container).
    pub environment: AgentEnvironment,
    /// Optional profile persona overlay injected into the agent context.
    pub profile_persona: Option<String>,
    /// Optional profile memories (preference + recency) injected as read-only context.
    pub profile_memories: Option<crate::memory::ProfileMemories>,
    /// Session-scoped planning workflow state.
    pub plan_state: SessionPlanState,
    /// Global network restriction policy.
    pub network_policy: Option<CompiledNetworkPolicy>,
    /// Optional location info for location-aware profiles.
    pub location: Option<crate::location::LocationInfo>,
    /// Optional broker for `task` command requests.
    pub task_broker: Option<crate::agent::task_tool::TaskBroker>,
    /// Optional broker for `notify` command requests.
    pub notify_broker: Option<crate::agent::notify_tool::NotifyBroker>,
    /// Optional broker for `browser` command requests.
    pub browser_broker: Option<BrowserBroker>,
    /// Optional broker for `register-skills` command requests.
    pub register_skills_broker: Option<RegisterBroker>,
    /// Optional broker for `register-subagents` command requests.
    pub register_subagents_broker: Option<RegisterBroker>,
    /// Whether this model/runtime pair supports native voice input.
    pub native_voice_input: bool,
    /// Model context window resolved from provider metadata.
    pub model_context_window: Option<u32>,
    /// Request-level parameters applied to each LLM call for this agent.
    pub request_parameters: Parameters,
}

#[derive(Clone)]
pub struct ImageGeneratorConfig {
    pub provider_id: String,
    pub provider: ProviderInstance,
    pub model: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct ImagineArgs {
    /// Prompt describing the image to generate.
    pub prompt: String,
    /// Image size as `WIDTHxHEIGHT`, for example 1024x1024.
    pub size: String,
}

#[derive(Clone)]
struct ImagineTool {
    generator: Option<ImageGeneratorConfig>,
    artifacts_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
enum ImagineToolError {
    #[error("no image_generation model configured")]
    MissingGenerator,
    #[error("selected provider does not support image generation")]
    UnsupportedProvider,
    #[error("image generation returned no data")]
    EmptyResponse,
    #[error("invalid size format: expected WIDTHxHEIGHT")]
    InvalidSizeFormat,
    #[error("invalid width in size")]
    InvalidWidth,
    #[error("invalid height in size")]
    InvalidHeight,
    #[error("size must be greater than 0")]
    NonPositiveSize,
    #[error("{0}")]
    MissingApiKey(String),
    #[error("failed to create image provider: {0}")]
    BuildProvider(String),
    #[error("image generation failed: {0}")]
    Generate(String),
    #[error("failed to prepare artifacts directory: {0}")]
    CreateArtifactsDir(std::io::Error),
    #[error("failed to write generated image: {0}")]
    WriteImage(std::io::Error),
}

#[derive(Clone)]
struct RuntimeSystemContext {
    memory_path: PathBuf,
    last_memory_content: String,
    terminal_runtime: Option<TerminalRuntimeContext>,
    workspace: Option<Workspace>,
    remote_environment: Option<RemoteEnvironmentContext>,
    container_environment: Option<ContainerEnvironmentContext>,
    browser: Option<Browser>,
    profile_persona: Option<ProfilePersona>,
    profile_memories: Option<ProfileMemories>,
    plan_mode: Option<PlanMode>,
    network_restrictions: Option<NetworkRestrictions>,
    location: Option<LocationContext>,
    native_voice_input: Option<NativeVoiceInput>,
    platform: Option<Platform>,
}

impl RuntimeSystemContext {
    const fn new(memory_path: PathBuf, memory_content: String) -> Self {
        Self {
            memory_path,
            last_memory_content: memory_content,
            terminal_runtime: None,
            workspace: None,
            remote_environment: None,
            container_environment: None,
            browser: None,
            profile_persona: None,
            profile_memories: None,
            plan_mode: None,
            network_restrictions: None,
            location: None,
            native_voice_input: None,
            platform: None,
        }
    }

    fn with_terminal_runtime(mut self, terminal_runtime: TerminalRuntimeContext) -> Self {
        self.terminal_runtime = Some(terminal_runtime);
        self
    }

    fn with_workspace(mut self, workspace: Workspace) -> Self {
        self.workspace = Some(workspace);
        self
    }

    fn with_remote_environment(mut self, remote_environment: RemoteEnvironmentContext) -> Self {
        self.remote_environment = Some(remote_environment);
        self
    }

    fn with_container_environment(
        mut self,
        container_environment: ContainerEnvironmentContext,
    ) -> Self {
        self.container_environment = Some(container_environment);
        self
    }

    fn with_browser(mut self, browser: Browser) -> Self {
        self.browser = Some(browser);
        self
    }

    fn with_profile_persona(mut self, profile_persona: ProfilePersona) -> Self {
        self.profile_persona = Some(profile_persona);
        self
    }

    fn with_profile_memories(mut self, profile_memories: ProfileMemories) -> Self {
        self.profile_memories = Some(profile_memories);
        self
    }

    fn with_plan_mode(mut self, plan_mode: PlanMode) -> Self {
        self.plan_mode = Some(plan_mode);
        self
    }

    fn with_network_restrictions(mut self, network_restrictions: NetworkRestrictions) -> Self {
        self.network_restrictions = Some(network_restrictions);
        self
    }

    fn with_location(mut self, location: LocationContext) -> Self {
        self.location = Some(location);
        self
    }

    fn with_native_voice_input(mut self, native_voice_input: NativeVoiceInput) -> Self {
        self.native_voice_input = Some(native_voice_input);
        self
    }

    fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = Some(platform);
        self
    }

    fn reconcile(&mut self, context: &mut aither_agent::Context) {
        let memory = build_memory_context(&self.last_memory_content);
        reconcile_system_block(context, Some(&memory));
        reconcile_system_block(context, self.terminal_runtime.as_ref());
        reconcile_system_block(context, self.workspace.as_ref());
        reconcile_system_block(context, self.remote_environment.as_ref());
        reconcile_system_block(context, self.container_environment.as_ref());
        reconcile_system_block(context, self.browser.as_ref());
        reconcile_system_block(context, self.profile_persona.as_ref());
        reconcile_system_block(context, self.profile_memories.as_ref());
        reconcile_system_block(context, self.plan_mode.as_ref());
        reconcile_system_block(context, self.network_restrictions.as_ref());
        reconcile_system_block(context, self.location.as_ref());
        reconcile_system_block(context, self.native_voice_input.as_ref());
        reconcile_system_block(context, self.platform.as_ref());
    }

    async fn sync_memory_file(&mut self) {
        let memory_content = async_fs::read_to_string(&self.memory_path)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "failed to read memory file at {}: {error}",
                    self.memory_path.display()
                )
            });
        self.last_memory_content = memory_content;
    }
}

fn reconcile_system_block<T: serde::Serialize>(
    context: &mut aither_agent::Context,
    value: Option<&T>,
) {
    match value {
        Some(value) => context.insert_system(value),
        None => context.remove_system::<T>(),
    }
}

impl Tool for ImagineTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("imagine")
    }

    type Arguments = ImagineArgs;
    type Res = ToolResult;

    async fn call(&self, arguments: Self::Arguments) -> aither_core::Result<Self::Res> {
        match self.generate_image(arguments).await {
            Ok(artifact_path) => Ok(ToolResult::text(artifact_path)),
            Err(error) => Ok(ToolResult::error(error.to_string())),
        }
    }
}

impl ImagineTool {
    async fn generate_image(&self, arguments: ImagineArgs) -> Result<String, ImagineToolError> {
        let generator = self
            .generator
            .as_ref()
            .ok_or(ImagineToolError::MissingGenerator)?;

        let size = parse_imagine_size(&arguments.size)?;
        let prompt = Prompt::new(arguments.prompt);

        let image_provider = build_image_provider(generator)?;

        let mut latest = None;
        match image_provider {
            CloudProvider::OpenAI(provider) => {
                let mut stream = std::pin::pin!(provider.create(prompt, size));
                while let Some(chunk) = stream.next().await {
                    latest = Some(chunk.map_err(|error| {
                        ImagineToolError::Generate(error.to_string())
                    })?);
                }
            }
            CloudProvider::Gemini(provider) => {
                let mut stream = std::pin::pin!(provider.create(prompt, size));
                while let Some(chunk) = stream.next().await {
                    latest = Some(chunk.map_err(|error| {
                        ImagineToolError::Generate(error.to_string())
                    })?);
                }
            }
            _ => return Err(ImagineToolError::UnsupportedProvider),
        }

        let image = latest.ok_or(ImagineToolError::EmptyResponse)?;
        let mut filename = uuid::Uuid::new_v4().to_string();
        filename.push_str(".png");
        let path = self.artifacts_dir.join(&filename);
        async_fs::create_dir_all(&self.artifacts_dir)
            .await
            .map_err(ImagineToolError::CreateArtifactsDir)?;
        async_fs::write(&path, image)
            .await
            .map_err(ImagineToolError::WriteImage)?;

        let mut artifact_path = String::with_capacity(filename.len() + 10);
        artifact_path.push_str("artifacts/");
        artifact_path.push_str(filename.as_str());
        Ok(artifact_path)
    }
}

fn parse_imagine_size(raw: &str) -> Result<Size, ImagineToolError> {
    let value = raw.trim();
    let Some((w, h)) = value.split_once('x') else {
        return Err(ImagineToolError::InvalidSizeFormat);
    };

    let width: u32 = w
        .trim()
        .parse()
        .map_err(|_| ImagineToolError::InvalidWidth)?;
    let height: u32 = h
        .trim()
        .parse()
        .map_err(|_| ImagineToolError::InvalidHeight)?;

    if width == 0 || height == 0 {
        return Err(ImagineToolError::NonPositiveSize);
    }

    Ok(Size::new(width, height))
}

fn build_image_provider(config: &ImageGeneratorConfig) -> Result<CloudProvider, ImagineToolError> {
    let provider_type = config.provider.resolve_type(&config.provider_id);
    let api_key = config.provider.api_key.as_deref().unwrap_or("");
    let base_url = config.provider.effective_base_url(&config.provider_id);

    if provider_type.requires_api_key() && api_key.is_empty() {
        return Err(ImagineToolError::MissingApiKey(err_api_key_required(
            &config.provider_id,
        )));
    }

    let provider = if provider_type == ProviderType::Google {
        let mut client = Gemini::new(api_key).with_image_model(&config.model);
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        CloudProvider::Gemini(client)
    } else {
        let mut client = OpenAI::new(api_key).with_image_model(&config.model);
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        CloudProvider::OpenAI(client)
    };

    Ok(provider)
}

fn resolve_image_generator_config(may_config: &MayConfig) -> Option<ImageGeneratorConfig> {
    let model = may_config
        .default_model_for_tier(crate::config::ModelTier::ImageGeneration)?
        .to_string();

    let provider_id = may_config
        .models
        .provider_for_model(&model, may_config.preferences.strict_model_matching)
        .map(std::string::ToString::to_string)
        .or_else(|| {
            let resolver = may_config.create_resolver();
            resolver
                .select_provider(&model)
                .map(|selected| selected.id.to_string())
        })?;

    let provider = may_config.provider.get(&provider_id)?.clone();

    Some(ImageGeneratorConfig {
        provider_id,
        provider,
        model,
    })
}

/// The May agent.
pub struct MayAgent {
    inner: Agent<
        RequestParameterizedModel<CloudProvider>,
        RequestParameterizedModel<CloudProvider>,
        RequestParameterizedModel<CloudProvider>,
        HCons<PendingSteerHook, HCons<ContextSnapshotHook, ()>>,
    >,
    session_id: SessionId,
    sandbox_dir: PathBuf,
    provider_id: String,
    model_context_window: Option<u32>,
    runtime_system_context: RuntimeSystemContext,
}

impl MayAgent {
    /// Create a new `MayAgent`.
    pub async fn new(config: MayAgentConfig) -> Result<Self, MayAgentError> {
        let is_container = matches!(config.environment, AgentEnvironment::Container { .. });

        let executor = DefaultExecutor;

        // Create the terminal command tool in the sandbox dir.
        let terminal_tool: TerminalTool<TerminalPermissionHandler, DefaultExecutor> =
            TerminalTool::new_exact(
                &config.sandbox_dir,
                TerminalPermissionHandler::for_agent(
                    config.permission_approver.clone(),
                    config.network_policy.clone(),
                    config.profile_id.clone(),
                ),
                executor,
            )
            .await
            .map_err(|e| MayAgentError::Sandbox(e.to_string()))?;

        let session_paths = SessionPaths::new(config.sandbox_dir.clone());

        // Create workspace with permissions (only used for local environment context)
        let workspace = MayWorkspace::new(
            config.sandbox_dir.clone(),
            config.cwd.clone(),
            session_paths.workspace_config(),
        )?;

        let ssh_servers: Vec<SshServer> = match &config.environment {
            AgentEnvironment::Local | AgentEnvironment::Container { .. } => config
                .ssh_servers
                .iter()
                .map(|s| SshServer {
                    name: s.name.clone(),
                    target: s.target.clone(),
                })
                .collect(),
            AgentEnvironment::Remote { ssh_server } => vec![SshServer {
                name: ssh_server.name.clone(),
                target: ssh_server.target.clone(),
            }],
        };
        let ssh_enabled = !ssh_servers.is_empty();

        // Read global memory from ~/.may/MEMORY.md.
        let memory_path = global_memory_file_path(&config.may_dir);
        let memory_content = async_fs::read_to_string(&memory_path).await?;
        let mut runtime_system_context =
            RuntimeSystemContext::new(memory_path.clone(), memory_content.clone());
        let terminal_runtime = build_terminal_runtime_context(
            &config,
            &session_paths,
            aither_models::lookup(config.model_id.as_str()),
        );
        runtime_system_context =
            runtime_system_context.with_terminal_runtime(terminal_runtime.clone());

        let llm = RequestParameterizedModel::new(config.llm, config.request_parameters.clone());

        // Clone LLM for terminal IPC commands before moving it into the builder.
        let terminal_command_llm = llm.clone();

        // Create tools
        let ssh_authorizer_broker = config.ask_user_broker.clone();
        let ask_user = AskUserTool::new(
            config.session_id.as_str().to_string(),
            config.ask_user_broker,
        );
        let imagine = ImagineTool {
            generator: config.image_generator,
            artifacts_dir: session_paths.artifacts_dir(),
        };
        let runtime_db = config
            .permission_approver
            .as_ref()
            .and_then(|approver| approver.runtime_service.as_ref())
            .map(|runtime_service| runtime_service.db());
        let pending_steer_hook = PendingSteerHook::new(runtime_db.clone(), &config.session_id);
        let context_snapshot_hook = ContextSnapshotHook::new(runtime_db, &config.session_id);

        let (skills_dir, subagents_dir) = if let Some(profile_id) = config.profile_id.as_deref() {
            let profile_home =
                crate::session::MayHomePaths::new(config.may_dir.clone()).profile_home(profile_id);
            (profile_home.skills_dir(), profile_home.subagents_dir())
        } else {
            let home = crate::session::MayHomePaths::new(config.may_dir.clone());
            (home.skills_dir(), home.subagents_dir())
        };

        // Shell runtime depends on the environment
        let availability = match &config.environment {
            AgentEnvironment::Container { .. } => ShellRuntimeAvailability {
                local: false,
                container: true,
                ssh: false,
            },
            AgentEnvironment::Remote { .. } | AgentEnvironment::Local => ShellRuntimeAvailability {
                local: true,
                container: false,
                ssh: ssh_enabled,
            },
        };

        // Build the agent using a minimal base prompt + structured context blocks.
        // Skills and subagents must be registered before with_default_prompt()
        // so they appear in the system prompt.
        let effective_max_iterations = if config.max_iterations == 0 {
            usize::MAX
        } else {
            config.max_iterations
        };
        let mut builder = TerminalAgentBuilder::new(llm, terminal_tool)
            .tool(ask_user)
            .tool(imagine)
            .hook(context_snapshot_hook)
            .hook(pending_steer_hook);
        let terminal_command_factory = builder.terminal_tool_factory();
        let shared_terminal_commands = register_shared_terminal_commands(
            builder.tool_registry_mut(),
            SharedTerminalCommandConfig {
                llm: terminal_command_llm,
                session_id: config.session_id.as_str().to_string(),
                may_dir: config.may_dir.clone(),
                sandbox_dir: config.sandbox_dir.clone(),
                search_embedder: config.search_embedder.clone(),
                task_runtime: match &config.environment {
                    AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                    AgentEnvironment::Remote { .. } => TaskRuntimeKind::Remote,
                    AgentEnvironment::Local => TaskRuntimeKind::Local,
                },
                task_broker: config.task_broker.clone(),
                notify_broker: config.notify_broker.clone(),
                browser_broker: config.browser_broker.clone(),
                register_skills_broker: config.register_skills_broker.clone(),
                register_subagents_broker: config.register_subagents_broker.clone(),
                workspace_request_broker: (!is_container)
                    .then_some(config.workspace_request_broker.clone()),
                terminal_tool_factory: Some(terminal_command_factory),
            },
        )
        .await?;
        for (name, description) in &shared_terminal_commands {
            builder = builder.tool_description(name.clone(), description.clone());
        }
        if is_container {
            builder = builder.with_skills_readable_only(&skills_dir).await?;
            builder = builder.with_subagents_readable_only(&subagents_dir).await?;
        } else {
            builder = builder.with_skills(&skills_dir).await?;
            builder = builder.with_subagents(&subagents_dir).await?;
        }
        let base_system_prompt =
            build_base_system_prompt(&shared_terminal_commands, &skills_dir, &subagents_dir)
                .await?;
        builder = builder
            .system_prompt_raw(base_system_prompt)
            .shell_runtime_availability(availability)
            .ssh_servers(ssh_servers)
            .ssh_authorizer(super::ssh_authorizer::MaySshAuthorizer::new(
                config.session_id.as_str().to_string(),
                ssh_authorizer_broker,
            ))
            .agent_kind(AgentKind::Chatbot)
            .persona_prompt(include_str!("../../templates/system.md"))
            .system(build_memory_context(&memory_content))
            .system(terminal_runtime)
            .max_iterations(effective_max_iterations);

        if let Some(voice_hint) = build_native_voice_context(config.native_voice_input) {
            runtime_system_context =
                runtime_system_context.with_native_voice_input(voice_hint.clone());
            builder = builder.system(voice_hint);
        }

        if let Some(ref persona) = config.profile_persona {
            let profile_persona = ProfilePersona {
                content: persona.clone(),
            };
            runtime_system_context =
                runtime_system_context.with_profile_persona(profile_persona.clone());
            builder = builder.system(profile_persona);
        }

        if let Some(ref memories) = config.profile_memories
            && !memories.is_empty()
        {
            let profile_memories = ProfileMemories {
                content: memories.to_context_string(),
            };
            runtime_system_context =
                runtime_system_context.with_profile_memories(profile_memories.clone());
            builder = builder.system(profile_memories);
        }

        if let Some(plan_mode) = build_plan_mode_context(config.plan_state) {
            runtime_system_context = runtime_system_context.with_plan_mode(plan_mode.clone());
            builder = builder.system(plan_mode);
        }

        if let Some(network_restrictions) =
            build_network_restrictions_context(config.network_policy.as_ref())
        {
            runtime_system_context =
                runtime_system_context.with_network_restrictions(network_restrictions.clone());
            builder = builder.system(network_restrictions);
        }

        if let Some(ref location) = config.location {
            let location_context = build_location_context(location);
            runtime_system_context = runtime_system_context.with_location(location_context.clone());
            builder = builder.system(location_context);
        }

        let browser_enabled = config.browser_broker.is_some()
            || matches!(
                config.environment,
                AgentEnvironment::Container {
                    browser_backend: Some(_),
                    ..
                }
            );
        let mut browser_cdp_endpoint = None;

        // Environment-specific configuration
        match config.environment {
            AgentEnvironment::Container {
                container_id,
                container_ipc_host,
                container_exec,
                browser_backend,
            } => {
                let container_context = build_container_environment_context(&config.cwd);
                runtime_system_context =
                    runtime_system_context.with_container_environment(container_context.clone());
                builder = builder.system(container_context);
                builder = builder.container_runtime(ContainerShellRuntime::new(
                    container_id,
                    container_ipc_host,
                    container_exec,
                ));
                if let Some(browser_backend) = browser_backend {
                    let endpoint = browser_backend.cdp_endpoint().await.map_err(|error| {
                        MayAgentError::Sandbox(prefixed_error(
                            "failed to resolve browser CDP endpoint from backend: ",
                            &error,
                        ))
                    })?;
                    browser_cdp_endpoint = Some(endpoint);
                }
            }
            AgentEnvironment::Local => {
                let workspace_context = build_workspace_context(&workspace);
                runtime_system_context =
                    runtime_system_context.with_workspace(workspace_context.clone());
                builder = builder.system(workspace_context);
                if let Some(macos_hint) = build_macos_host_automation_context() {
                    runtime_system_context =
                        runtime_system_context.with_platform(macos_hint.clone());
                    builder = builder.system(macos_hint);
                }
            }
            AgentEnvironment::Remote { ssh_server } => {
                let remote_context = build_remote_environment_context(&workspace, &ssh_server);
                runtime_system_context =
                    runtime_system_context.with_remote_environment(remote_context.clone());
                builder = builder.system(remote_context);
            }
        }

        let browser_context =
            build_browser_context(browser_enabled, browser_cdp_endpoint.as_deref());
        runtime_system_context = runtime_system_context.with_browser(browser_context.clone());
        builder = builder.system(browser_context);

        // Get the actual working directory before building
        let actual_sandbox_dir = PathBuf::from(builder.sandbox_dir().as_ref());

        let agent = builder.build();

        Ok(Self {
            inner: agent,
            session_id: config.session_id,
            sandbox_dir: actual_sandbox_dir,
            provider_id: config.provider_id,
            model_context_window: config.model_context_window,
            runtime_system_context,
        })
    }

    /// Create a `MayAgent` from config file and session.
    pub async fn from_config(
        may_config: &MayConfig,
        sandbox_dir: PathBuf,
        may_dir: PathBuf,
        cwd: PathBuf,
        session_id: SessionId,
        workspace_request_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
    ) -> Result<Self, MayAgentError> {
        // Get model: persisted default first, otherwise first Advanced model
        let model = may_config
            .last_model
            .as_deref()
            .or_else(|| may_config.models.advanced.first().map(String::as_str))
            .ok_or_else(|| MayAgentError::MissingApiKey(ERR_NO_MODELS.trim().to_string()))?;

        Self::from_config_with_model(
            may_config,
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            workspace_request_broker,
            ask_user_broker,
            model,
        )
        .await
    }

    /// Create a `MayAgent` from config file with a specific model.
    ///
    /// Provider selection uses an ordered candidate list:
    /// model-mapped provider first (if configured), then explicit providers,
    /// then generic proxy providers as fallback.
    pub async fn from_config_with_model(
        may_config: &MayConfig,
        sandbox_dir: PathBuf,
        may_dir: PathBuf,
        cwd: PathBuf,
        session_id: SessionId,
        workspace_request_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
        model: &str,
    ) -> Result<Self, MayAgentError> {
        Self::from_config_with_model_and_permissions(
            may_config,
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            workspace_request_broker,
            ask_user_broker,
            model,
            None,
            None,
            None,
            None,
            None,
            SessionPlanState::Disabled,
            None,
            None,
            None,
            None,
            None,
            None,
            AgentEnvironment::Local,
            0,
        )
        .await
    }

    pub async fn from_config_with_model_and_permissions(
        may_config: &MayConfig,
        sandbox_dir: PathBuf,
        may_dir: PathBuf,
        cwd: PathBuf,
        session_id: SessionId,
        workspace_request_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        profile_id: Option<String>,
        permission_approver: Option<PermissionApprover>,
        profile_persona: Option<String>,
        profile_memories: Option<crate::memory::ProfileMemories>,
        plan_state: SessionPlanState,
        location: Option<crate::location::LocationInfo>,
        task_broker: Option<crate::agent::task_tool::TaskBroker>,
        notify_broker: Option<crate::agent::notify_tool::NotifyBroker>,
        browser_broker: Option<BrowserBroker>,
        register_skills_broker: Option<RegisterBroker>,
        register_subagents_broker: Option<RegisterBroker>,
        environment: AgentEnvironment,
        provider_attempt: usize,
    ) -> Result<Self, MayAgentError> {
        let resolved_provider =
            resolve_provider_runtime(may_config, model, provider_attempt).await?;
        let provider_id = resolved_provider.provider_id;
        let llm = resolved_provider.llm;
        let model_info = aither_models::lookup(model);
        let request_parameters = if let Some(reasoning_effort) = reasoning_effort {
            Parameters::default().reasoning_effort(reasoning_effort)
        } else {
            Parameters::default()
        };

        let config = MayAgentConfig {
            llm,
            model_id: model.to_string(),
            provider_id,
            search_embedder: resolved_provider.search_embedder,
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            profile_id,
            max_iterations: may_config.max_iterations,
            workspace_request_broker,
            ask_user_broker,
            image_generator: resolve_image_generator_config(may_config),
            ssh_servers: may_config.ssh_servers.clone(),
            permission_approver,
            environment,
            profile_persona,
            profile_memories,
            plan_state,
            network_policy: may_config
                .preferences
                .network_policy
                .compile()
                .map_err(MayAgentError::Sandbox)?,
            location,
            task_broker,
            notify_broker,
            browser_broker,
            register_skills_broker,
            register_subagents_broker,
            native_voice_input: model_info
                .is_some_and(|info| info.has_ability(aither_core::llm::model::Ability::Audio)),
            model_context_window: model_info.and_then(aither_models::ModelEntry::max_input_tokens),
            request_parameters,
        };

        Self::new(config).await
    }

    /// Run a query and stream events.
    pub fn run(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> impl Stream<Item = Result<AgentEvent, AgentError>> + '_ {
        self.inner.clear_transient_system_messages();
        self.inner.run(prompt, attachments)
    }

    /// Run a query and persist per-turn hidden context as a regular system message.
    pub fn run_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> impl Stream<Item = Result<AgentEvent, AgentError>> + '_ {
        self.inner.clear_transient_system_messages();
        if let Some(hidden_context) = hidden_context
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            self.inner.push_message(LlmMessage::system(hidden_context));
        }
        self.inner.run(prompt, attachments)
    }

    /// One-shot query.
    pub async fn query(&mut self, prompt: &str) -> Result<String, AgentError> {
        self.sync_runtime_context().await;
        self.inner.clear_transient_system_messages();
        self.inner.query(prompt).await
    }

    /// Preview the exact request message sequence for the next turn.
    pub async fn preview_request_messages(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> Vec<LlmMessage> {
        self.sync_runtime_context().await;
        self.inner.clear_transient_system_messages();
        self.inner
            .preview_request_messages(prompt, attachments)
            .await
    }

    /// Preview the exact request message sequence for the next turn with hidden context.
    pub async fn preview_request_messages_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> Vec<LlmMessage> {
        self.sync_runtime_context().await;
        let messages = hidden_context
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| vec![text.to_string()])
            .unwrap_or_default();
        self.inner.set_transient_system_messages(messages);
        let preview = self
            .inner
            .preview_request_messages(prompt, attachments)
            .await;
        self.inner.clear_transient_system_messages();
        preview
    }

    /// Get session ID.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Get session sandbox directory.
    #[must_use]
    pub fn sandbox_dir(&self) -> PathBuf {
        self.sandbox_dir.clone()
    }

    /// Selected provider ID for the current request.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Model context window resolved for this agent.
    #[must_use]
    pub const fn model_context_window(&self) -> Option<u32> {
        self.model_context_window
    }

    pub(crate) async fn export_checkpoint(
        &mut self,
        history_anchor: Option<usize>,
    ) -> Result<aither_agent::AgentCheckpoint, AgentError> {
        self.inner.export_checkpoint(history_anchor).await
    }

    pub(crate) fn push_message(&mut self, message: aither_core::llm::Message) {
        self.inner.push_message(message);
    }

    pub(crate) async fn compact(
        &mut self,
        focus: Option<&str>,
    ) -> Result<Option<CompactResult>, AgentError> {
        self.inner.compact(focus).await
    }

    pub(crate) fn restore_checkpoint(
        &mut self,
        checkpoint: aither_agent::AgentCheckpoint,
    ) -> Result<(), AgentError> {
        self.inner.restore_checkpoint(checkpoint)?;
        self.runtime_system_context
            .reconcile(self.inner.context_mut());
        Ok(())
    }

    /// Clear conversation history.
    pub fn clear_history(&mut self) {
        self.inner.clear_history();
    }

    pub(crate) async fn sync_runtime_context(&mut self) {
        self.runtime_system_context.sync_memory_file().await;
        self.runtime_system_context
            .reconcile(self.inner.context_mut());
    }
}

/// Build a provider chain for fast-tier background memory extraction.
///
/// Uses all configured fast-tier models first, then balanced-tier models.
/// For each model, tries explicit providers before generic proxy providers.
pub async fn build_fast_provider_chain(
    config: &MayConfig,
) -> Result<Vec<crate::memory::MemoryExtractionProvider>, MayAgentError> {
    let candidates = provider_candidates_for_tiers(
        config,
        &[
            crate::config::ModelTier::Fast,
            crate::config::ModelTier::Balanced,
        ],
    );

    if candidates.is_empty() {
        return Err(MayAgentError::MissingApiKey(
            ERR_NO_MODELS.trim().to_string(),
        ));
    }
    let first_model = candidates[0].model.clone();

    let mut built = Vec::new();
    let mut last_error: Option<MayAgentError> = None;

    for candidate in candidates {
        let Some(instance) = config.get_provider(&candidate.provider_id) else {
            continue;
        };

        match build_cloud_provider_from_instance(&candidate.provider_id, instance, &candidate.model)
            .await
        {
            Ok(llm) => built.push(crate::memory::MemoryExtractionProvider {
                provider_id: candidate.provider_id.clone(),
                model: candidate.model.clone(),
                llm,
            }),
            Err(error) => {
                tracing::warn!(
                    provider_id = %candidate.provider_id,
                    model = %candidate.model,
                    error = %error,
                    "failed to initialize memory extraction provider candidate"
                );
                last_error = Some(error);
            }
        }
    }

    if built.is_empty() {
        return Err(last_error
            .unwrap_or_else(|| MayAgentError::MissingApiKey(err_no_provider(&first_model))));
    }

    Ok(built)
}

/// Build a `CloudProvider` from a `ProviderInstance` directly.
async fn build_cloud_provider_from_instance(
    id: &str,
    instance: &ProviderInstance,
    model: &str,
) -> Result<CloudProvider, MayAgentError> {
    let provider_type = instance.resolve_type(id);
    let api_key = instance.api_key.as_deref().unwrap_or("");
    let base_url = instance.effective_base_url(id);

    if provider_type.requires_api_key() && api_key.is_empty() {
        return Err(MayAgentError::MissingApiKey(err_api_key_required(id)));
    }

    let llm: CloudProvider = match provider_type {
        ProviderType::Anthropic => {
            let mut client = Claude::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::Google => {
            let mut client = Gemini::new(api_key).with_text_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::OpenAI => {
            let mut client = OpenAI::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::OpenRouter => OpenAI::openrouter(api_key).with_model(model).into(),
        ProviderType::DeepSeek => OpenAI::deepseek(api_key).with_model(model).into(),
        ProviderType::Copilot => {
            let credentials = match crate::copilot::get_copilot_credentials().await {
                Ok(Some(credentials)) => credentials,
                Ok(None) => {
                    return Err(MayAgentError::MissingApiKey(
                        "GitHub Copilot not authenticated. Use /config to set up OAuth."
                            .to_string(),
                    ));
                }
                Err(e) => {
                    if is_copilot_unauthorized(&e)
                        && let Err(clear_err) = clear_copilot_credentials().await
                    {
                        tracing::warn!(
                            "Failed to clear invalid Copilot credentials: {}",
                            clear_err
                        );
                    }
                    return Err(MayAgentError::CopilotToken(e));
                }
            };

            Copilot::new(credentials.token)
                .with_base_url(credentials.api_endpoint)
                .with_oauth_token(credentials.oauth_token)
                .with_model(model)
                .into()
        }
        ProviderType::Ollama => {
            let url = base_url.unwrap_or("http://localhost:11434/v1");
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .into()
        }
        ProviderType::LMStudio => {
            let url = base_url.unwrap_or("http://127.0.0.1:1234/v1");
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .with_chat_completions_api()
                .into()
        }
        ProviderType::Groq | ProviderType::Together | ProviderType::Mistral | ProviderType::XAI => {
            let mut client = OpenAI::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::Custom => {
            let url = base_url.ok_or_else(|| {
                MayAgentError::MissingApiKey(quoted_name_error(
                    "Base URL required for custom provider ",
                    id,
                    "",
                ))
            })?;
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .with_chat_completions_api()
                .into()
        }
    };

    Ok(llm)
}

const fn is_copilot_unauthorized(err: &crate::copilot::CopilotTokenError) -> bool {
    matches!(
        err,
        crate::copilot::CopilotTokenError::Exchange(
            aither_copilot::CopilotError::Http(zenwave::Error::Http { status, .. })
        ) if status.as_u16() == 401
    )
}

/// Generate a session title using the fast model.
/// Returns None if title generation fails.
pub async fn generate_session_title(
    config: &MayConfig,
    user_message: &str,
    assistant_response: &str,
) -> Option<String> {
    use aither_core::LanguageModel;
    use aither_core::llm::{LLMRequest, Message};
    use futures_lite::StreamExt;

    // Get a fast model
    let model = config
        .default_model_for_tier(crate::config::ModelTier::Fast)
        .or(config.last_model.as_deref())?;

    // Find a provider for this model
    let resolver = config.create_resolver();
    let selected = resolver.select_provider(model)?;

    // Build the cloud provider
    let llm = build_cloud_provider_from_instance(selected.id, selected.instance, model)
        .await
        .ok()?;

    // Truncate messages for prompt
    let user_truncated = truncate_at_char_boundary(user_message, 200);
    let assistant_truncated = truncate_at_char_boundary(assistant_response, 300);

    let prompt = join_text(&[
        "Generate a short, descriptive title (3-6 words) for this conversation. Output ONLY the title, nothing else.\n\nUser: ",
        user_truncated,
        "\n\nAssistant: ",
        assistant_truncated,
    ]);

    let request = LLMRequest::new(vec![Message::user(&prompt)]);
    let mut stream = std::pin::pin!(llm.respond(request));
    let mut title = String::new();

    while let Some(result) = stream.next().await {
        if let Ok(event) = result
            && let aither_core::llm::Event::Text(text) = event
        {
            title.push_str(&text);
        }
    }

    let title = title.trim().to_string();
    if title.is_empty() {
        None
    } else {
        // Clean up title - remove quotes if present
        let title = title.trim_matches('"').trim_matches('\'').trim();
        Some(title.to_string())
    }
}

fn truncate_at_char_boundary(input: &str, max_chars: usize) -> &str {
    input
        .char_indices()
        .nth(max_chars)
        .map_or(input, |(idx, _)| &input[..idx])
}

const MEMORY_CONTEXT_HINT: &str = include_str!("../texts/memory_context_hint.txt");
const MACOS_HOST_AUTOMATION_HINT: &str = include_str!("../texts/macos_host_hint.txt");
const NATIVE_VOICE_AVAILABLE_HINT: &str = "Native voice input is available in this session. You may treat spoken input as first-class user messages.";
const PLAN_MODE_DRAFT_HINT: &str = include_str!("../texts/plan_mode_draft.txt");
const PLAN_MODE_APPROVED_HINT: &str = include_str!("../texts/plan_mode_approved.txt");

#[derive(Clone, Template)]
#[template(path = "browser_context.xml", escape = "none")]
struct BrowserTemplate {
    enabled: bool,
    cdp_endpoint: Option<String>,
    instructions: String,
}

#[derive(Clone, serde::Serialize)]
struct Browser {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cdp_endpoint: Option<String>,
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, Template)]
#[template(path = "terminal_runtime_context.xml", escape = "none")]
struct TerminalRuntimeContextTemplate {
    model_id: String,
    home_environment_kind: String,
    operating_system: String,
    home_directory: String,
    session_directory: String,
    current_working_directory: String,
    home_layout: Vec<String>,
    session_layout: Vec<String>,
    command_rules: Vec<String>,
    environment_rules: Vec<String>,
    modality_support: Vec<String>,
}

#[derive(Clone, serde::Serialize)]
struct TerminalRuntimeContext {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, Template)]
#[template(path = "workspace_context.xml", escape = "none")]
struct WorkspaceTemplate {
    operating_system: String,
    sandbox: String,
    cwd: String,
    permission_rules: Vec<String>,
    workdirs: Vec<WorkspaceDirEntry>,
}

#[derive(Clone, serde::Serialize)]
struct Workspace {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, serde::Serialize)]
struct WorkspaceDirEntry {
    path: String,
    description: String,
}

impl From<&MayWorkspaceDir> for WorkspaceDirEntry {
    fn from(value: &MayWorkspaceDir) -> Self {
        Self {
            path: value.path.display().to_string(),
            description: value
                .description
                .clone()
                .unwrap_or_else(|| "User-approved directory".to_string()),
        }
    }
}

#[derive(Clone, serde::Serialize)]
struct Memory {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, serde::Serialize)]
struct ProfilePersona {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, serde::Serialize)]
struct ProfileMemories {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, serde::Serialize)]
struct PlanMode {
    #[serde(rename = "@state")]
    state: String,
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, serde::Serialize)]
struct NetworkRestrictions {
    mode: NetworkPolicyMode,
    hosts: Vec<String>,
}

#[derive(Clone, serde::Serialize)]
struct LocationContext {
    city: String,
    region: String,
    country: String,
    timezone: String,
}

#[derive(Clone, serde::Serialize)]
struct NativeVoiceInput {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, serde::Serialize)]
struct Platform {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, Template)]
#[template(path = "remote_environment_context.xml", escape = "none")]
struct RemoteEnvironmentContextTemplate {
    operating_system: String,
    sandbox: String,
    cwd: String,
    ssh_server_id: String,
    ssh_target: String,
    permission_rules: Vec<String>,
}

#[derive(Clone, serde::Serialize)]
struct RemoteEnvironmentContext {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone, Template)]
#[template(path = "container_environment_context.xml", escape = "none")]
struct ContainerEnvironmentContextTemplate {
    operating_system: String,
    cwd: String,
    permission_rules: Vec<String>,
}

#[derive(Clone, serde::Serialize)]
struct ContainerEnvironmentContext {
    #[serde(rename = "$text")]
    content: String,
}

#[derive(Clone)]
struct BaseSystemCommand {
    name: String,
    description: String,
}

#[derive(Clone)]
struct BaseSystemSkill {
    dir_name: String,
    name: String,
    description: String,
}

#[derive(Clone)]
struct BaseSystemSubagent {
    id: String,
    description: String,
}

#[derive(Clone, Template)]
#[template(path = "base_system.md", escape = "none")]
struct BaseSystemTemplate {
    has_commands: bool,
    commands: Vec<BaseSystemCommand>,
    has_skills: bool,
    skills: Vec<BaseSystemSkill>,
    has_subagents: bool,
    subagents: Vec<BaseSystemSubagent>,
}

fn build_browser_context(enabled: bool, cdp_endpoint: Option<&str>) -> Browser {
    let instructions = if enabled {
        "Browser automation is enabled for this session and May owns the runtime lifecycle. The browser may be backed by a local managed runtime or a remote managed runtime, but it is never the user's personal browser. For web browsing, use the `browser` command itself, especially `browser read <url>`. `browser read` returns a structured page snapshot with fields such as `url`, `title`, `mainHeading`, `headings`, and `textExcerpt`; for straightforward page facts, one read is usually enough. Prefer answering directly from the `browser read` result instead of post-processing it with extra shell commands unless you truly need additional computation. Do not install Playwright, do not launch Chrome yourself, and do not rely on a visible desktop browser window.".to_string()
    } else {
        "Browser automation is disabled for this session. Do not install Playwright or launch browser runtimes yourself. If browser automation is needed, ask the user to enable the browser feature in settings.".to_string()
    };
    let template = BrowserTemplate {
        enabled,
        cdp_endpoint: cdp_endpoint.map(str::to_string),
        instructions,
    };
    Browser {
        enabled,
        cdp_endpoint: cdp_endpoint.map(str::to_string),
        content: template
            .render()
            .unwrap_or_else(|error| panic!("failed to render browser context: {error}")),
    }
}

fn build_terminal_runtime_context(
    config: &MayAgentConfig,
    session_paths: &SessionPaths,
    model_info: Option<&aither_models::ModelEntry>,
) -> TerminalRuntimeContext {
    let mut modality_support = Vec::new();
    if let Some(info) = model_info {
        modality_support.push(resolved_model_capability(info.id()));
        if info.has_ability(Ability::Vision) {
            modality_support.push("Image input is supported natively.".to_string());
        }
        if info.has_ability(Ability::Audio) {
            modality_support.push("Audio input is supported natively.".to_string());
        }
        if info.has_ability(Ability::AudioOutput) {
            modality_support.push("Audio output is supported natively.".to_string());
        }
        if info.has_ability(Ability::Video) {
            modality_support.push("Video input is supported natively.".to_string());
        }
        if info.has_ability(Ability::Pdf) {
            modality_support.push("PDF/document input is supported natively.".to_string());
        }
        if info.has_ability(Ability::ComputerUse) {
            modality_support.push(
                "Computer-use capable runtime interactions are supported by the model.".to_string(),
            );
        }
    }
    if modality_support.is_empty() {
        modality_support.push(
            "No additional multimodal abilities were resolved for this model; rely on persisted files and CLI tools when media cannot be viewed directly.".to_string(),
        );
    }

    let template = TerminalRuntimeContextTemplate {
        model_id: config.model_id.clone(),
        home_environment_kind: home_environment_kind_label(&config.environment).to_string(),
        operating_system: operating_system_hint(&config.environment).to_string(),
        home_directory: session_paths.home_root().display().to_string(),
        session_directory: session_paths.root().display().to_string(),
        current_working_directory: config.cwd.display().to_string(),
        home_layout: vec![
            "/sessions - per-session sandboxes under this home environment".to_string(),
            "/skills - profile/home-scoped skills, exposed read-only inside the session"
                .to_string(),
            "/subagents - profile/home-scoped subagents, exposed read-only inside the session"
                .to_string(),
            "workspace.toml - May-managed workspace grants for this home environment".to_string(),
        ],
        session_layout: vec![
            "/tmp - disposable temporary files".to_string(),
            "/artifacts - durable user-facing outputs; put deliverables here instead of tmp/"
                .to_string(),
            "/files - durable uploaded or recalled files".to_string(),
            "/outputs - durable overflow and full command outputs".to_string(),
            "/data - durable session-owned state".to_string(),
            "/skills - read-only profile skills mounted into this session".to_string(),
            "/subagents - read-only profile subagents mounted into this session".to_string(),
            "tasks.md - canonical task/plan document for long-running work".to_string(),
        ],
        command_rules: vec![
            "The only native execution tool is `terminal`. `terminal_kill`, `terminal_input`, and `terminal_read` manage background terminal jobs. Higher-level capabilities are terminal CLI commands exposed through the IPC bridge rather than extra native tools; rely on runtime tool descriptions and command `--help` output instead of assuming a static command list.".to_string(),
            "Issue at most one tool call per assistant turn. If you need multiple operations or parallel work, compose them inside one terminal command with shell control flow, pipelines, or background jobs instead of emitting multiple tool calls in the same response.".to_string(),
            "IPC-backed commands are only valid when executed inside this profile's home environment. If you get a system-alert because you tried to call a non-existent direct tool, immediately retry through `terminal` with the corresponding CLI command.".to_string(),
            "Commands run in the current session home environment by default unless you explicitly target another environment. Treat the home environment as the place where profile-scoped state, IPC commands, and durable session directories live.".to_string(),
            "Every terminal command call must set timeout. timeout=0 means immediate background execution. timeout>0 runs in foreground until timeout and then promotes to a background task if still running.".to_string(),
            "Inline command results are previews, not guaranteed byte-for-byte raw output. May may clean, fold, summarize, or structurally compress large outputs before putting them in context.".to_string(),
            "When exact fidelity matters, call `terminal` with raw=true. If output overflows, the complete result is persisted under session outputs/ and only a preview plus file reference is returned inline.".to_string(),
            "Treat context as memory and the filesystem as disk. After compaction, restart, or long runs, recover prior tool results from outputs/, data/, files/, artifacts/, or tasks.md instead of assuming the inline preview was complete.".to_string(),
            "For large saved outputs, use normal shell tools such as `head`, `tail`, `sed`, `rg`, `jq`, or the `ask` command against files to recall only the relevant slice.".to_string(),
            "Background tasks persist output to files. Use terminal_read for incremental reads, terminal_input for stdin when the process needs user input, and terminal_kill to stop them.".to_string(),
            "For image-producing commands, set expect=image and optionally resolution=auto|low|medium|high|native. If omitted, May picks a preview resolution from the source media.".to_string(),
            "When you create a user-facing file, write it under artifacts/ so it survives and can be surfaced cleanly to the user.".to_string(),
            "Use tasks.md as the canonical working document for multi-step work; it should stay accurate enough to resume after context loss.".to_string(),
        ],
        environment_rules: environment_runtime_rules(&config.environment),
        modality_support,
    };
    TerminalRuntimeContext {
        content: template
            .render()
            .unwrap_or_else(|error| panic!("failed to render terminal runtime context: {error}")),
    }
}

fn resolved_model_capability(model_id: &str) -> String {
    let mut text = String::with_capacity(model_id.len() + 16);
    text.push_str("Resolved model: ");
    text.push_str(model_id);
    text
}

fn build_workspace_context(workspace: &MayWorkspace) -> Workspace {
    let template = WorkspaceTemplate {
        operating_system: operating_system_hint(&AgentEnvironment::Local).to_string(),
        sandbox: workspace.sandbox.display().to_string(),
        cwd: workspace.cwd.display().to_string(),
        permission_rules: vec![
            "You are on the user's real local machine inside the profile home environment.".to_string(),
            "The session sandbox is the default writable workspace. Stay there unless there is a concrete reason to touch another host path.".to_string(),
            "Within May-approved workspace roots you may read and write normally. Outside them, prefer inspection first and request approval before host-level side effects.".to_string(),
            "Because this is the user's actual computer, treat destructive or irreversible commands as dangerous and seek approval before running them.".to_string(),
        ],
        workdirs: workspace
            .config
            .workdirs
            .iter()
            .map(WorkspaceDirEntry::from)
            .collect(),
    };
    Workspace {
        content: template
            .render()
            .unwrap_or_else(|error| panic!("failed to render workspace context: {error}")),
    }
}

fn build_remote_environment_context(
    workspace: &MayWorkspace,
    ssh_server: &crate::config::SshServerConfig,
) -> RemoteEnvironmentContext {
    let template = RemoteEnvironmentContextTemplate {
        operating_system: operating_system_hint(&AgentEnvironment::Remote {
            ssh_server: ssh_server.clone(),
        })
        .to_string(),
        sandbox: workspace.sandbox.display().to_string(),
        cwd: workspace.cwd.display().to_string(),
        ssh_server_id: ssh_server.name.clone(),
        ssh_target: ssh_server.target.clone(),
        permission_rules: vec![
            "This profile's home environment is the bound SSH target, so unqualified terminal commands default to that remote machine.".to_string(),
            "Treat the remote host as real infrastructure. Inspect before mutating, and ask for approval before privileged or potentially destructive changes.".to_string(),
            "Remote shell, filesystem layout, and operating system details are not guaranteed. Detect them on-host before using shell-specific or OS-specific commands.".to_string(),
        ],
    };
    RemoteEnvironmentContext {
        content: template
            .render()
            .unwrap_or_else(|error| panic!("failed to render remote environment context: {error}")),
    }
}

fn build_container_environment_context(cwd: &std::path::Path) -> ContainerEnvironmentContext {
    let template = ContainerEnvironmentContextTemplate {
        operating_system: "linux (managed container runtime)".to_string(),
        cwd: cwd.display().to_string(),
        permission_rules: vec![
            "This profile's home environment is a managed container rather than the user's host machine.".to_string(),
            "Use terminal commands aggressively; the container filesystem is the primary writable workspace unless a specific command reports otherwise.".to_string(),
            "Persist anything important into durable session directories such as data/, outputs/, files/, or artifacts/ because container-global state may disappear.".to_string(),
            "Do not assume desktop automation or host-specific tools exist inside the container unless you verify them.".to_string(),
        ],
    };
    ContainerEnvironmentContext {
        content: template.render().unwrap_or_else(|error| {
            panic!("failed to render container environment context: {error}")
        }),
    }
}

async fn build_base_system_prompt(
    shared_terminal_commands: &[(String, String)],
    skills_dir: &std::path::Path,
    subagents_dir: &std::path::Path,
) -> Result<String, MayAgentError> {
    let mut commands = shared_terminal_commands
        .iter()
        .map(|(name, description)| BaseSystemCommand {
            name: name.clone(),
            description: description.clone(),
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.name.cmp(&right.name));

    let skills = SkillManager::from_skills_dir(skills_dir.to_path_buf())
        .list_skills()
        .await
        .map_err(|error| {
            MayAgentError::Sandbox(format!("failed to load skill prompt metadata: {error}"))
        })?
        .into_iter()
        .map(|skill| BaseSystemSkill {
            dir_name: skill.dir_name,
            name: skill.name,
            description: skill.description,
        })
        .collect::<Vec<_>>();

    let mut subagents = load_prompt_subagents(subagents_dir).await?;
    subagents.sort_by(|left, right| left.id.cmp(&right.id));

    let template = BaseSystemTemplate {
        has_commands: !commands.is_empty(),
        commands,
        has_skills: !skills.is_empty(),
        skills,
        has_subagents: !subagents.is_empty(),
        subagents,
    };
    template.render().map_err(|error| {
        MayAgentError::Sandbox(format!("failed to render base system prompt: {error}"))
    })
}

async fn load_prompt_subagents(
    subagents_dir: &std::path::Path,
) -> Result<Vec<BaseSystemSubagent>, MayAgentError> {
    let manager = SubagentManager::from_subagents_dir(subagents_dir.to_path_buf());
    let path = manager.subagents_dir();
    if async_fs::metadata(path).await.is_err() {
        return Ok(Vec::new());
    }

    let defs = SubagentDefinition::load_from_dir_async(path)
        .await
        .map_err(|error| {
            MayAgentError::Sandbox(format!("failed to load subagent prompt metadata: {error}"))
        })?;

    Ok(defs
        .into_iter()
        .map(|def| BaseSystemSubagent {
            id: def.id,
            description: def.description,
        })
        .collect())
}

fn home_environment_kind_label(environment: &AgentEnvironment) -> &'static str {
    match environment {
        AgentEnvironment::Local => "local",
        AgentEnvironment::Remote { .. } => "remote",
        AgentEnvironment::Container { .. } => "container",
    }
}

fn operating_system_hint(environment: &AgentEnvironment) -> &'static str {
    match environment {
        AgentEnvironment::Local => host_operating_system_label(),
        AgentEnvironment::Remote { .. } => "unknown until detected on the remote host",
        AgentEnvironment::Container { .. } => "linux (managed container runtime)",
    }
}

fn host_operating_system_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

fn environment_runtime_rules(environment: &AgentEnvironment) -> Vec<String> {
    match environment {
        AgentEnvironment::Local => vec![
            "This home environment runs on the user's local machine. Default to the session sandbox and approved workspace roots for writes.".to_string(),
            "Outside approved writable paths, treat the host as read-mostly and request approval before side effects.".to_string(),
            "Host automation must respect the local operating system and user presence.".to_string(),
        ],
        AgentEnvironment::Remote { ssh_server } => vec![
            format!(
                "This home environment is the remote SSH target `{}` (`{}`).",
                ssh_server.name, ssh_server.target
            ),
            "Treat the remote machine as real infrastructure and avoid assuming its shell, package manager, or OS family until you verify them on-host.".to_string(),
            "Prefer explicit, auditable commands and ask for approval before privileged or destructive operations.".to_string(),
        ],
        AgentEnvironment::Container { .. } => vec![
            "This home environment runs inside a managed container. Prefer direct terminal usage and assume the filesystem is writable unless a command says otherwise.".to_string(),
            "Container state outside the durable session directories may be ephemeral, so persist anything important into session files.".to_string(),
            "The container is isolated from the user's host machine, but you should still avoid needless destructive commands inside it.".to_string(),
        ],
    }
}

fn build_location_context(location: &crate::location::LocationInfo) -> LocationContext {
    LocationContext {
        city: location.city.clone(),
        region: location.region.clone(),
        country: location.country.clone(),
        timezone: location.timezone.clone(),
    }
}

fn build_memory_context_content(memory: &str) -> String {
    join_text(&[MEMORY_CONTEXT_HINT.trim(), "\n\n", memory])
}

fn build_memory_context(memory: &str) -> Memory {
    Memory {
        content: build_memory_context_content(memory),
    }
}

fn build_native_voice_context(native_voice_input: bool) -> Option<NativeVoiceInput> {
    if native_voice_input {
        Some(NativeVoiceInput {
            content: NATIVE_VOICE_AVAILABLE_HINT.to_string(),
        })
    } else {
        None
    }
}

fn build_plan_mode_context(plan_state: SessionPlanState) -> Option<PlanMode> {
    match plan_state {
        SessionPlanState::Disabled => None,
        SessionPlanState::Draft => Some(PlanMode {
            state: SessionPlanState::Draft.as_str().to_string(),
            content: PLAN_MODE_DRAFT_HINT.to_string(),
        }),
        SessionPlanState::Approved => Some(PlanMode {
            state: SessionPlanState::Approved.as_str().to_string(),
            content: PLAN_MODE_APPROVED_HINT.to_string(),
        }),
    }
}

fn build_network_restrictions_context(
    policy: Option<&CompiledNetworkPolicy>,
) -> Option<NetworkRestrictions> {
    let policy = policy?;
    if matches!(policy.mode(), NetworkPolicyMode::Disabled) {
        return None;
    }
    Some(NetworkRestrictions {
        mode: policy.mode(),
        hosts: policy.hosts().to_vec(),
    })
}

fn build_macos_host_automation_context() -> Option<Platform> {
    if cfg!(target_os = "macos") && !is_running_in_container() {
        return Some(Platform {
            content: MACOS_HOST_AUTOMATION_HINT.to_string(),
        });
    }
    None
}

fn is_running_in_container() -> bool {
    if std::env::var_os("container").is_some() {
        return true;
    }

    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        return true;
    }

    std::fs::read_to_string("/proc/1/cgroup")
        .map(|content| {
            let lower = content.to_ascii_lowercase();
            lower.contains("docker")
                || lower.contains("containerd")
                || lower.contains("kubepods")
                || lower.contains("podman")
                || lower.contains("lxc")
        })
        .unwrap_or(false)
}

fn global_memory_file_path(may_dir: &std::path::Path) -> PathBuf {
    let upper = crate::session::MayHomePaths::new(may_dir.to_path_buf()).memory_file();
    if !upper.exists() {
        std::fs::write(&upper, "").unwrap_or_else(|e| {
            panic!("failed to initialize MEMORY.md at {}: {e}", upper.display())
        });
    }
    upper
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{ProfilePersona, RuntimeSystemContext, Workspace, build_memory_context_content};

    #[test]
    fn runtime_system_context_reinstates_memory_and_prunes_absent_blocks() {
        let dir = tempdir().expect("temp dir should be created");
        let memory_path = dir.path().join("MEMORY.md");
        std::fs::write(&memory_path, "remember this").expect("memory file should be written");

        let mut runtime_context =
            RuntimeSystemContext::new(memory_path, "remember this".to_string());
        let mut context = aither_agent::Context::default();
        context.insert_system(&ProfilePersona {
            content: "stale persona".to_string(),
        });
        context.insert_system(&Workspace {
            content: "stale workspace".to_string(),
        });

        runtime_context.reconcile(&mut context);

        assert!(context.has_system_block("memory"));
        let memory = context
            .system_blocks()
            .get("memory")
            .expect("memory block should be present");
        assert!(
            memory.contains(&build_memory_context_content("remember this")),
            "unexpected memory block: {memory}"
        );
        assert!(!context.has_system_block("profile_persona"));
        assert!(!context.has_system_block("workspace"));
    }

    #[test]
    fn runtime_system_context_refreshes_memory_from_disk() {
        let dir = tempdir().expect("temp dir should be created");
        let memory_path = dir.path().join("MEMORY.md");
        std::fs::write(&memory_path, "before").expect("memory file should be written");

        let mut runtime_context =
            RuntimeSystemContext::new(memory_path.clone(), "before".to_string());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);

        std::fs::write(&memory_path, "after").expect("memory file should be updated");
        tokio_test::block_on(runtime_context.sync_memory_file());
        runtime_context.reconcile(&mut context);

        let memory = context
            .system_blocks()
            .get("memory")
            .expect("memory block should be present");
        assert!(
            memory.contains(&build_memory_context_content("after")),
            "unexpected memory block: {memory}"
        );
    }

    #[test]
    fn terminal_runtime_context_is_reconciled_as_system_block() {
        let dir = tempdir().expect("temp dir should be created");
        let memory_path = dir.path().join("MEMORY.md");
        std::fs::write(&memory_path, "remember this").expect("memory file should be written");

        let mut runtime_context =
            RuntimeSystemContext::new(memory_path, "remember this".to_string())
                .with_terminal_runtime(super::TerminalRuntimeContext {
                    content: "<terminal_runtime_context><model_id>demo-model</model_id></terminal_runtime_context>"
                        .to_string(),
                });
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);

        let terminal_runtime = context
            .system_blocks()
            .get("terminal_runtime_context")
            .expect("terminal runtime block should be present");
        assert!(
            terminal_runtime.contains("demo-model"),
            "unexpected terminal runtime block: {terminal_runtime}"
        );
    }
}
