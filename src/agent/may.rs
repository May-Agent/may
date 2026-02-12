//! May agent wrapper around aither-agent.

use std::{borrow::Cow, path::PathBuf};

use async_channel::{Sender, bounded};

use askama::Template;

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::TaskTool;
use aither_agent::webfetch::WebFetchTool;
use aither_agent::websearch::WebSearchTool;
use aither_agent::{
    Agent, AgentError, AgentEvent, AgentKind, BashAgentBuilder, ContextBlock, ContextBlockPriority,
};
use aither_cloud::{
    CloudProvider, claude::Claude, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_core::image::{ImageGenerator, Prompt, Size};
use aither_core::llm::{Tool, ToolOutput};
use aither_sandbox::{
    BashMode, BashTool, PermissionHandler, ShellRuntimeAvailability, SshServer,
    permission::PermissionError,
};
use executor_core::DefaultExecutor;
use futures_lite::{Stream, StreamExt};

use crate::ask_user::{AskUserBroker, AskUserTool};
use crate::config::{
    LoadBalanceStrategy, MayConfig, ProviderInstance, ProviderType, SelectedProvider,
};
use crate::copilot::clear_copilot_credentials;
use crate::fs_util;
use crate::models::fetch_models_by_provider;
use crate::session::SessionId;
use crate::workspace::{Workspace, WorkspaceDir};
use crate::workspace_request::{RequestWorkspaceTool, WorkspaceRequestBroker};

// Error message templates
const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");
const ERR_NO_PROVIDER_FMT: &str = include_str!("../texts/agent_err_no_provider.txt");
const ERR_API_KEY_FMT: &str = include_str!("../texts/agent_err_api_key_required.txt");

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
}

/// Permission handler for May agent.
///
/// Allows sandboxed mode by default, network mode with approval,
/// and unsafe mode requires explicit user approval.
#[derive(Debug, Clone)]
pub struct MayPermissionHandler {
    mode: PermissionMode,
}

impl MayPermissionHandler {
    pub fn new() -> Self {
        Self {
            mode: PermissionMode::Tui,
        }
    }

    pub fn with_webui(approver: PermissionApprover) -> Self {
        Self {
            mode: PermissionMode::WebUi(approver),
        }
    }
}

impl Default for MayPermissionHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionHandler for MayPermissionHandler {
    async fn check(&self, mode: BashMode, script: &str) -> Result<bool, PermissionError> {
        match (&self.mode, mode) {
            (_, BashMode::Sandboxed) => Ok(true),
            (PermissionMode::Tui, BashMode::Network) => Ok(true),
            (PermissionMode::Tui, BashMode::Unsafe) => Ok(false),
            (PermissionMode::WebUi(approver), BashMode::Network) => {
                if approver.network_approved() {
                    Ok(true)
                } else {
                    approver.request_and_wait(mode, script.to_string()).await
                }
            }
            (PermissionMode::WebUi(approver), BashMode::Unsafe) => {
                approver.request_and_wait(mode, script.to_string()).await
            }
        }
    }

    async fn check_domain(&self, _domain: &str, _port: u16) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
enum PermissionMode {
    Tui,
    WebUi(PermissionApprover),
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRequestMode {
    Sandboxed,
    Network,
    Unsafe,
}

impl From<BashMode> for PermissionRequestMode {
    fn from(value: BashMode) -> Self {
        match value {
            BashMode::Sandboxed => Self::Sandboxed,
            BashMode::Network => Self::Network,
            BashMode::Unsafe => Self::Unsafe,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionRequest {
    pub id: String,
    pub mode: PermissionRequestMode,
    pub script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PermissionApprover {
    pending_queue:
        std::sync::Arc<tokio::sync::Mutex<std::collections::VecDeque<PermissionRequest>>>,
    pending: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, (PermissionRequest, Sender<bool>)>>,
    >,
    pending_notify: std::sync::Arc<tokio::sync::Notify>,
    network_approved: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_id: Option<String>,
}

impl PermissionApprover {
    pub fn new() -> Self {
        Self {
            pending_queue: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            pending: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            pending_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            network_approved: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_id: None,
        }
    }

    pub fn for_session(&self, session_id: impl Into<String>) -> Self {
        let mut cloned = self.clone();
        cloned.session_id = Some(session_id.into());
        cloned
    }

    pub fn network_approved(&self) -> bool {
        self.network_approved
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn request_and_wait(
        &self,
        mode: BashMode,
        script: String,
    ) -> Result<bool, PermissionError> {
        let request = PermissionRequest {
            id: uuid::Uuid::new_v4().to_string(),
            mode: mode.into(),
            script,
            session_id: self.session_id.clone(),
        };

        let (tx, rx) = bounded(1);
        self.pending
            .lock()
            .map_err(|_| PermissionError::Interrupted)?
            .insert(request.id.clone(), (request.clone(), tx));

        self.pending_queue.lock().await.push_back(request);
        self.pending_notify.notify_waiters();

        rx.recv().await.map_err(|_| PermissionError::Interrupted)
    }

    pub async fn next_request_for_session(
        &self,
        session_id: Option<&str>,
    ) -> Option<PermissionRequest> {
        loop {
            {
                let mut queue = self.pending_queue.lock().await;
                if let Some(index) =
                    queue
                        .iter()
                        .position(|req| match (session_id, req.session_id.as_deref()) {
                            (Some(expected), Some(actual)) => expected == actual,
                            (None, Some(_)) => true,
                            _ => false,
                        })
                {
                    return queue.remove(index);
                }
            }

            self.pending_notify.notified().await;
        }
    }

    pub fn respond(&self, request_id: &str, approved: bool) -> bool {
        let Some((request, tx)) = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(request_id))
        else {
            return false;
        };

        if approved && matches!(request.mode, PermissionRequestMode::Network) {
            self.network_approved
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        tx.try_send(approved).is_ok()
    }

    pub fn snapshot_session_runtime(&self, session_id: Option<&str>) -> Option<PermissionRequest> {
        let pending = self.pending.lock().ok()?;
        pending.values().find_map(|(request, _tx)| {
            let belongs = match (session_id, request.session_id.as_deref()) {
                (Some(expected), Some(actual)) => expected == actual,
                _ => false,
            };
            if !belongs {
                return None;
            }
            Some(request.clone())
        })
    }
}

impl Default for PermissionApprover {
    fn default() -> Self {
        Self::new()
    }
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
    /// Maximum iterations.
    pub max_iterations: usize,
    /// Channel for workspace access requests.
    pub workspace_request_broker: WorkspaceRequestBroker,
    /// Channel for ask user requests.
    pub ask_user_broker: AskUserBroker,
    /// Image generation provider and model.
    pub image_generator: Option<ImageGeneratorConfig>,
    /// Preconfigured SSH servers available for `open_shell` ssh backend.
    pub ssh_servers: Vec<crate::config::SshServerConfig>,
    /// Optional web UI permission approver.
    pub permission_approver: Option<PermissionApprover>,
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
        return Err(anyhow::anyhow!(
            ERR_API_KEY_FMT.replace("{provider}", &config.provider_id)
        ));
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
        .provider_for_model(&model)
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
    inner: Agent<CloudProvider, CloudProvider, CloudProvider, ()>,
    session_id: SessionId,
    sandbox_dir: PathBuf,
    provider_id: String,
}

impl MayAgent {
    /// Create a new MayAgent.
    pub async fn new(config: MayAgentConfig) -> Result<Self, MayAgentError> {
        // Create permission handler
        let permission_handler = config
            .permission_approver
            .as_ref()
            .map_or_else(MayPermissionHandler::new, |approver| {
                MayPermissionHandler::with_webui(approver.clone())
            });

        let executor = DefaultExecutor;

        // Create bash tool in sandbox dir (BashTool creates its own working directory inside)
        let memory_file = global_memory_file_path(&config.may_dir);
        let bash_tool: BashTool<MayPermissionHandler, DefaultExecutor> =
            BashTool::new_exact(&config.sandbox_dir, permission_handler, executor)
                .await
                .map_err(|e| MayAgentError::Sandbox(e.to_string()))?
                .with_auto_open_default_session(true)
                .with_writable_paths([memory_file]);

        // Create workspace with permissions
        let workspace = Workspace::new(
            config.sandbox_dir.clone(),
            config.cwd.clone(),
            config.may_dir.clone(),
        );

        let ssh_servers: Vec<SshServer> = config
            .ssh_servers
            .iter()
            .map(|s| SshServer {
                name: s.name.clone(),
                target: s.target.clone(),
            })
            .collect();
        let ssh_enabled = !ssh_servers.is_empty();

        // Read global memory from ~/.may/MEMORY.md.
        let memory_path = global_memory_file_path(&config.may_dir);
        let memory_content = fs_util::read_to_string(&memory_path).await?;

        // Clone LLM for tools that need it before it's moved into the builder
        let fast_llm = config.llm.clone();
        let task_llm = config.llm.clone();

        // Create tools
        let websearch = WebSearchTool::default();
        let webfetch = WebFetchTool::new();
        let ask = AskCommand::new(fast_llm);
        let request_workspace = RequestWorkspaceTool::new(config.workspace_request_broker);
        let ssh_authorizer_broker = config.ask_user_broker.clone();
        let ask_user = AskUserTool::new(config.ask_user_broker);
        let imagine = ImagineTool {
            generator: config.image_generator,
            artifacts_dir: config.sandbox_dir.join("artifacts"),
        };
        let task_tool = TaskTool::new(task_llm).with_builtins();

        let transcript_file = config.sandbox_dir.join("transcript.md");
        let transcript_path = transcript_file.display().to_string();

        let skills_dir = config.may_dir.join("skills");
        let subagents_dir = config.may_dir.join("subagents");

        // Build the agent using a minimal base prompt + structured context blocks.
        // Skills and subagents must be registered before with_default_prompt()
        // so they appear in the system prompt.
        let mut builder = BashAgentBuilder::new(config.llm, bash_tool)
            .tool(websearch)
            .tool(webfetch)
            .tool(ask)
            .tool(request_workspace)
            .tool(ask_user)
            .tool(imagine)
            .with_skills_async(&skills_dir)
            .await
            .with_subagents_async(&subagents_dir)
            .await
            .with_default_prompt()
            .shell_runtime_availability(ShellRuntimeAvailability {
                local: true,
                container: false,
                ssh: ssh_enabled,
            })
            .ssh_servers(ssh_servers)
            .ssh_authorizer(super::ssh_authorizer::MaySshAuthorizer::new(
                ssh_authorizer_broker,
            ))
            .agent_kind(AgentKind::Coding)
            .persona_prompt(include_str!("../../templates/system.md"))
            .transcript_path(transcript_path)
            .transcript(transcript_file)
            .context_block(build_workspace_context_block(&workspace))
            .context_block(build_memory_context_block(&memory_content))
            .context_block(build_workflow_context_block())
            .max_iterations(config.max_iterations);

        // Get the actual working directory before building
        let actual_sandbox_dir = PathBuf::from(builder.sandbox_dir().as_ref());

        // Configure TaskTool with the sandbox directory and add to builder
        let task_tool = task_tool.with_base_dir(actual_sandbox_dir.clone());
        builder = builder.tool(task_tool);

        let agent = builder.build();

        Ok(Self {
            inner: agent,
            session_id: config.session_id,
            sandbox_dir: actual_sandbox_dir,
            provider_id: config.provider_id,
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
        // Get model: use last_model if set, otherwise fallback to Balanced tier
        let model = may_config
            .last_model
            .as_deref()
            .or_else(|| may_config.default_model_for_tier(crate::config::ModelTier::Balanced))
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
    /// This first checks the stored model_providers mapping for a known provider,
    /// falling back to resolver search if not found.
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
        permission_approver: Option<PermissionApprover>,
    ) -> Result<Self, MayAgentError> {
        // First try exact provider-model matches from standard aither /models listing.
        let provider_model_results = fetch_models_by_provider(may_config).await;
        let successful_fetches = provider_model_results
            .iter()
            .filter(|row| row.error.is_none())
            .count();
        let exact_matches = provider_model_results
            .iter()
            .filter(|row| row.models.iter().any(|m| m.id == model))
            .map(|row| row.provider_id.clone())
            .collect::<Vec<_>>();

        let (llm, provider_id) = if !exact_matches.is_empty() {
            let mut pool = exact_matches;
            pool.sort();
            pool.dedup();

            let picked = match may_config.preferences.load_balance_strategy {
                LoadBalanceStrategy::FailoverOnly => pool.first().cloned(),
                LoadBalanceStrategy::Random => {
                    use std::collections::hash_map::RandomState;
                    use std::hash::{BuildHasher, Hasher};
                    let idx = RandomState::new().build_hasher().finish() as usize % pool.len();
                    pool.get(idx).cloned()
                }
                LoadBalanceStrategy::RoundRobin => {
                    // Resolver is recreated per request; use session id hash to distribute consistently.
                    let seed = session_id.as_str().bytes().fold(0usize, |acc, b| {
                        acc.wrapping_mul(131).wrapping_add(b as usize)
                    });
                    let idx = seed % pool.len();
                    pool.get(idx).cloned()
                }
            };

            if let Some(provider_id) = picked {
                if let Some(instance) = may_config.provider.get(&provider_id) {
                    (
                        build_cloud_provider_from_instance(&provider_id, instance, model).await?,
                        provider_id,
                    )
                } else {
                    return Err(MayAgentError::MissingApiKey(
                        ERR_NO_PROVIDER_FMT.replace("{model}", model),
                    ));
                }
            } else {
                return Err(MayAgentError::MissingApiKey(
                    ERR_NO_PROVIDER_FMT.replace("{model}", model),
                ));
            }
        } else if successful_fetches > 0 {
            // We have authoritative /models data from at least one provider.
            // If none advertise this model, do not route by loose capability inference.
            return Err(MayAgentError::MissingApiKey(
                ERR_NO_PROVIDER_FMT.replace("{model}", model),
            ));
        } else {
            // Fallback only when provider model listing is unavailable.
            let resolver = may_config.create_resolver();
            let selected = resolver.select_provider(model).ok_or_else(|| {
                MayAgentError::MissingApiKey(ERR_NO_PROVIDER_FMT.replace("{model}", model))
            })?;
            (
                build_cloud_provider(&selected, model).await?,
                selected.id.to_string(),
            )
        };

        let config = MayAgentConfig {
            llm,
            provider_id,
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            max_iterations: may_config.max_iterations,
            workspace_request_broker,
            ask_user_broker,
            image_generator: resolve_image_generator_config(may_config),
            ssh_servers: may_config.ssh_servers.clone(),
            permission_approver,
        };

        Self::new(config).await
    }

    /// Run a query and stream events.
    pub fn run(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = url::Url>,
    ) -> impl Stream<Item = Result<AgentEvent, AgentError>> + '_ {
        self.inner.run(prompt, attachments)
    }

    /// One-shot query.
    pub async fn query(&mut self, prompt: &str) -> Result<String, AgentError> {
        self.inner.query(prompt).await
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

    /// Get mutable reference to the inner agent.
    pub fn inner_mut(&mut self) -> &mut Agent<CloudProvider, CloudProvider, CloudProvider, ()> {
        &mut self.inner
    }

    /// Clear conversation history.
    pub fn clear_history(&mut self) {
        self.inner.clear_history();
    }

    /// Force-close shell sessions and terminate their running jobs.
    pub async fn close_shell_sessions(&self) {
        self.inner.close_shell_sessions().await;
    }
}

/// Build a CloudProvider from a selected provider and model.
async fn build_cloud_provider(
    selected: &SelectedProvider<'_>,
    model: &str,
) -> Result<CloudProvider, MayAgentError> {
    build_cloud_provider_from_instance(selected.id, selected.instance, model).await
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
        return Err(MayAgentError::MissingApiKey(
            ERR_API_KEY_FMT.replace("{provider}", id),
        ));
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
                    if is_copilot_unauthorized(&e) {
                        if let Err(clear_err) = clear_copilot_credentials() {
                            tracing::warn!(
                                "Failed to clear invalid Copilot credentials: {}",
                                clear_err
                            );
                        }
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
    let user_truncated: String = user_message.chars().take(200).collect();
    let assistant_truncated: String = assistant_response.chars().take(300).collect();

    let prompt = format!(
        "Generate a short, descriptive title (3-6 words) for this conversation. Output ONLY the title, nothing else.\n\nUser: {}\n\nAssistant: {}",
        user_truncated, assistant_truncated
    );

    let request = LLMRequest::new(vec![Message::user(&prompt)]);
    let mut stream = std::pin::pin!(llm.respond(request));
    let mut title = String::new();

    while let Some(result) = stream.next().await {
        if let Ok(event) = result {
            if let aither_core::llm::Event::Text(text) = event {
                title.push_str(&text);
            }
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

fn build_workspace_context_block(workspace: &Workspace) -> ContextBlock {
    #[derive(askama::Template)]
    #[template(
        source = "<workspace>\n<sandbox>{{ sandbox }}</sandbox>\n<cwd>{{ cwd }}</cwd>\n{% if !workdirs.is_empty() %}<approved_workdirs>\n{% for wd in workdirs %}<workdir path=\"{{ wd.path }}\">{{ wd.description }}</workdir>\n{% endfor %}</approved_workdirs>\n{% endif %}</workspace>",
        ext = "xml",
        escape = "none"
    )]
    struct WorkspaceBlockTemplate {
        sandbox: String,
        cwd: String,
        workdirs: Vec<WorkspaceDirView>,
    }

    #[derive(Clone)]
    struct WorkspaceDirView {
        path: String,
        description: String,
    }

    impl From<&WorkspaceDir> for WorkspaceDirView {
        fn from(value: &WorkspaceDir) -> Self {
            Self {
                path: value.path.display().to_string(),
                description: value
                    .description
                    .clone()
                    .unwrap_or_else(|| "User-approved directory".to_string()),
            }
        }
    }

    let workdirs = workspace
        .config
        .workdirs
        .iter()
        .map(WorkspaceDirView::from)
        .collect::<Vec<_>>();

    let template = WorkspaceBlockTemplate {
        sandbox: workspace.sandbox.display().to_string(),
        cwd: workspace.cwd.display().to_string(),
        workdirs,
    };

    ContextBlock::new(
        "workspace",
        template
            .render()
            .unwrap_or_else(|e| panic!("failed to render workspace context block: {e}")),
    )
    .with_priority(ContextBlockPriority::High)
}

fn build_memory_context_block(memory: &str) -> ContextBlock {
    let with_hint = format!(
        "Global memory file path: ~/.may/MEMORY.md\nThis is your persistent cross-session memory source.\nWhen the user asks you to remember/update memory, directly read or edit this file instead of only relying on short-term context.\n\n{}",
        memory
    );
    ContextBlock::new("memory", with_hint).with_priority(ContextBlockPriority::High)
}

fn build_workflow_context_block() -> ContextBlock {
    ContextBlock::new(
        "workflow",
        "Workflow note: task tracking should be file-system based. If tracking is needed, update TODO.md directly with normal file search/replace edits. For large scoped implementation planning, use plan/task flow to align with user before major execution.",
    )
    .with_priority(ContextBlockPriority::High)
}

fn global_memory_file_path(may_dir: &std::path::Path) -> PathBuf {
    let upper = may_dir.join("MEMORY.md");
    if upper.exists() {
        return upper;
    }

    let lower = may_dir.join("memory.md");
    if lower.exists() {
        let _ = std::fs::rename(&lower, &upper);
        if upper.exists() {
            return upper;
        }
    }

    if !upper.exists() {
        let _ = std::fs::write(&upper, "");
    }
    upper
}
