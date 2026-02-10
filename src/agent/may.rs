//! May agent wrapper around aither-agent.

use std::path::PathBuf;

use askama::Template;

use aither_agent::sandbox::builtin::AskCommand;
use aither_agent::specialized::TaskTool;
use aither_agent::webfetch::WebFetchTool;
use aither_agent::websearch::WebSearchTool;
use aither_agent::{
    Agent, AgentError, AgentEvent, AgentKind, BashAgentBuilder, BuiltinToolHint, ContextBlock,
    ContextBlockPriority, TodoTool,
};
use aither_cloud::{
    CloudProvider, claude::Claude, copilot::Copilot, gemini::Gemini, openai::OpenAI,
};
use aither_sandbox::{
    BashMode, BashTool, PermissionHandler, ShellRuntimeAvailability, SshServer,
    permission::PermissionError,
};
use executor_core::DefaultExecutor;
use futures_lite::Stream;

use crate::ask_user::{AskUserBroker, AskUserTool};
use crate::config::{MayConfig, ProviderInstance, ProviderType, SelectedProvider};
use crate::copilot::clear_copilot_credentials;
use crate::fs_util;
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
    /// Whether network mode has been approved (for future TUI integration).
    #[allow(dead_code)]
    network_approved: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MayPermissionHandler {
    pub fn new() -> Self {
        Self {
            network_approved: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Approve network mode (for future TUI integration).
    #[allow(dead_code)]
    pub fn approve_network(&self) {
        self.network_approved
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for MayPermissionHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionHandler for MayPermissionHandler {
    async fn check(&self, mode: BashMode, _script: &str) -> Result<bool, PermissionError> {
        match mode {
            BashMode::Sandboxed => Ok(true),
            BashMode::Network => {
                // For now, auto-approve network mode
                // In a real TUI, this would prompt the user
                Ok(true)
            }
            BashMode::Unsafe => {
                // Unsafe mode should be handled by the TUI
                // For library usage, deny by default
                Ok(false)
            }
        }
    }

    async fn check_domain(&self, _domain: &str, _port: u16) -> bool {
        // Allow all domains when in network mode
        true
    }
}

/// Configuration for creating a MayAgent.
pub struct MayAgentConfig {
    /// The LLM provider.
    pub llm: CloudProvider,
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
    /// Preconfigured SSH servers available for `open_shell` ssh backend.
    pub ssh_servers: Vec<crate::config::SshServerConfig>,
}

/// The May agent.
pub struct MayAgent {
    inner: Agent<CloudProvider, CloudProvider, CloudProvider, ()>,
    session_id: SessionId,
    sandbox_dir: PathBuf,
}

impl MayAgent {
    /// Create a new MayAgent.
    pub async fn new(config: MayAgentConfig) -> Result<Self, MayAgentError> {
        // Create permission handler
        let permission_handler = MayPermissionHandler::new();

        let executor = DefaultExecutor;

        // Create bash tool in sandbox dir (BashTool creates its own working directory inside)
        let memory_file = config.may_dir.join("memory.md");
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

        // Read global memory from ~/.may/memory.md.
        let memory_path = config.may_dir.join("memory.md");
        let memory_content = fs_util::read_to_string(&memory_path).await?;

        // Clone LLM for tools that need it before it's moved into the builder
        let fast_llm = config.llm.clone();
        let task_llm = config.llm.clone();

        // Create tools
        let websearch = WebSearchTool::default();
        let webfetch = WebFetchTool::new();
        let todo = TodoTool::new();
        let ask = AskCommand::new(fast_llm);
        let request_workspace = RequestWorkspaceTool::new(config.workspace_request_broker);
        let ssh_authorizer_broker = config.ask_user_broker.clone();
        let ask_user = AskUserTool::new(config.ask_user_broker);
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
            .tool(todo)
            .tool(ask)
            .tool(request_workspace)
            .tool(ask_user)
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
            .builtin_tool_hint(BuiltinToolHint::new(
                "ask",
                "Fast side-analysis for piped content; use `ask --help` for usage.",
            ))
            .builtin_tool_hint(BuiltinToolHint::new(
                "task",
                "Spawn a subagent for isolated complex work; use `task --help`.",
            ))
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
        // Check if we have a stored provider mapping for this model
        if let Some(provider_id) = may_config.models.provider_for_model(model) {
            if let Some(instance) = may_config.provider.get(provider_id) {
                // Use the stored provider directly - no need to search
                let selected = SelectedProvider {
                    id: provider_id,
                    instance,
                };
                let llm = build_cloud_provider(&selected, model).await?;

                let config = MayAgentConfig {
                    llm,
                    sandbox_dir,
                    may_dir,
                    cwd,
                    session_id,
                    max_iterations: may_config.max_iterations,
                    workspace_request_broker: workspace_request_broker.clone(),
                    ask_user_broker: ask_user_broker.clone(),
                    ssh_servers: may_config.ssh_servers.clone(),
                };

                return Self::new(config).await;
            }
        }

        // Fallback: use the provider resolver to find a provider that can serve the model
        let resolver = may_config.create_resolver();
        let selected = resolver.select_provider(model).ok_or_else(|| {
            MayAgentError::MissingApiKey(ERR_NO_PROVIDER_FMT.replace("{model}", model))
        })?;

        // Build the CloudProvider
        let llm = build_cloud_provider(&selected, model).await?;

        let config = MayAgentConfig {
            llm,
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            max_iterations: may_config.max_iterations,
            workspace_request_broker,
            ask_user_broker,
            ssh_servers: may_config.ssh_servers.clone(),
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
    ContextBlock::new("memory", memory).with_priority(ContextBlockPriority::Low)
}
