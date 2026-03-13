//! May agent wrapper around aither-agent.

use std::{borrow::Cow, collections::HashSet, path::PathBuf, sync::Arc};

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::{SubagentFileMount, SubagentTool};
use aither_agent::webfetch::WebFetchTool;
use aither_agent::websearch::WebSearchTool;
use aither_agent::{
    Agent, AgentError, AgentEvent, AgentKind, BashAgentBuilder, BrowserBackend, CompactResult,
    HCons, Hook, TurnBoundaryAction, TurnBoundaryContext,
};
use aither_cloud::{
    CloudProvider, claude::Claude, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_core::image::{ImageGenerator, Prompt, Size};
use aither_core::llm::{Message as LlmMessage, Tool, ToolOutput};
use aither_sandbox::{
    BashMode, BashTool, ContainerExecHandle, ShellRuntimeAvailability, SshServer,
    permission::{NoopPermissionHandler, PermissionError, PermissionHandler},
};
use executor_core::DefaultExecutor;
use futures_lite::{Stream, StreamExt};

use crate::agent::notify_tool::NotifyTool;
use crate::agent::register_tool::{
    RegisterSkillsBroker, RegisterSkillsTool, RegisterSubagentsBroker, RegisterSubagentsTool,
};
use crate::agent::search_tool::SearchTool;
use crate::agent::session_tool::{EnvironmentTool, SessionTool};
use crate::agent::task_tool::{TaskRuntimeKind, TaskTool};
use crate::ask_user::{AskUserBroker, AskUserTool};
use crate::config::{MatchMode, MayConfig, ProviderInstance, ProviderType, model_ids_match};
use crate::copilot::clear_copilot_credentials;
use crate::db::NetworkAccessLogParams;
use crate::live_response::LiveResponseRegistry;
use crate::network_policy::{CompiledNetworkPolicy, NetworkPolicyMode};
use crate::session::{MessageRole, SessionId, SessionPaths, SessionPlanState};
use crate::workspace::{Workspace as MayWorkspace, WorkspaceDir as MayWorkspaceDir};
use crate::workspace_request::{RequestWorkspaceTool, WorkspaceRequestBroker};

// Error message templates
const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");

fn err_no_provider(model: &str) -> String {
    format!("No provider available for model '{model}'. Use /config to add providers.")
}

fn err_api_key_required(provider: &str) -> String {
    format!("API key required for provider '{provider}'")
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

pub fn provider_candidate_count(config: &MayConfig, model: &str) -> usize {
    provider_candidates_for_model(config, model).len()
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
            MayAgentError::Llm(format!(
                "Failed to validate model '{model}' for provider '{provider_id}' via /models: {error}"
            ))
        })?;

    let mode = MatchMode::from_strict_flag(config.preferences.strict_model_matching);
    let has_match = fetched
        .iter()
        .any(|candidate| model_ids_match(candidate.id.as_str(), model, mode));

    if !has_match {
        return Err(MayAgentError::Llm(format!(
            "Provider '{provider_id}' does not advertise model '{model}' in /models"
        )));
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
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
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

impl core::str::FromStr for PermissionRequestMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Default),
            "unsafe" => Ok(Self::Unsafe),
            "ssh" => Ok(Self::Ssh),
            other => Err(format!("unknown permission request mode: {other}")),
        }
    }
}

impl From<BashMode> for PermissionRequestMode {
    fn from(value: BashMode) -> Self {
        match value {
            BashMode::Sandboxed | BashMode::Network => Self::Default,
            BashMode::Unsafe => Self::Unsafe,
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

#[derive(Debug, Clone)]
pub struct PermissionApprover {
    live_responses: Arc<LiveResponseRegistry<bool>>,
    session_id: Option<String>,
    runtime_db: Option<crate::db::ChatDb>,
    ui_events_notify: Option<std::sync::Arc<tokio::sync::Notify>>,
}

impl PermissionApprover {
    pub fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            session_id: None,
            runtime_db: None,
            ui_events_notify: None,
        }
    }

    pub fn for_session(&self, session_id: impl Into<String>) -> Self {
        Self {
            live_responses: self.live_responses.clone(),
            session_id: Some(session_id.into()),
            runtime_db: self.runtime_db.clone(),
            ui_events_notify: self.ui_events_notify.clone(),
        }
    }

    #[must_use]
    pub fn with_runtime_sync(
        mut self,
        db: crate::db::ChatDb,
        notify: std::sync::Arc<tokio::sync::Notify>,
    ) -> Self {
        self.runtime_db = Some(db);
        self.ui_events_notify = Some(notify);
        self
    }

    fn notify_ui_events(&self) {
        if let Some(notify) = &self.ui_events_notify {
            notify.notify_waiters();
        }
    }

    fn persisted_request(
        request_id: String,
        payload: PermissionRequest,
    ) -> crate::session::PersistedPermissionRequest {
        crate::session::PersistedPermissionRequest {
            id: request_id,
            request: crate::session::PersistedPermissionRequestPayload {
                mode: payload.mode.as_str().to_string(),
                script: payload.script,
                session_id: payload.session_id,
            },
        }
    }

    fn history_mode(mode: BashMode) -> &'static str {
        match mode {
            BashMode::Sandboxed => "sandboxed",
            BashMode::Network => "network",
            BashMode::Unsafe => "unsafe",
        }
    }

    async fn append_request_history(
        db: &crate::db::ChatDb,
        session_id: &str,
        request_id: &str,
        mode: BashMode,
        script: &str,
    ) -> Result<(), PermissionError> {
        let content = serde_json::to_string(&crate::session::history::PermissionRequestData {
            id: request_id.to_string(),
            mode: Self::history_mode(mode).to_string(),
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
        mode: BashMode,
        script: String,
    ) -> Result<bool, PermissionError> {
        let Some(session_id) = self.session_id.clone() else {
            return Err(PermissionError::Internal(anyhow::anyhow!(
                "permission approval requires a bound session id"
            )));
        };

        if mode == BashMode::Network
            && let Some(db) = &self.runtime_db
            && db
                .session_has_approved_permission_mode(&session_id, Self::history_mode(mode))
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

        if let Some(db) = self.runtime_db.clone() {
            db.set_waiting_permission(
                &session_id,
                Some(Self::persisted_request(request_id.clone(), payload.clone())),
            )
            .await
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)?;
            Self::append_request_history(&db, &session_id, &request_id, mode, &payload.script)
                .await?;
        }

        self.notify_ui_events();

        rx.recv().await.map_err(|_| PermissionError::Interrupted)
    }

    pub fn respond(&self, request_id: &str, approved: bool) -> bool {
        let responded = self.live_responses.resolve(request_id, approved);
        if responded {
            self.notify_ui_events();
        }
        responded
    }
}

impl Default for PermissionApprover {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionHandler for PermissionApprover {
    async fn check(&self, mode: BashMode, script: &str) -> Result<bool, PermissionError> {
        match mode {
            BashMode::Sandboxed => Ok(true),
            BashMode::Network | BashMode::Unsafe => {
                let approved = self.request_and_wait(mode, script.to_string()).await?;
                if approved {
                    Ok(true)
                } else {
                    Err(PermissionError::Denied(
                        [mode.description(), " mode denied for script"].concat(),
                    ))
                }
            }
        }
    }

    async fn check_domain(&self, _domain: &str, _port: u16) -> bool {
        true
    }
}

#[derive(Debug, Clone, Default)]
struct MayPermissionHandler {
    approver: Option<PermissionApprover>,
    network_policy: Option<CompiledNetworkPolicy>,
    profile_id: Option<String>,
}

impl MayPermissionHandler {
    const fn new(
        approver: Option<PermissionApprover>,
        network_policy: Option<CompiledNetworkPolicy>,
        profile_id: Option<String>,
    ) -> Self {
        Self {
            approver,
            network_policy,
            profile_id,
        }
    }

    async fn append_network_audit_log(
        &self,
        domain: &str,
        port: u16,
        allowed: bool,
        reason: Option<&str>,
    ) {
        let Some(approver) = &self.approver else {
            return;
        };
        let Some(db) = approver.runtime_db.clone() else {
            return;
        };
        let _ = db
            .append_network_access_log(NetworkAccessLogParams {
                session_id: approver.session_id.as_deref(),
                profile_id: self.profile_id.as_deref(),
                source: "chat",
                mode: "network",
                domain,
                port,
                allowed,
                reason,
            })
            .await;
    }
}

impl PermissionHandler for MayPermissionHandler {
    async fn check(&self, mode: BashMode, script: &str) -> Result<bool, PermissionError> {
        match &self.approver {
            Some(approver) => approver.check(mode, script).await,
            None => NoopPermissionHandler.check(mode, script).await,
        }
    }

    async fn check_domain(&self, domain: &str, port: u16) -> bool {
        if let Some(policy) = &self.network_policy
            && !policy.allows_domain(domain)
        {
            tracing::warn!(domain, port, mode = %policy.mode().as_str(), "blocked outbound domain by configured network policy");
            self.append_network_audit_log(domain, port, false, Some(policy.mode().as_str()))
                .await;
            return false;
        }
        let allowed = match &self.approver {
            Some(approver) => approver.check_domain(domain, port).await,
            None => NoopPermissionHandler.check_domain(domain, port).await,
        };
        self.append_network_audit_log(domain, port, allowed, None).await;
        allowed
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingSteerHook {
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

/// Execution environment for a MayAgent.
pub enum AgentEnvironment {
    /// Native macOS sandbox via leash.
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
        /// Executor for running commands inside the container.
        container_exec: Arc<ContainerExecHandle>,
        /// Optional browser backend that can provide a CDP endpoint.
        browser_backend: Option<Arc<dyn BrowserBackend>>,
    },
}

/// Configuration for creating a MayAgent.
pub struct MayAgentConfig {
    /// The LLM provider.
    pub llm: CloudProvider,
    /// Provider ID selected for this request.
    pub provider_id: String,
    /// Session sandbox directory (BashTool creates working dir inside).
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
    /// Preconfigured SSH servers available for `bash` ssh mode.
    pub ssh_servers: Vec<crate::config::SshServerConfig>,
    /// Optional web UI permission approver.
    pub permission_approver: Option<PermissionApprover>,
    /// Execution environment (local leash sandbox or container).
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
    /// Optional broker for `register-skills` command requests.
    pub register_skills_broker: Option<RegisterSkillsBroker>,
    /// Optional broker for `register-subagents` command requests.
    pub register_subagents_broker: Option<RegisterSubagentsBroker>,
    /// Whether this model/runtime pair supports native voice input.
    pub native_voice_input: bool,
    /// Model context window resolved from provider metadata.
    pub model_context_window: Option<u32>,
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
    /// Image size as WIDTHxHEIGHT, for example 1024x1024.
    pub size: String,
}

#[derive(Clone)]
struct ImagineTool {
    generator: Option<ImageGeneratorConfig>,
    artifacts_dir: PathBuf,
}

#[derive(Clone)]
struct RuntimeSystemContext {
    memory_path: PathBuf,
    last_memory_content: String,
    workspace: Option<Workspace>,
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
    fn new(memory_path: PathBuf, memory_content: String) -> Self {
        Self {
            memory_path,
            last_memory_content: memory_content,
            workspace: None,
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

    fn with_workspace(mut self, workspace: Workspace) -> Self {
        self.workspace = Some(workspace);
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
        self.refresh_memory_file();
        let memory = build_memory_context(&self.last_memory_content);
        reconcile_system_block(context, Some(&memory));
        reconcile_system_block(context, self.workspace.as_ref());
        reconcile_system_block(context, self.browser.as_ref());
        reconcile_system_block(context, self.profile_persona.as_ref());
        reconcile_system_block(context, self.profile_memories.as_ref());
        reconcile_system_block(context, self.plan_mode.as_ref());
        reconcile_system_block(context, self.network_restrictions.as_ref());
        reconcile_system_block(context, self.location.as_ref());
        reconcile_system_block(context, self.native_voice_input.as_ref());
        reconcile_system_block(context, self.platform.as_ref());
    }

    fn refresh_memory_file(&mut self) {
        let memory_content =
            std::fs::read_to_string(&self.memory_path).unwrap_or_else(|error| {
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

    async fn call(&self, arguments: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let generator = self
            .generator
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no image_generation model configured"))?;

        let size = parse_imagine_size(&arguments.size)?;
        let prompt = Prompt::new(arguments.prompt);

        let image_provider = build_image_provider(generator)?;

        let mut latest = None;
        match image_provider {
            CloudProvider::OpenAI(provider) => {
                let mut stream = std::pin::pin!(provider.create(prompt, size));
                while let Some(chunk) = stream.next().await {
                    latest = Some(chunk.map_err(|e| anyhow::anyhow!(e.to_string()))?);
                }
            }
            CloudProvider::Gemini(provider) => {
                let mut stream = std::pin::pin!(provider.create(prompt, size));
                while let Some(chunk) = stream.next().await {
                    latest = Some(chunk.map_err(|e| anyhow::anyhow!(e.to_string()))?);
                }
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "selected provider does not support image generation"
                ));
            }
        }

        let image = latest.ok_or_else(|| anyhow::anyhow!("image generation returned no data"))?;
        let filename = format!("{}.png", uuid::Uuid::new_v4());
        let path = self.artifacts_dir.join(&filename);
        async_fs::create_dir_all(&self.artifacts_dir).await?;
        async_fs::write(&path, image).await?;

        Ok(ToolOutput::text(format!("artifacts/{filename}")))
    }
}

fn parse_imagine_size(raw: &str) -> aither_core::Result<Size> {
    let value = raw.trim();
    let Some((w, h)) = value.split_once('x') else {
        return Err(anyhow::anyhow!(
            "invalid size format: expected WIDTHxHEIGHT"
        ));
    };

    let width: u32 = w
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid width in size"))?;
    let height: u32 = h
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid height in size"))?;

    if width == 0 || height == 0 {
        return Err(anyhow::anyhow!("size must be greater than 0"));
    }

    Ok(Size::new(width, height))
}

fn build_image_provider(config: &ImageGeneratorConfig) -> aither_core::Result<CloudProvider> {
    let provider_type = config.provider.resolve_type(&config.provider_id);
    let api_key = config.provider.api_key.as_deref().unwrap_or("");
    let base_url = config.provider.effective_base_url(&config.provider_id);

    if provider_type.requires_api_key() && api_key.is_empty() {
        return Err(anyhow::anyhow!(err_api_key_required(&config.provider_id)));
    }

    let provider = match provider_type {
        ProviderType::Google => {
            let mut client = Gemini::new(api_key).with_image_model(&config.model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            CloudProvider::Gemini(client)
        }
        _ => {
            let mut client = OpenAI::new(api_key).with_image_model(&config.model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            CloudProvider::OpenAI(client)
        }
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
        .map(|s| s.to_string())
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
    inner: Agent<CloudProvider, CloudProvider, CloudProvider, HCons<PendingSteerHook, ()>>,
    session_id: SessionId,
    sandbox_dir: PathBuf,
    provider_id: String,
    model_context_window: Option<u32>,
    runtime_system_context: RuntimeSystemContext,
}

impl MayAgent {
    /// Create a new MayAgent.
    pub async fn new(config: MayAgentConfig) -> Result<Self, MayAgentError> {
        let is_container = matches!(config.environment, AgentEnvironment::Container { .. });

        let executor = DefaultExecutor;

        // Create bash tool in sandbox dir (BashTool creates its own working directory inside)
        let bash_tool: BashTool<MayPermissionHandler, DefaultExecutor> = BashTool::new_exact(
            &config.sandbox_dir,
            MayPermissionHandler::new(
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

        // Clone LLM for tools that need it before it's moved into the builder
        let fast_llm = config.llm.clone();
        let subagent_llm = config.llm.clone();

        // Create tools
        let websearch = WebSearchTool::default();
        let webfetch = WebFetchTool::new();
        let ask = AskCommand::new(fast_llm);
        let request_workspace = RequestWorkspaceTool::new(config.workspace_request_broker);
        let ssh_authorizer_broker = config.ask_user_broker.clone();
        let ask_user = AskUserTool::new(config.ask_user_broker);
        let session_tool = SessionTool::new(
            config.may_dir.clone(),
            config.session_id.as_str().to_string(),
        );
        let environment_tool = EnvironmentTool::new(
            config.may_dir.clone(),
            config.session_id.as_str().to_string(),
            config.profile_id.clone(),
        );
        let search_tool = SearchTool::new(config.sandbox_dir.clone());
        let task_tool = TaskTool::new(
            config.session_id.as_str().to_string(),
            match &config.environment {
                AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                AgentEnvironment::Remote { .. } => TaskRuntimeKind::Remote,
                AgentEnvironment::Local => TaskRuntimeKind::Local,
            },
            config.task_broker,
        );
        let imagine = ImagineTool {
            generator: config.image_generator,
            artifacts_dir: session_paths.artifacts_dir(),
        };
        let notify_tool = NotifyTool::new(config.notify_broker);
        let register_skills_tool = RegisterSkillsTool::new(
            config.session_id.as_str().to_string(),
            config.register_skills_broker,
        );
        let register_subagents_tool = RegisterSubagentsTool::new(
            config.session_id.as_str().to_string(),
            config.register_subagents_broker,
        );
        let subagent_tool = SubagentTool::new(subagent_llm).with_builtins();
        let pending_steer_hook = PendingSteerHook::new(
            config
                .permission_approver
                .as_ref()
                .and_then(|approver| approver.runtime_db.clone()),
            &config.session_id,
        );

        let transcript_file = session_paths.transcript_path();
        let transcript_path = transcript_file.display().to_string();

        let skills_dir = config.sandbox_dir.join("skills");
        let subagents_dir = config.sandbox_dir.join("subagents");

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
        let mut builder = BashAgentBuilder::new(config.llm, bash_tool)
            .tool(websearch)
            .tool(webfetch)
            .tool(ask)
            .tool(ask_user)
            .tool(session_tool)
            .tool(environment_tool)
            .tool(search_tool)
            .tool(task_tool)
            .tool(imagine)
            .tool(notify_tool)
            .tool(register_skills_tool)
            .tool(register_subagents_tool)
            .hook(pending_steer_hook);
        if !is_container {
            builder = builder.tool(request_workspace);
        }
        if is_container {
            builder = builder.with_skills_readable_only(&skills_dir).await?;
            builder = builder.with_subagents_readable_only(&subagents_dir).await?;
        } else {
            builder = builder.with_skills(&skills_dir).await?;
            builder = builder.with_subagents(&subagents_dir).await?;
        }
        builder = builder
            .with_default_prompt()
            .shell_runtime_availability(availability)
            .ssh_servers(ssh_servers)
            .ssh_authorizer(super::ssh_authorizer::MaySshAuthorizer::new(
                ssh_authorizer_broker,
            ))
            .agent_kind(AgentKind::Coding)
            .persona_prompt(include_str!("../../templates/system.md"))
            .transcript_path(transcript_path)
            .transcript(transcript_file)
            .system(build_memory_context(&memory_content))
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

        // Environment-specific configuration
        match config.environment {
            AgentEnvironment::Container {
                container_id,
                container_exec,
                browser_backend,
            } => {
                builder = builder
                    .container_id(container_id)
                    .container_exec(container_exec);
                if let Some(browser_backend) = browser_backend {
                    let endpoint = browser_backend.cdp_endpoint().await.map_err(|error| {
                        MayAgentError::Sandbox(format!(
                            "failed to resolve browser CDP endpoint from backend: {error}"
                        ))
                    })?;
                    let browser_context = build_browser_context(Some(endpoint.as_str()));
                    runtime_system_context =
                        runtime_system_context.with_browser(browser_context.clone());
                    builder = builder.system(browser_context);
                }
            }
            AgentEnvironment::Local => {
                let workspace_context = build_workspace_context(&workspace);
                runtime_system_context =
                    runtime_system_context.with_workspace(workspace_context.clone());
                builder = builder.system(workspace_context);
                if let Some(macos_hint) = build_macos_host_automation_context() {
                    runtime_system_context = runtime_system_context.with_platform(macos_hint.clone());
                    builder = builder.system(macos_hint);
                }
            }
            AgentEnvironment::Remote { ssh_server } => {
                let remote_context = build_remote_environment_context(&workspace, &ssh_server);
                builder = builder.system(remote_context);
            }
        }

        // Get the actual working directory before building
        let actual_sandbox_dir = PathBuf::from(builder.sandbox_dir().as_ref());

        // Configure SubagentTool with explicit roots so file-based subagents resolve
        // correctly in both local and container profiles.
        let subagent_tool = subagent_tool
            .with_base_dir(actual_sandbox_dir.clone())
            .with_file_mounts([
                SubagentFileMount::new(PathBuf::from("."), actual_sandbox_dir.clone()),
                SubagentFileMount::new(
                    PathBuf::from("subagents"),
                    actual_sandbox_dir.join("subagents"),
                ),
                SubagentFileMount::new(PathBuf::from("skills"), actual_sandbox_dir.join("skills")),
            ]);
        builder = builder.tool(subagent_tool);

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

    /// Create a MayAgent from config file and session.
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

    /// Create a MayAgent from config file with a specific model.
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
            SessionPlanState::Disabled,
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
        profile_id: Option<String>,
        permission_approver: Option<PermissionApprover>,
        profile_persona: Option<String>,
        profile_memories: Option<crate::memory::ProfileMemories>,
        plan_state: SessionPlanState,
        location: Option<crate::location::LocationInfo>,
        task_broker: Option<crate::agent::task_tool::TaskBroker>,
        notify_broker: Option<crate::agent::notify_tool::NotifyBroker>,
        register_skills_broker: Option<RegisterSkillsBroker>,
        register_subagents_broker: Option<RegisterSubagentsBroker>,
        environment: AgentEnvironment,
        provider_attempt: usize,
    ) -> Result<Self, MayAgentError> {
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

            match build_cloud_provider_from_instance(&provider_id, &provider_instance, model).await
            {
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
        let model_info = aither_models::lookup(model);

        let config = MayAgentConfig {
            llm,
            provider_id,
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
            network_policy: may_config.preferences.network_policy.compile().map_err(MayAgentError::Sandbox)?,
            location,
            task_broker,
            notify_broker,
            register_skills_broker,
            register_subagents_broker,
            native_voice_input: model_info
                .is_some_and(|info| info.has_ability(aither_core::llm::model::Ability::Audio)),
            model_context_window: model_info.and_then(|info| info.max_input_tokens()),
        };

        Self::new(config).await
    }

    /// Run a query and stream events.
    pub fn run(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> impl Stream<Item = Result<AgentEvent, AgentError>> + '_ {
        self.refresh_runtime_context();
        self.inner.run(prompt, attachments)
    }

    /// One-shot query.
    pub async fn query(&mut self, prompt: &str) -> Result<String, AgentError> {
        self.refresh_runtime_context();
        self.inner.query(prompt).await
    }

    /// Preview the exact request message sequence for the next turn.
    pub async fn preview_request_messages(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> Vec<LlmMessage> {
        self.refresh_runtime_context();
        self.inner
            .preview_request_messages(prompt, attachments)
            .await
    }

    /// Get session ID.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Get session sandbox directory.
    pub fn sandbox_dir(&self) -> PathBuf {
        self.sandbox_dir.clone()
    }

    /// Selected provider ID for the current request.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Model context window resolved for this agent.
    pub const fn model_context_window(&self) -> Option<u32> {
        self.model_context_window
    }

    pub(crate) fn export_context_json(&self) -> Result<String, serde_json::Error> {
        self.inner.export_context_json()
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

    pub(crate) async fn snapshot_context_window(&mut self) -> aither_agent::ContextWindowSnapshot {
        self.inner.snapshot_context_window().await
    }

    pub(crate) fn restore_context_json(
        &mut self,
        context_json: &str,
    ) -> Result<(), serde_json::Error> {
        self.inner.restore_context_json(context_json)?;
        self.refresh_runtime_context();
        Ok(())
    }

    /// Clear conversation history.
    pub fn clear_history(&mut self) {
        self.inner.clear_history();
    }

    fn refresh_runtime_context(&mut self) {
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

/// Build a CloudProvider from a ProviderInstance directly.
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
        ProviderType::Groq | ProviderType::Together | ProviderType::Mistral | ProviderType::XAI => {
            let mut client = OpenAI::new(api_key).with_model(model);
            if let Some(url) = base_url {
                client = client.with_base_url(url);
            }
            client.into()
        }
        ProviderType::Custom => {
            let url = base_url.ok_or_else(|| {
                MayAgentError::MissingApiKey(format!(
                    "Base URL required for custom provider '{}'",
                    id
                ))
            })?;
            OpenAI::new(api_key)
                .with_base_url(url)
                .with_model(model)
                .into()
        }
    };

    Ok(llm)
}

fn is_copilot_unauthorized(err: &crate::copilot::CopilotTokenError) -> bool {
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

    let prompt = format!(
        "Generate a short, descriptive title (3-6 words) for this conversation. Output ONLY the title, nothing else.\n\nUser: {}\n\nAssistant: {}",
        user_truncated, assistant_truncated
    );

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

#[derive(Clone, serde::Serialize)]
struct Browser {
    #[serde(skip_serializing_if = "Option::is_none")]
    cdp_endpoint: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct Workspace {
    sandbox: String,
    cwd: String,
    workdirs: Vec<WorkspaceDirEntry>,
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
    mode: String,
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

#[derive(Clone, serde::Serialize)]
struct RemoteEnvironmentContext {
    sandbox: String,
    cwd: String,
    ssh_server_id: String,
    ssh_target: String,
}

fn build_browser_context(cdp_endpoint: Option<&str>) -> Browser {
    Browser {
        cdp_endpoint: cdp_endpoint.map(str::to_string),
    }
}

fn build_workspace_context(workspace: &MayWorkspace) -> Workspace {
    Workspace {
        sandbox: workspace.sandbox.display().to_string(),
        cwd: workspace.cwd.display().to_string(),
        workdirs: workspace
            .config
            .workdirs
            .iter()
            .map(WorkspaceDirEntry::from)
            .collect(),
    }
}

fn build_remote_environment_context(
    workspace: &MayWorkspace,
    ssh_server: &crate::config::SshServerConfig,
) -> RemoteEnvironmentContext {
    RemoteEnvironmentContext {
        sandbox: workspace.sandbox.display().to_string(),
        cwd: workspace.cwd.display().to_string(),
        ssh_server_id: ssh_server.name.clone(),
        ssh_target: ssh_server.target.clone(),
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
    format!("{}\n\n{}", MEMORY_CONTEXT_HINT.trim(), memory)
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
        mode: policy.mode().as_str().to_string(),
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

    use super::{
        ProfilePersona, RuntimeSystemContext, Workspace, WorkspaceDirEntry,
        build_memory_context_content,
    };

    #[test]
    fn runtime_system_context_reinstates_memory_and_prunes_absent_blocks() {
        let dir = tempdir().expect("temp dir should be created");
        let memory_path = dir.path().join("MEMORY.md");
        std::fs::write(&memory_path, "remember this")
            .expect("memory file should be written");

        let mut runtime_context =
            RuntimeSystemContext::new(memory_path, "remember this".to_string());
        let mut context = aither_agent::Context::default();
        context.insert_system(&ProfilePersona {
            content: "stale persona".to_string(),
        });
        context.insert_system(&Workspace {
            sandbox: "/tmp/old".to_string(),
            cwd: "/tmp/old".to_string(),
            workdirs: vec![WorkspaceDirEntry {
                path: "/tmp/old".to_string(),
                description: "stale".to_string(),
            }],
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

        let mut runtime_context = RuntimeSystemContext::new(memory_path.clone(), "before".to_string());
        let mut context = aither_agent::Context::default();
        runtime_context.reconcile(&mut context);

        std::fs::write(&memory_path, "after").expect("memory file should be updated");
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
}
