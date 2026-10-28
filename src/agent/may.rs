//! May agent wrapper around aither-agent.

use std::path::PathBuf;
use std::pin::Pin;
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
use crate::config::MayConfig;
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
    /// Config error.
    #[error("Config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    /// Baseline preparation failed before the agent became ready.
    #[error("Baseline tool initialization failed: {0}")]
    ToolInitialization(#[from] aither_sandbox::ToolRuntimeError),
    /// The session's prepared tool environment could not be acquired.
    #[error(transparent)]
    ToolEnvironment(#[from] ToolEnvironmentCacheError),
    /// I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// LLM error.
    #[error("LLM error: {0}")]
    Llm(String),
    /// Sandbox error.
    #[error("Sandbox error: {0}")]
    Sandbox(String),
    /// Agent error.
    #[error("Agent error: {0}")]
    Agent(#[from] AgentError),
    /// Missing API key.
    #[error("Missing API key: {0}")]
    MissingApiKey(String),
    /// Copilot token error.
    #[error("Copilot token error: {0}")]
    CopilotToken(#[from] crate::copilot::CopilotTokenError),
    /// `ChatGPT` auth error.
    #[error("ChatGPT auth error: {0}")]
    ChatgptAuth(#[from] crate::chatgpt::ChatgptAuthError),
    /// Workspace config error.
    #[error("Workspace config error: {0}")]
    Workspace(#[from] crate::workspace::WorkspaceConfigError),
    /// MCP error.
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
#[derive(Debug)]
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
#[derive(Debug)]
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
    /// Extra launch-granted directories mounted read-write (`--add-dir`).
    pub extra_workspace_dirs: Vec<PathBuf>,
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

/// Session-scoped inputs for [`MayAgent::from_config_with_model_and_permissions`].
#[derive(Debug)]
pub struct MayAgentSessionInput<'a> {
    /// Session sandbox directory (`TerminalTool` creates a working dir inside).
    pub sandbox_dir: PathBuf,
    /// May directory (`~/.may/`).
    pub may_dir: PathBuf,
    /// Host directory where May was launched.
    pub cwd: PathBuf,
    /// Session ID.
    pub session_id: SessionId,
    /// Channel for workspace access requests.
    pub workspace_request_broker: WorkspaceRequestBroker,
    /// Channel for ask-user requests.
    pub ask_user_broker: AskUserBroker,
    /// Database handle for runtime memory/state lookups.
    pub db: crate::db::ChatDb,
    /// Concrete model identifier selected for this request.
    pub model: &'a str,
    /// Reasoning effort override, if any.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    /// Optional permission approver handling interactive approval.
    pub permission_approver: Option<PermissionApprover>,
    /// Extra launch-granted directories mounted read-write (`--add-dir`).
    pub extra_workspace_dirs: Vec<PathBuf>,
    /// Optional profile persona overlay injected into the agent context.
    pub profile_persona: Option<String>,
    /// Optional profile memories injected as read-only context.
    pub profile_memories: Option<crate::memory::ProfileMemories>,
    /// Session-scoped planning workflow state.
    pub plan_state: SessionPlanState,
    /// Resolved location info, when the profile is location-aware.
    pub location: Option<crate::location::LocationInfo>,
    /// Channel for scheduled-task create requests.
    pub task_broker: Option<crate::agent::task_tool::TaskBroker>,
    /// Channel for notification requests.
    pub notify_broker: Option<crate::agent::notify_tool::NotifyBroker>,
    /// Channel for browser requests.
    pub browser_broker: Option<BrowserBroker>,
    /// Channel for register-skills requests.
    pub register_skills_broker: Option<RegisterBroker>,
    /// Channel for register-subagents requests.
    pub register_subagents_broker: Option<RegisterBroker>,
    /// Execution environment (local heel sandbox or container).
    pub environment: AgentEnvironment,
    /// Trusted baseline tool distribution used by every configured backend.
    pub tool_distribution: Arc<aither_sandbox::ToolDistribution>,
    /// Prepared tool environments kept across agent rebuilds.
    pub tool_environments: ToolEnvironmentCache,
    /// Which provider candidate this build should try (retry counter).
    pub provider_attempt: usize,
}

type MayAgentInner = Agent<
    RequestParameterizedModel<CloudProvider>,
    RequestParameterizedModel<CloudProvider>,
    RequestParameterizedModel<CloudProvider>,
    HCons<PendingSteerHook, HCons<ContextSnapshotHook, ()>>,
>;

/// The May agent.
pub struct MayAgent {
    inner: MayAgentInner,
    session_id: SessionId,
    sandbox_dir: PathBuf,
    provider_id: String,
    model_id: String,
    model_context_window: Option<u32>,
    /// IPC command names backed by MCP connections — lets `/context` split
    /// the `<commands>` index into built-in vs MCP rows.
    mcp_command_names: Vec<String>,
    runtime_system_context: RuntimeSystemContext,
    job_registry: aither_sandbox::JobRegistry,
    subagent_runs: aither_agent::specialized::SubagentRuns,
    /// Model clone reserved for `/btw` side questions.
    btw_llm: RequestParameterizedModel<CloudProvider>,
    subagent_tracker: crate::agent::subagent_tracker::SubagentTracker,
}

impl std::fmt::Debug for MayAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MayAgent")
            .field("session_id", &self.session_id)
            .field("sandbox_dir", &self.sandbox_dir)
            .field("provider_id", &self.provider_id)
            .field("model_id", &self.model_id)
            .field("model_context_window", &self.model_context_window)
            .finish_non_exhaustive()
    }
}

impl MayAgent {
    /// Create a new `MayAgent`.
    ///
    /// # Errors
    ///
    /// Returns [`MayAgentError`] when the sandbox environment cannot be
    /// prepared — opening the network audit log, creating the terminal tool,
    /// or registering the skills/subagents directories fails.
    pub async fn new(config: MayAgentConfig) -> Result<Self, MayAgentError> {
        let is_container = matches!(config.environment, AgentEnvironment::Container { .. });

        let executor = DefaultExecutor;

        let network_audit = aither_sandbox::open_network_audit_log(
            config.may_dir.join(crate::config::NETWORK_AUDIT_LOG_SUBDIR),
            crate::config::NETWORK_AUDIT_MAX_FILES,
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

        // Mount workspace grants into the terminal sandbox: read-only dirs
        // become readable mounts, read-write dirs writable mounts; cwd lands
        // in the read-write set because `MayWorkspace::new` trusts it.
        let readable = workspace
            .config
            .workdirs
            .iter()
            .filter(|dir| dir.permission == crate::workspace::Permission::ReadOnly)
            .map(|dir| dir.path.clone());
        let writable = workspace
            .config
            .workdirs
            .iter()
            .filter(|dir| dir.permission == crate::workspace::Permission::ReadWrite)
            .map(|dir| dir.path.clone())
            .chain(config.extra_workspace_dirs.iter().cloned());

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
            .with_process_priority(config.command_priority.process_priority())
            .with_readable_paths(readable)
            .with_writable_paths(writable);
        let job_registry = terminal_tool.job_registry();
        let subagent_runs = aither_agent::specialized::SubagentRuns::default();

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
        // `/btw` side questions run through this clone — separate from the
        // agent's own model so a side query never joins the agent's history.
        let btw_llm = llm.clone();
        // Ask-tier models arrive already wrapped in the parameters of their
        // own provider instances.
        let ask_flagship = config.ask_models.flagship.clone();
        let ask_balanced = config.ask_models.balanced.clone();
        let ask_fast = config.ask_models.fast.clone();

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
        let subagent_tracker = crate::agent::subagent_tracker::SubagentTracker::default();
        let shared_terminal_commands = register_shared_terminal_commands(
            builder.tool_registry_mut(),
            SharedTerminalCommandConfig {
                llm: terminal_command_llm,
                subagent_tracker: subagent_tracker.clone(),
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
                subagent_runs: subagent_runs.clone(),
            },
        )
        .await?;
        for (name, description) in &shared_terminal_commands.descriptions {
            builder = builder.tool_description(name.clone(), description.clone());
        }
        if is_container {
            builder = builder.with_skills_readable_only(&skills_dir).await?;
            builder = builder.with_subagents_readable_only(&subagents_dir).await?;
        } else {
            builder = builder.with_skills(&skills_dir).await?;
            builder = builder.with_subagents(&subagents_dir).await?;
        }
        // The builder's descriptions cover every shared command registered
        // above; the native tool surface comes from the same builder.
        let native_tool_definitions = builder.native_tool_definitions();
        let base_system_prompt = build_base_system_prompt(
            builder.tool_descriptions(),
            &native_tool_definitions,
            &config.request_parameters,
            &skills_dir,
            &subagents_dir,
        )
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
                builder.prepared_shell_sessions(*registry).await?,
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
            model_id: config.model_id,
            model_context_window: config.model_context_window,
            mcp_command_names: shared_terminal_commands.mcp_command_names,
            runtime_system_context,
            job_registry,
            subagent_runs,
            btw_llm,
            subagent_tracker,
        })
    }

    /// Create a `MayAgent` for a session from the May config plus the
    /// session-scoped inputs in `input`.
    ///
    /// # Errors
    ///
    /// Returns [`MayAgentError`] when resolving the provider runtime fails,
    /// the SSH attachments for the profile cannot be loaded, or
    /// [`MayAgent::new`] fails.
    pub async fn from_config_with_model_and_permissions(
        may_config: &MayConfig,
        input: MayAgentSessionInput<'_>,
    ) -> Result<Self, MayAgentError> {
        let MayAgentSessionInput {
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            workspace_request_broker,
            ask_user_broker,
            db,
            model,
            reasoning_effort,
            profile_id,
            permission_approver,
            extra_workspace_dirs,
            profile_persona,
            profile_memories,
            plan_state,
            location,
            task_broker,
            notify_broker,
            browser_broker,
            register_skills_broker,
            register_subagents_broker,
            environment,
            tool_distribution,
            tool_environments,
            provider_attempt,
        } = input;
        let resolved_provider =
            resolve_provider_runtime(may_config, model, provider_attempt).await?;
        let provider_id = resolved_provider.provider_id;
        let llm = resolved_provider.llm;
        let model_info = aither_models::lookup(model);
        let request_parameters = super::provider::request_parameters_for(
            &provider_id,
            may_config.provider.get(&provider_id),
            model,
            reasoning_effort,
        );

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
            super::provider::resolve_ask_tier_models(may_config, &provider_id, model).await?;
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
            extra_workspace_dirs,
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

        Box::pin(Self::new(config)).await
    }

    /// Run a query and stream events.
    ///
    /// The run's state machine spans every agent turn and is tens of
    /// kilobytes, so it is built on the heap here, in a frame that has
    /// returned before the stream is first polled. A caller that pinned it in
    /// place would carry it in its own future and, in debug builds, in the
    /// stack frame of every poll down to the provider request.
    pub fn run(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Pin<Box<impl Stream<Item = Result<AgentEvent, AgentError>> + '_>> {
        Box::pin(self.inner.run(prompt, attachments))
    }

    /// Run a query and persist per-turn hidden context as a regular system
    /// message. The stream is boxed for the reason given on [`Self::run`].
    pub fn run_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Pin<Box<impl Stream<Item = Result<AgentEvent, AgentError>> + '_>> {
        if let Some(hidden_context) = hidden_context
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            self.inner.push_message(LlmMessage::system(hidden_context));
        }
        self.run(prompt, attachments)
    }

    /// One-shot query.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the underlying agent run fails.
    pub async fn query(&mut self, prompt: &str) -> Result<String, AgentError> {
        self.sync_runtime_context().await;
        Box::pin(self.inner.query(prompt)).await
    }

    /// Preview the exact request message sequence for the next turn.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the model profiles cannot be fetched
    /// during initialization.
    pub async fn preview_request_messages(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Result<Vec<LlmMessage>, AgentError> {
        self.sync_runtime_context().await;
        self.inner
            .preview_request_messages(prompt, attachments)
            .await
    }

    /// Preview the exact request message sequence for the next turn with hidden context.
    ///
    /// Mirrors [`run_with_hidden_context`](Self::run_with_hidden_context): the
    /// hidden context is appended as a regular system message at the
    /// conversation tail. The preview is assembled against a fork of the
    /// context installed for the duration of the call, so the live
    /// conversation is never mutated.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the model profiles cannot be fetched
    /// during initialization.
    pub async fn preview_request_messages_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Result<Vec<LlmMessage>, AgentError> {
        self.sync_runtime_context().await;
        // aither populates the static system blocks into `Agent::context`
        // during the first request assembly; they must land on the live
        // context, not on the fork that is discarded below. `profile` is set
        // by that same first assembly, so a missing profile means the context
        // has not been initialized yet.
        if self.inner.profile().is_none() {
            self.inner.snapshot_context_window().await?;
        }
        let mut preview_context = self.inner.context().fork();
        if let Some(hidden_context) = hidden_context
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            preview_context.push(LlmMessage::system(hidden_context));
        }
        std::mem::swap(self.inner.context_mut(), &mut preview_context);
        let preview = self
            .inner
            .preview_request_messages(prompt, attachments)
            .await;
        std::mem::swap(self.inner.context_mut(), &mut preview_context);
        preview
    }

    /// Measure the context window for the `/context` command.
    ///
    /// Runtime system blocks are reconciled first, so the report describes
    /// the same state the next request would be assembled from. Token counts
    /// are estimates at ~4 bytes per token — the heuristic the agent itself
    /// uses for context budgeting.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the model profiles cannot be fetched
    /// during initialization.
    pub async fn context_report(
        &mut self,
    ) -> Result<super::context_report::ContextReport, AgentError> {
        self.sync_runtime_context().await;
        let snapshot = self.inner.snapshot_context_window().await?;
        Ok(super::context_report::ContextReport::assemble(
            &self.model_id,
            &self.provider_id,
            &self.mcp_command_names,
            self.inner.context(),
            &snapshot.metrics,
            snapshot.phase,
        ))
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

    /// Background-job registry shared with this session's terminal tool.
    /// Cloneable handle — the TUI polls it for `/jobs` and kills by pid.
    #[must_use]
    pub fn job_registry(&self) -> aither_sandbox::JobRegistry {
        self.job_registry.clone()
    }

    /// Registry of in-flight `subagent` command runs.
    /// Cloneable handle — the TUI lists and cancels individual runs.
    #[must_use]
    pub fn subagent_runs(&self) -> aither_agent::specialized::SubagentRuns {
        self.subagent_runs.clone()
    }

    /// Model clone for `/btw` side questions — shares the agent's provider
    /// but runs outside the agent, so answers never enter its history.
    #[must_use]
    pub fn btw_llm(&self) -> RequestParameterizedModel<CloudProvider> {
        self.btw_llm.clone()
    }

    /// Tracker shared with this session's `subagent` IPC command — the TUI
    /// reads live per-run stats off it for the jobs panel and status bar.
    #[must_use]
    pub fn subagent_tracker(&self) -> crate::agent::subagent_tracker::SubagentTracker {
        self.subagent_tracker.clone()
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
        Box::pin(self.inner.compact(focus)).await
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

/// Builds a `MayAgent` around the given model for tests. Hooks are
/// constructed without a live database, which makes them no-ops.
#[cfg(test)]
pub async fn test_agent(
    llm: RequestParameterizedModel<CloudProvider>,
    provider_id: &str,
    model_id: &str,
) -> (MayAgent, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = crate::db::ChatDb::open(&temp.path().join("history.db"))
        .await
        .expect("open test db");
    let session_id = SessionId::new();
    let inner = Agent::builder(llm.clone())
        .hook(ContextSnapshotHook::new(None, &session_id))
        .hook(PendingSteerHook::new(None, &session_id))
        .build();
    let (job_registry, service) = aither_sandbox::job_registry::job_registry_channel();
    tokio::spawn(service.serve());
    (
        MayAgent {
            inner,
            session_id,
            job_registry,
            subagent_runs: aither_agent::specialized::SubagentRuns::default(),
            btw_llm: llm,
            sandbox_dir: temp.path().join("sandbox"),
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            model_context_window: None,
            mcp_command_names: Vec::new(),
            runtime_system_context: RuntimeSystemContext::new(db, String::new()),
            subagent_tracker: crate::agent::subagent_tracker::SubagentTracker::default(),
        },
        temp,
    )
}

#[cfg(test)]
mod tests {
    use aither_core::llm::Role;

    use super::*;

    /// A `Codex` provider — its `profile()` is a static `aither-models`
    /// lookup, so initialization never touches the network.
    fn codex_llm() -> RequestParameterizedModel<CloudProvider> {
        RequestParameterizedModel::new(
            CloudProvider::Codex(aither_cloud::codex::Codex::new(
                aither_cloud::codex::auth::CodexCredentials::new("test-access", "test-refresh"),
                "gpt-5",
            )),
            Parameters::default(),
        )
    }

    #[tokio::test]
    async fn preview_with_hidden_context_leaves_conversation_untouched() {
        let (mut agent, _temp) = test_agent(codex_llm(), "chatgpt", "gpt-5").await;
        agent.push_message(LlmMessage::user("first turn"));
        agent.push_message(LlmMessage::assistant("first answer"));
        let before = agent.inner.history();

        let preview = agent
            .preview_request_messages_with_hidden_context(
                "second turn",
                Some("hidden facts"),
                Vec::<LlmAttachment>::new(),
            )
            .await
            .expect("preview should succeed");

        assert_eq!(agent.inner.history(), before);
        // The hidden context sits immediately before the user prompt, exactly
        // where `run_with_hidden_context` pushes it.
        let prompt_index = preview
            .iter()
            .position(|message| {
                message.role() == Role::User && message.content().contains("second turn")
            })
            .expect("preview contains the user prompt");
        let hidden = &preview[prompt_index - 1];
        assert_eq!(hidden.role(), Role::System);
        assert!(hidden.content().contains("hidden facts"));
        // And it did not leak into the live conversation.
        assert!(
            !agent
                .inner
                .history()
                .iter()
                .any(|message| message.content().contains("hidden facts"))
        );
    }

    #[tokio::test]
    async fn preview_with_hidden_context_matches_run_position_on_fresh_agent() {
        let (mut preview_agent, _temp) = test_agent(codex_llm(), "chatgpt", "gpt-5").await;
        let (mut run_agent, _temp2) = test_agent(codex_llm(), "chatgpt", "gpt-5").await;

        let preview = preview_agent
            .preview_request_messages_with_hidden_context(
                "hi",
                Some("hidden facts"),
                Vec::<LlmAttachment>::new(),
            )
            .await
            .expect("preview should succeed");

        // The same sequence `run_with_hidden_context` would send: history,
        // hidden system message, then the user prompt, with generated
        // reminders appended at the tail.
        let conversation: Vec<_> = preview
            .iter()
            .filter(|message| message.role() != Role::System || message.content() == "hidden facts")
            .collect();
        let hidden_index = conversation
            .iter()
            .position(|message| message.content() == "hidden facts")
            .expect("preview contains the hidden context");
        assert_eq!(conversation[hidden_index].role(), Role::System);
        assert_eq!(conversation[hidden_index + 1].role(), Role::User);

        // A fresh agent's live context must not be left half-initialized by a
        // preview that ran on a fork.
        run_agent.push_message(LlmMessage::system("hidden facts"));
        let run_preview = run_agent
            .preview_request_messages("hi", Vec::<LlmAttachment>::new())
            .await
            .expect("preview should succeed");
        assert_eq!(preview, run_preview);
    }
}
