//! May agent wrapper around aither-agent.

use std::{path::PathBuf, sync::Arc};

use aither_agent::{
    Agent, AgentError, AgentEvent, AgentKind, BrowserBackend, CompactResult, HCons,
    TerminalAgentBuilder,
};
use aither_cloud::CloudProvider;
use aither_core::llm::{
    LLMRequest, Message as LlmMessage,
    model::{Parameters, Profile as ModelProfile, ReasoningEffort},
};
use aither_sandbox::{
    ContainerExecHandle, ContainerShellRuntime, ShellRuntimeAvailability, SshServer, TerminalTool,
};
use executor_core::DefaultExecutor;
use futures_lite::{Stream, StreamExt};

use super::hooks::{ContextSnapshotHook, PendingSteerHook};
use super::imagine::{ImageGeneratorConfig, ImagineTool, resolve_image_generator_config};
use super::permission::PermissionApprover;
use super::provider::{RequestParameterizedModel, resolve_provider_runtime};
use super::runtime_context::{
    RuntimeSystemContext, build_base_system_prompt, build_browser_context,
    build_container_environment_context, build_location_context,
    build_macos_host_automation_context, build_memory_context_content, build_native_voice_context,
    build_network_restrictions_context, build_plan_mode_context, build_terminal_runtime_context,
    build_workspace_context, build_workspace_workdirs_context, tag,
};
use crate::agent::browser_tool::BrowserBroker;
use crate::agent::register_tool::RegisterBroker;
use crate::agent::task_tool::TaskRuntimeKind;
use crate::ask_user::{AskUserBroker, AskUserTool};
use crate::config::MayConfig;
use crate::embedding_provider::EmbeddingModelResolution;
use crate::network_policy::CompiledNetworkPolicy;
use crate::session::{SessionId, SessionPaths, SessionPlanState};
use crate::terminal_commands::{SharedTerminalCommandConfig, register_shared_terminal_commands};
use crate::terminal_permission_handler::TerminalPermissionHandler;
use crate::workspace::Workspace as MayWorkspace;
use crate::workspace_request::WorkspaceRequestBroker;

const ERR_NO_MODELS: &str = include_str!("../texts/agent_err_no_models.txt");

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

/// Execution environment for a `MayAgent`.
///
/// Only `Local` and `Container` are valid home environments. SSH servers
/// attached to the profile are surfaced through `MayAgentConfig::ssh_servers`
/// and reached per-command via the `terminal` tool's `mode: "ssh"` +
/// `ssh_server_id: <name>` parameters. There is no remote-home variant: remote
/// is a per-invocation routing choice, not a session-level environment.
pub enum AgentEnvironment {
    /// Native macOS sandbox via heel.
    Local,
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
    /// Database handle for runtime memory/state lookups.
    pub db: crate::db::ChatDb,
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

        let ssh_servers: Vec<SshServer> = config
            .ssh_servers
            .iter()
            .map(|s| SshServer {
                name: s.name.clone(),
                target: s.target.clone(),
            })
            .collect();
        let ssh_enabled = !ssh_servers.is_empty();

        // Read global memory from the database.
        let memory_content = config
            .db
            .get_global_memory()
            .await
            .unwrap_or_else(|error| {
                tracing::error!(error = %error, "failed to load global memory from DB");
                None
            })
            .unwrap_or_default();
        let mut runtime_system_context =
            RuntimeSystemContext::new(config.db.clone(), memory_content.clone());
        let terminal_runtime = build_terminal_runtime_context(
            &config,
            &session_paths,
            aither_models::lookup(config.model_id.as_str()),
            config.ssh_servers.clone(),
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

        // Shell runtime depends on the home environment. SSH remains a
        // per-command routing capability on top of Local (it cannot be the
        // session-level runtime here).
        let availability = match &config.environment {
            AgentEnvironment::Container { .. } => ShellRuntimeAvailability {
                local: false,
                container: true,
                ssh: ssh_enabled,
            },
            AgentEnvironment::Local => ShellRuntimeAvailability {
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
            .system_text(tag::MEMORY, build_memory_context_content(&memory_content))
            .system_text(tag::TERMINAL_RUNTIME_CONTEXT, terminal_runtime)
            .max_iterations(effective_max_iterations);

        if let Some(voice_hint) = build_native_voice_context(config.native_voice_input) {
            runtime_system_context =
                runtime_system_context.with_native_voice_input(voice_hint.clone());
            builder = builder.system_text(tag::NATIVE_VOICE_INPUT, voice_hint);
        }

        if let Some(persona_content) = config.profile_persona.clone() {
            runtime_system_context =
                runtime_system_context.with_profile_persona(persona_content.clone());
            builder = builder.system_text(tag::PROFILE_PERSONA, persona_content);
        }

        if let Some(ref memories) = config.profile_memories
            && !memories.is_empty()
        {
            let profile_memories = memories.to_context_string();
            runtime_system_context =
                runtime_system_context.with_profile_memories(profile_memories.clone());
            builder = builder.system_text(tag::PROFILE_MEMORIES, profile_memories);
        }

        if let Some(plan_mode) = build_plan_mode_context(config.plan_state) {
            runtime_system_context = runtime_system_context.with_plan_mode(plan_mode.clone());
            builder = builder.system_text(tag::PLAN_MODE, plan_mode);
        }

        if let Some(network_restrictions) =
            build_network_restrictions_context(config.network_policy.as_ref())
        {
            runtime_system_context =
                runtime_system_context.with_network_restrictions(network_restrictions.clone());
            builder = builder.system_text(tag::NETWORK_RESTRICTIONS, network_restrictions);
        }

        if let Some(ref location) = config.location {
            let location_context = build_location_context(location);
            runtime_system_context = runtime_system_context.with_location(location_context.clone());
            builder = builder.system_text(tag::LOCATION_CONTEXT, location_context);
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
                builder =
                    builder.system_text(tag::CONTAINER_ENVIRONMENT_CONTEXT, container_context);
                builder = builder.container_runtime(ContainerShellRuntime::new(
                    container_id,
                    container_ipc_host,
                    container_exec,
                ));
                if let Some(browser_backend) = browser_backend {
                    let endpoint = browser_backend.cdp_endpoint().await.map_err(|error| {
                        MayAgentError::Sandbox(format!(
                            "failed to resolve browser CDP endpoint from backend: {error}"
                        ))
                    })?;
                    browser_cdp_endpoint = Some(endpoint);
                }
            }
            AgentEnvironment::Local => {
                let workspace_context = build_workspace_context(&workspace);
                runtime_system_context =
                    runtime_system_context.with_workspace(workspace_context.clone());
                builder = builder.system_text(tag::WORKSPACE, workspace_context);
                if let Some(workdirs_context) = build_workspace_workdirs_context(&workspace) {
                    runtime_system_context =
                        runtime_system_context.with_workspace_workdirs(workdirs_context.clone());
                    builder = builder.system_text(tag::WORKSPACE_WORKDIRS, workdirs_context);
                }
                if let Some(macos_hint) = build_macos_host_automation_context() {
                    runtime_system_context =
                        runtime_system_context.with_platform(macos_hint.clone());
                    builder = builder.system_text(tag::PLATFORM, macos_hint);
                }
            }
        }

        let browser_context =
            build_browser_context(browser_enabled, browser_cdp_endpoint.as_deref());
        runtime_system_context = runtime_system_context.with_browser(browser_context.clone());
        builder = builder.system_text(tag::BROWSER, browser_context);

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

    pub async fn from_config_with_model_and_permissions(
        may_config: &MayConfig,
        sandbox_dir: PathBuf,
        may_dir: PathBuf,
        cwd: PathBuf,
        session_id: SessionId,
        workspace_request_broker: WorkspaceRequestBroker,
        ask_user_broker: AskUserBroker,
        db: crate::db::ChatDb,
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

        let ssh_servers = crate::profile::resolve_attached_ssh_servers(
            &db,
            profile_id.as_deref(),
            &may_config.ssh_servers,
        )
        .await
        .map_err(|error| {
            MayAgentError::Sandbox(format!(
                "failed to load SSH attachments for profile {profile_id:?}: {error}"
            ))
        })?;

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
            ssh_servers,
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
            db,
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
    ) -> Result<aither_agent::AgentCheckpoint, AgentError> {
        self.inner.export_checkpoint().await
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
        self.runtime_system_context.sync_memory_content().await;
        self.runtime_system_context
            .reconcile(self.inner.context_mut());
    }
}

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
    let llm =
        super::provider::build_cloud_provider_from_instance(selected.id, selected.instance, model)
            .await
            .ok()?;

    // Truncate messages for prompt
    let user_truncated = truncate_at_char_boundary(user_message, 200);
    let assistant_truncated = truncate_at_char_boundary(assistant_response, 300);

    let prompt = format!(
        "Generate a short, descriptive title (3-6 words) for this conversation. Output ONLY the title, nothing else.\n\nUser: {user_truncated}\n\nAssistant: {assistant_truncated}"
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
