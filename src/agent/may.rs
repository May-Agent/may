//! May agent wrapper around aither-agent.

use std::path::PathBuf;
use std::sync::Arc;

use aither_agent::{
    Agent, AgentError, AgentEvent, AgentKind, CompactResult, HCons, TerminalAgentBuilder,
};
use aither_cloud::CloudProvider;
use aither_core::llm::{
    Attachment as LlmAttachment, Message as LlmMessage,
    model::{Parameters, ReasoningEffort},
};
use aither_sandbox::{AnyContainerExec, ContainerShellRuntime, SshServer, TerminalTool};
use executor_core::DefaultExecutor;
use futures_lite::Stream;

use super::hooks::{ContextSnapshotHook, PendingSteerHook};
use super::imagine::{ImageGeneratorConfig, resolve_image_generator_config};
use super::permission::PermissionApprover;
use super::provider::{RequestParameterizedModel, resolve_provider_runtime};
use super::runtime_context::{
    RuntimeSystemContext, build_base_system_prompt, build_browser_context,
    build_container_environment_context, build_location_context,
    build_macos_host_automation_context, build_memory_context_content, build_native_voice_context,
    build_network_restrictions_context, build_plan_mode_context, build_terminal_runtime_context,
    build_workspace_context, build_workspace_workdirs_context, tag,
};
use super::tool_environment_cache::{
    Acquired, HomeBackend, ToolEnvironmentCache, ToolEnvironmentCacheError, ToolEnvironmentKey,
};
use crate::agent::browser_tool::BrowserBroker;
use crate::agent::register_tool::RegisterBroker;
use crate::agent::task_tool::TaskRuntimeKind;
use crate::ask_user::AskUserBroker;
use crate::config::{MayConfig, NativeToolsConfig, ProviderType};
use crate::embedding_provider::EmbeddingModelResolution;
use crate::network_policy::CompiledNetworkPolicy;
use crate::session::{SessionId, SessionPaths, SessionPlanState};
use crate::terminal_commands::{
    ImagineCommandConfig, SharedTerminalCommandConfig, register_shared_terminal_commands,
};
use crate::terminal_permission_handler::TerminalPermissionHandler;
use crate::workspace::Workspace as MayWorkspace;
use crate::workspace_request::WorkspaceRequestBroker;

/// May agent error.
#[derive(Debug, thiserror::Error)]
pub enum MayAgentError {
    #[error("Config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    /// Baseline preparation failed before the agent became ready.
    #[error("Baseline tool initialization failed: {0}")]
    ToolInitialization(#[from] aither_sandbox::ToolRuntimeError),
    /// The session's prepared tool environment could not be acquired.
    #[error(transparent)]
    ToolEnvironment(#[from] ToolEnvironmentCacheError),
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
    #[error("ChatGPT auth error: {0}")]
    ChatgptAuth(#[from] crate::chatgpt::ChatgptAuthError),
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
        /// Route the container uses to reach the host's IPC bridge.
        container_ipc_route: aither_sandbox::ContainerIpcRoute,
        /// Executor for running commands inside the container.
        container_exec: AnyContainerExec,
        /// Resolved browser CDP endpoint reachable from inside the container.
        browser_cdp_endpoint: Option<String>,
    },
}

/// Configuration for creating a `MayAgent`.
pub struct MayAgentConfig {
    /// Trusted baseline tool distribution used by every configured backend.
    pub tool_distribution: Arc<aither_sandbox::ToolDistribution>,
    /// Prepared tool environments kept across rebuilds of a session's agent.
    pub tool_environments: ToolEnvironmentCache,
    /// Scheduling priority applied to commands this agent runs.
    pub command_priority: crate::config::CommandPriority,
    /// The LLM provider.
    pub llm: CloudProvider,
    /// The concrete model identifier selected for this request.
    pub model_id: String,
    /// Provider ID selected for this request.
    pub provider_id: String,
    /// Optional embedding provider used for semantic retrieval tools.
    pub search_embedder: EmbeddingModelResolution,
    /// Tier-resolved models for the `ask` terminal command.
    pub ask_models: super::provider::AskTierModels,
    /// Session sandbox directory (`TerminalTool` creates working dir inside).
    pub sandbox_dir: PathBuf,
    /// May directory (~/.may/).
    pub may_dir: PathBuf,
    /// Host directory where May was launched (read-only in local environments).
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

        let network_audit = aither_sandbox::open_network_audit_log(
            config.may_dir.join(crate::config::NETWORK_AUDIT_LOG_SUBDIR),
            crate::config::NETWORK_AUDIT_MAX_FILES,
        )
        .await
        .map_err(|e| MayAgentError::Sandbox(e.to_string()))?;

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
            .map_err(|e| MayAgentError::Sandbox(e.to_string()))?
            .with_network_audit_log(network_audit)
            .with_process_priority(config.command_priority.process_priority());

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
        // The cache key and the builder's backend setters are derived from
        // the same values, so an equal key means the cached registry is the
        // one this builder would prepare.
        let tool_environment_key = ToolEnvironmentKey::new(
            match &config.environment {
                AgentEnvironment::Local => HomeBackend::Local,
                AgentEnvironment::Container {
                    container_id,
                    container_ipc_route,
                    ..
                } => HomeBackend::container(container_id, container_ipc_route),
            },
            &ssh_servers,
        );

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
        let wrap_ask_model = |model: Option<CloudProvider>| {
            model.map(|model| {
                RequestParameterizedModel::new(model, config.request_parameters.clone())
            })
        };
        let ask_flagship = wrap_ask_model(config.ask_models.flagship.clone());
        let ask_balanced = wrap_ask_model(config.ask_models.balanced.clone());
        let ask_fast = wrap_ask_model(config.ask_models.fast.clone());

        let ssh_authorizer_broker = config.ask_user_broker.clone();
        let imagine_config = ImagineCommandConfig {
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

        // Build the agent using a minimal base prompt + structured context blocks.
        // Skills and subagents must be registered before with_default_prompt()
        // so they appear in the system prompt.
        let effective_max_iterations = if config.max_iterations == 0 {
            usize::MAX
        } else {
            config.max_iterations
        };
        let mut builder = TerminalAgentBuilder::new(llm, terminal_tool)
            .hook(context_snapshot_hook)
            .hook(pending_steer_hook);
        let terminal_command_factory = builder.terminal_tool_factory();
        let shared_terminal_commands = register_shared_terminal_commands(
            builder.tool_registry_mut(),
            SharedTerminalCommandConfig {
                llm: terminal_command_llm,
                ask_flagship,
                ask_balanced,
                ask_fast,
                session_id: config.session_id.as_str().to_string(),
                may_dir: config.may_dir.clone(),
                sandbox_dir: config.sandbox_dir.clone(),
                search_embedder: config.search_embedder.clone(),
                ask_user_broker: Some(config.ask_user_broker),
                imagine: Some(imagine_config),
                task_runtime: match &config.environment {
                    AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                    AgentEnvironment::Local => TaskRuntimeKind::Local,
                },
                task_broker: config.task_broker.clone(),
                notify_broker: config.notify_broker.clone(),
                browser_broker: config.browser_broker.clone(),
                register_skills_broker: config.register_skills_broker.clone(),
                register_subagents_broker: config.register_subagents_broker.clone(),
                request_parameters: config.request_parameters.clone(),
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
        // The builder's descriptions cover the terminal controls aither
        // registers itself plus every shared command registered above.
        let base_system_prompt =
            build_base_system_prompt(builder.tool_descriptions(), &skills_dir, &subagents_dir)
                .await?;
        builder = builder
            .system_prompt_raw(base_system_prompt)
            // Shell runtime depends on the home environment. SSH remains a
            // per-command routing capability on top of it.
            .shell_runtime_availability(tool_environment_key.availability())
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
                    browser_cdp_endpoint: Some(_),
                    ..
                }
            );
        let mut browser_cdp_endpoint = None;

        // Environment-specific configuration
        match config.environment {
            AgentEnvironment::Container {
                container_id,
                container_ipc_route,
                container_exec,
                browser_cdp_endpoint: cdp_endpoint,
            } => {
                let container_context = build_container_environment_context(session_paths.root());
                runtime_system_context =
                    runtime_system_context.with_container_environment(container_context.clone());
                builder =
                    builder.system_text(tag::CONTAINER_ENVIRONMENT_CONTEXT, container_context);
                builder = builder.container_runtime(ContainerShellRuntime::new(
                    container_id,
                    container_ipc_route,
                    container_exec,
                ));
                browser_cdp_endpoint = cdp_endpoint;
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
        tracing::debug!(
            stage = "tool_preparation",
            "preparing session tool environments"
        );
        let preparation_started = std::time::Instant::now();
        let (builder, outcome) = match config
            .tool_environments
            .acquire(&config.session_id, tool_environment_key)
            .await?
        {
            Acquired::Ready(registry) => (
                // An equal key describes this registry, so a rejection here
                // is a key derivation bug and is reported, never re-prepared.
                builder.prepared_shell_sessions(registry).await?,
                "adopted",
            ),
            Acquired::Prepare(lease) => {
                let builder = builder
                    .initialize_tools(
                        &config.tool_distribution,
                        &crate::session::MayHomePaths::new(config.may_dir.clone()).tools_dir(),
                    )
                    .await
                    .inspect_err(|error| {
                        tracing::error!(
                            stage = "tool_preparation",
                            %error,
                            "tool environment preparation failed"
                        );
                    })?;
                lease.complete(builder.shell_sessions().clone());
                (builder, "prepared")
            }
        };
        tracing::debug!(
            stage = "tool_preparation",
            session_id = %config.session_id,
            outcome,
            elapsed_ms = preparation_started.elapsed().as_millis(),
            "session tool environments ready"
        );
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
        tool_distribution: Arc<aither_sandbox::ToolDistribution>,
        tool_environments: ToolEnvironmentCache,
        provider_attempt: usize,
    ) -> Result<Self, MayAgentError> {
        let resolved_provider =
            resolve_provider_runtime(may_config, model, provider_attempt).await?;
        let provider_id = resolved_provider.provider_id;
        let llm = resolved_provider.llm;
        let model_info = aither_models::lookup(model);
        let mut request_parameters = Parameters::default();
        if let Some(reasoning_effort) = reasoning_effort {
            request_parameters = request_parameters.reasoning_effort(reasoning_effort);
        }

        // Enable provider-hosted web tools only for the resolved provider type:
        // each provider crate reads its own `native_tools` section, and the
        // portable `websearch` flag reaches OpenAI/Gemini only.
        let provider_instance = may_config.provider.get(&provider_id);
        let native = provider_instance.map_or_else(NativeToolsConfig::default, |instance| {
            instance.native_tools.clone()
        });
        let provider_type = provider_instance.map(|instance| instance.resolve_type(&provider_id));
        let model_has_web_search = model_info
            .is_some_and(|info| info.has_ability(aither_core::llm::model::Ability::WebSearch));
        match provider_type {
            Some(ProviderType::Anthropic) => {
                request_parameters.native_tools.claude.web_search =
                    native.web_search && model_has_web_search;
                request_parameters.native_tools.claude.web_fetch = native.web_fetch;
            }
            Some(ProviderType::Google) => {
                request_parameters.websearch = native.web_search && model_has_web_search;
                request_parameters.native_tools.gemini.url_context = native.web_fetch;
            }
            Some(ProviderType::OpenAI) => {
                request_parameters.websearch = native.web_search && model_has_web_search;
            }
            Some(ProviderType::Chatgpt) => {
                // Hosted search is included in the subscription (no per-call
                // billing), so default it on unless native_tools was configured
                // explicitly. The backend's own catalog decides support — the
                // aither-models db has no entries for codex slugs, so the
                // Ability::WebSearch gate would always read false here.
                request_parameters.websearch = native.web_search || native.is_empty();
            }
            _ => {}
        }

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

        let ask_models =
            super::provider::resolve_ask_tier_models(may_config, &provider_id, model).await;
        let config = MayAgentConfig {
            tool_distribution,
            tool_environments,
            command_priority: may_config.sandbox.command_priority,
            llm,
            model_id: model.to_string(),
            provider_id,
            search_embedder: resolved_provider.search_embedder,
            ask_models,
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
                .map_err(|error| MayAgentError::Sandbox(error.to_string()))?,
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
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> impl Stream<Item = Result<AgentEvent, AgentError>> + '_ {
        self.inner.run(prompt, attachments)
    }

    /// Run a query and persist per-turn hidden context as a regular system message.
    pub fn run_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> impl Stream<Item = Result<AgentEvent, AgentError>> + '_ {
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
        self.inner.query(prompt).await
    }

    /// Preview the exact request message sequence for the next turn.
    pub async fn preview_request_messages(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Vec<LlmMessage> {
        self.sync_runtime_context().await;
        self.inner
            .preview_request_messages(prompt, attachments)
            .await
    }

    /// Preview the exact request message sequence for the next turn with hidden context.
    ///
    /// Mirrors [`run_with_hidden_context`](Self::run_with_hidden_context): the
    /// hidden context is pushed as a regular system message at the conversation
    /// tail, the preview is built, then the checkpointed state is restored.
    pub async fn preview_request_messages_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Vec<LlmMessage> {
        self.sync_runtime_context().await;
        let checkpoint = self
            .inner
            .export_checkpoint()
            .await
            .expect("checkpoint export for preview must succeed");
        if let Some(hidden_context) = hidden_context
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            self.inner.push_message(LlmMessage::system(hidden_context));
        }
        let preview = self
            .inner
            .preview_request_messages(prompt, attachments)
            .await;
        if let Err(error) = self.inner.restore_checkpoint(checkpoint) {
            tracing::warn!(%error, "failed to restore checkpoint after preview");
        }
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

    /// Stop every terminal task that is still running for this agent.
    pub async fn kill_running_terminal_jobs(&self) -> usize {
        self.inner.kill_running_terminal_jobs().await
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
    let selector = config
        .default_model_for_tier(crate::config::ModelTier::Fast)
        .or(config.last_model.as_ref())?;
    let provider_id = selector.provider_id();
    let provider = config.get_provider(provider_id)?;

    // Build the cloud provider
    let llm = super::provider::build_cloud_provider_from_instance(
        provider_id,
        provider,
        selector.model_id(),
    )
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
