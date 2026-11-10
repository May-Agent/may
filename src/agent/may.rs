//! May agent wrapper around aither-agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aither_agent::{Agent, AgentBuilder, AgentError, AnyClock, CompactResult, PromptCache, Run};
use aither_core::llm::{
    Attachment as LlmAttachment, Message as LlmMessage,
    model::{Parameters, ReasoningEffort},
};
use aither_sandbox::{
    AnyContainerExec, ContainerShellRuntime, Grants, Interpreter, ShellEvents, SshServer,
};
use executor_core::DefaultExecutor;

use super::agents_md;
use super::compaction_report::CompactionReporter;
use super::context_dump::ContextDump;
use super::hooks::MayHooks;
use super::imagine::{ImageGeneratorConfig, resolve_image_generator_config};
use super::job_events::{BackgroundWake, JobFact, JobReport, JobReports};
use super::permission::PermissionApprover;
use super::provider::{ModelSlot, ProviderLlm, resolve_provider_runtime};
use super::runtime_context::{
    RepoCloneEntry, RuntimeSystemContext, build_agents_md_blocks, build_browser_context,
    build_capabilities, build_commands, build_efficiency, build_environment, build_evidence,
    build_harness, build_identity, build_languages, build_permissions, build_plan_mode_context,
    build_prepared_tools, build_profile_memories, build_profile_persona, build_repo_clones_context,
    build_session, build_terminal, build_time, build_work, build_workspace_workdirs_context, tag,
};
use super::tool_environment_cache::{
    Acquired, HomeBackend, ToolEnvironmentCache, ToolEnvironmentCacheError,
};
use crate::agent::browser_tool::BrowserBroker;
use crate::agent::register_tool::RegisterBroker;
use crate::agent::task_tool::TaskRuntimeKind;
use crate::ask_user::AskUserBroker;
use crate::command_policy::{PolicySet, RefusedReads};
use crate::config::MayConfig;
use crate::harness::compact_tool::CompactSignal;
use crate::harness::disk::{AnyDiskReader, SystemDisk, home_volumes};
use crate::harness::host_load::{
    AnyHostReader, HostLoadConfig, HostLoadMonitor, SystemReader, UnwatchedHost,
};
use crate::harness::language::{LanguageAlerts, LanguageGuard, Languages};
use crate::harness::runaway::RunawayGuard;
use crate::harness::working_document::file::DocumentFile;
use crate::harness::working_document::{
    AnyBlockerResolver, AnyWorkTracker, WorkEvents, WorkingDocument,
};
use crate::ipc::{CommandRegistry, CommandRegistryBuilder, SessionShell, ShellSlot};
use crate::network_policy::CompiledNetworkPolicy;
use crate::secrets::SecretRedactor;
use crate::session::{SessionId, SessionPaths, SessionPlanState};
use crate::shell::{ShellAssembly, ShellAssemblyError, ShellToolkit};
use crate::subagent::KeepAlive;
use crate::terminal_commands::{
    ImagineCommandConfig, SharedTerminalCommandConfig, SubagentAssembly,
    register_shared_terminal_commands,
};
use crate::terminal_permission_handler::TerminalPermissionHandler;
use crate::tools::ToolEnvironments;
use crate::workspace::Workspace as MayWorkspace;
use crate::workspace_request::WorkspaceRequestBroker;

/// How many keep-alives in one run count as idling frequently.
const FREQUENT_IDLE_THRESHOLD: usize = 3;

/// May agent error.
#[derive(Debug, thiserror::Error)]
pub enum MayAgentError {
    /// Config error.
    #[error("Config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    /// Baseline preparation failed before the agent became ready.
    #[error("Baseline tool initialization failed: {0}")]
    ToolInitialization(#[from] crate::tools::ToolRuntimeError),
    /// The session's prepared tool environment could not be acquired.
    #[error(transparent)]
    ToolEnvironment(#[from] ToolEnvironmentCacheError),
    /// The session's shell could not be assembled.
    #[error(transparent)]
    Shell(#[from] ShellAssemblyError),
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
    /// xAI Grok auth error.
    #[error("xAI Grok auth error: {0}")]
    GrokAuth(#[from] crate::grok::GrokAuthError),
    /// Workspace config error.
    #[error("Workspace config error: {0}")]
    Workspace(#[from] crate::workspace::WorkspaceConfigError),
    /// Issue backend resolution error.
    #[error("Issue backend error: {0}")]
    Issue(#[from] crate::issue::IssueError),
    /// MCP error.
    #[error("MCP error: {0}")]
    Mcp(#[from] crate::mcp::McpConnectionLoadError),
    /// Terminal command registration error.
    #[error("terminal command error: {0}")]
    Commands(#[from] crate::terminal_commands::TerminalCommandsError),
    /// The session's row could not be read.
    #[error("cannot read the session's repository binding: {0}")]
    SessionRow(#[from] crate::db::ChatDbError),
    /// The session's pool of model-facing ids could not be built.
    #[error("cannot restore the session's model-facing ids: {0}")]
    SessionIds(crate::db::ChatDbError),
    /// The session's stored repository binding is malformed.
    #[error(transparent)]
    Binding(#[from] crate::session::BindingError),
}

/// Execution environment for a `MayAgent`.
///
/// Only `Local` and `Container` are valid home environments. SSH servers
/// attached to the profile are surfaced through `MayAgentConfig::ssh_servers`
/// and reached per-command via the `terminal` tool's `location: <name>`
/// parameter. There is no remote-home variant: remote
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
        container_ipc_route: aither_sandbox::HostRoute,
        /// Executor for running commands inside the container.
        container_exec: AnyContainerExec,
        /// Resolved browser CDP endpoint reachable from inside the container.
        browser_cdp_endpoint: Option<String>,
    },
}

/// Makes the reader a session's host load monitor reads in place of this
/// host. A reader is not shared between threads, so the config carries what
/// makes one.
#[derive(Clone)]
pub struct HostReaderSource(Arc<dyn Fn() -> AnyHostReader + Send + Sync>);

impl HostReaderSource {
    /// A source calling `make` for each agent built.
    pub fn new(make: impl Fn() -> AnyHostReader + Send + Sync + 'static) -> Self {
        Self(Arc::new(make))
    }

    fn reader(&self) -> AnyHostReader {
        (self.0)()
    }
}

impl std::fmt::Debug for HostReaderSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostReaderSource")
    }
}

/// Configuration for creating a `MayAgent`.
#[derive(Debug)]
pub struct MayAgentConfig {
    /// The session's subagent tracker; `None` starts a fresh one.
    pub subagent_tracker: Option<crate::agent::subagent_tracker::SubagentTracker>,
    /// The daemon's connectivity source: the agent abandons a response on
    /// sleep or network loss, and its shell records the outages commands ran
    /// through.
    pub connectivity: aither_connectivity::Connectivity,
    /// Trusted baseline tool distribution used by every configured backend.
    pub tool_distribution: Arc<crate::tools::ToolDistribution>,
    /// Prepared tool environments kept across rebuilds of a session's agent.
    pub tool_environments: ToolEnvironmentCache,
    /// The daemon's machine-wide jobserver pool; this agent's shell and its
    /// subagents' draw their jobs from it.
    pub job_server: crate::jobserver::JobServer,
    /// Scheduling priority applied to commands this agent runs.
    pub command_priority: crate::config::CommandPriority,
    /// The LLM provider.
    pub llm: ProviderLlm,
    /// The concrete model identifier selected for this request.
    pub model_id: String,
    /// Provider ID selected for this request.
    pub provider_id: String,
    /// Tier-resolved models for the `ask` terminal command.
    pub ask_models: super::provider::AskTierModels,
    /// Model compaction runs on; `None` compacts on the session model.
    pub compaction_llm: Option<ModelSlot<ProviderLlm>>,
    /// How long the session provider keeps the conversation cached; idle
    /// compaction and reassembly follow it.
    pub cache_lifetime: crate::cache_lifetime::CacheLifetime,
    /// When the conversation is compacted.
    pub context_policy: aither_agent::ContextPolicy,
    /// Time source for idle timing; `None` reads the system clock. Tests set it.
    pub clock: Option<aither_agent::ManualClock>,
    /// Session sandbox directory (the shell's working directory).
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
    /// The session's communication and working languages.
    pub languages: Languages,
    /// Channel for workspace access requests.
    pub workspace_request_broker: WorkspaceRequestBroker,
    /// Channel for ask user requests.
    pub ask_user_broker: AskUserBroker,
    /// Image generation provider and model.
    pub image_generator: Option<ImageGeneratorConfig>,
    /// Preconfigured SSH servers a `terminal` command names as its `location`.
    pub ssh_servers: Vec<crate::config::SshServerConfig>,
    /// Optional web UI permission approver.
    pub permission_approver: Option<PermissionApprover>,
    /// The daemon's live configuration, read when the agent publishes: the
    /// `[issue_approval]` settings apply as they are saved.
    pub live_config: Option<Arc<arc_swap::ArcSwap<MayConfig>>>,
    /// Discovery and messaging between main agents, when the daemon hosts
    /// the session.
    pub agent_messaging: Option<crate::agent_link::AgentMessaging>,
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
    /// Optional broker for `tunnel` command requests.
    pub tunnel_broker: Option<crate::agent::tunnel_tool::TunnelBroker>,
    /// Optional broker for `browser` command requests.
    pub browser_broker: Option<BrowserBroker>,
    /// Connected ACP agents registered as subagent types, when the daemon
    /// hosts the session.
    pub acp: Option<Arc<super::acp_subagent::AcpHosts>>,
    /// Daemon handles for the `computer` command — a session virtual
    /// desktop exists only on macOS local environments.
    #[cfg(target_os = "macos")]
    pub computer: Option<crate::agent::computer_tool::ComputerHandles>,
    /// Optional broker for `register-skills` command requests.
    pub register_skills_broker: Option<RegisterBroker>,
    /// The user's timezone, stated in the system prompt.
    pub timezone: crate::config::TimeZoneName,
    /// When the agent is told the host is under memory pressure or high
    /// load.
    pub host_load: HostLoadConfig,
    /// Where the load monitor reads the host; `None` reads this host. Tests
    /// set it.
    pub host_reader: Option<HostReaderSource>,
    /// Model context window resolved from provider metadata.
    pub model_context_window: Option<u32>,
    /// Request-level parameters applied to each LLM call for this agent.
    pub request_parameters: Parameters,
    /// Prompt-cache epoch of the agent this one replaces; 0 for a first build.
    pub prompt_cache_epoch: u32,
    /// Database handle for runtime memory/state lookups.
    pub db: crate::db::ChatDb,
    /// Keychain secrets snapshot: injected into the session env file and
    /// listed by name in the terminal runtime context.
    pub secrets: crate::secrets::Secrets,
    /// The rules this session imports, for a session `/import-claude` or
    /// `/import-codex` opened.
    pub rules_import: Option<crate::rules_import::RulesImport>,
    /// The daemon's live limits, shared by every agent it builds.
    pub limits: super::LiveLimits,
}

/// Session-scoped inputs for [`MayAgent::from_config_with_model_and_permissions`].
#[derive(Debug)]
pub struct MayAgentSessionInput<'a> {
    /// Session sandbox directory (the shell's working directory).
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
    /// Prompt-cache epoch of the agent this one replaces; 0 for a first build.
    pub prompt_cache_epoch: u32,
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    /// Optional permission approver handling interactive approval.
    pub permission_approver: Option<PermissionApprover>,
    /// The daemon's live configuration, when the session has one.
    pub live_config: Option<Arc<arc_swap::ArcSwap<MayConfig>>>,
    /// Discovery and messaging between main agents, when the daemon hosts
    /// the session.
    pub agent_messaging: Option<crate::agent_link::AgentMessaging>,
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
    /// Channel for tunnel registrations.
    pub tunnel_broker: Option<crate::agent::tunnel_tool::TunnelBroker>,
    /// The daemon's ACP hosts; connected ACP agents run on them, inside the
    /// session's sandbox.
    pub acp_host: Option<Arc<super::acp_subagent::AcpHosts>>,
    /// Channel for browser requests.
    pub browser_broker: Option<BrowserBroker>,
    /// Daemon handles for `computer` commands (macOS local only).
    #[cfg(target_os = "macos")]
    pub computer: Option<crate::agent::computer_tool::ComputerHandles>,
    /// Channel for register-skills requests.
    pub register_skills_broker: Option<RegisterBroker>,
    /// Execution environment (local heel sandbox or container).
    pub environment: AgentEnvironment,
    /// Trusted baseline tool distribution used by every configured backend.
    pub tool_distribution: Arc<crate::tools::ToolDistribution>,
    /// Prepared tool environments kept across agent rebuilds.
    pub tool_environments: ToolEnvironmentCache,
    /// The daemon's machine-wide jobserver pool.
    pub job_server: crate::jobserver::JobServer,
    /// Which provider candidate this build should try (retry counter).
    pub provider_attempt: usize,
    /// The session's subagent tracker; `None` starts a fresh one.
    pub subagent_tracker: Option<crate::agent::subagent_tracker::SubagentTracker>,
    /// The daemon's connectivity source.
    pub connectivity: aither_connectivity::Connectivity,
    /// The rules this session imports, for a session `/import-claude` or
    /// `/import-codex` opened.
    pub rules_import: Option<crate::rules_import::RulesImport>,
    /// The daemon's live limits, shared by every agent it builds.
    pub limits: super::LiveLimits,
    /// The model's context window as the daemon resolved it (provider
    /// report, then the model table); the agent compacts against it.
    pub model_context_window: Option<u32>,
}

type MayAgentInner = Agent<ProviderLlm, MayHooks, ProviderLlm>;

/// The May agent.
pub struct MayAgent {
    inner: MayAgentInner,
    /// The session's shell: its jobs, events and grants.
    shell: Arc<SessionShell>,
    /// The facts the shell raises about its jobs, which the idle agent waits
    /// on.
    job_events: ShellEvents,
    /// Where a fact of a main-agent job is told to the clients that show it.
    job_facts_tx: async_channel::Sender<JobFact>,
    /// The other end of [`Self::job_facts_tx`].
    job_facts: async_channel::Receiver<JobFact>,
    /// Set when the user stopped the run: facts wait for the user's next
    /// input instead of waking the session.
    interrupted: bool,
    /// The live grants of the session's sandbox: an approved workspace
    /// request reaches the shell through them.
    grants: Grants,
    session_id: SessionId,
    sandbox_dir: PathBuf,
    provider_id: String,
    model_id: String,
    model_context_window: Option<u32>,
    /// The thresholds the agent compacts by; `/context` reports them.
    context_policy: aither_agent::ContextPolicy,
    /// The commands registered by name with their descriptions; the command
    /// table lists them and every tool of the registry's command sets.
    command_descriptions: Vec<(String, String)>,
    /// The registry whose command sets (MCP servers) the table reads live.
    commands: CommandRegistry,
    /// The commands registered by name; `/context` counts every other row of
    /// the command table as an MCP tool.
    named_commands: Vec<String>,
    /// The native tool definitions the agent sends with every request;
    /// `/context` sizes them.
    native_tools: Vec<aither_core::llm::tool::ToolDefinition>,
    runtime_system_context: RuntimeSystemContext,
    subagent_tracker: crate::agent::subagent_tracker::SubagentTracker,
    /// Model clone reserved for `/btw` side questions.
    btw_llm: ModelSlot<ProviderLlm>,
    language_guard: LanguageGuard,
    /// Set when the `compact` tool ended a turn for a compaction.
    compaction_requested: Arc<AtomicBool>,
    /// This agent's entry among the daemon's btw fork hosts, while it is the
    /// session's agent.
    _fork_registration: Option<crate::agent_link::ForkRegistration>,
}

/// The prompt cache of `session_id`'s agent resumed at `epoch`: the one
/// identity both the agent's requests and its side requests take their key
/// from.
fn session_prompt_cache(session_id: &str, epoch: u32) -> PromptCache {
    PromptCache::new(session_id).with_epoch(epoch)
}

/// `model` for a side request of `session_id`'s agent at prompt-cache
/// `epoch`: its requests carry the key the agent's own take, so the provider
/// places both on the same cache shard.
pub fn side_slot(
    model: &ModelSlot<ProviderLlm>,
    session_id: &str,
    epoch: u32,
) -> ModelSlot<ProviderLlm> {
    let mut slot = model.clone();
    slot.parameters = slot
        .parameters
        .prompt_cache_key(session_prompt_cache(session_id, epoch).key());
    slot
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

/// The session's prepared tools: the cached ones when the session's home
/// backend is unchanged, otherwise prepared now.
async fn session_tools(
    config: &MayAgentConfig,
    backend: HomeBackend,
) -> Result<ToolEnvironments, MayAgentError> {
    let started = std::time::Instant::now();
    let tools_dir = crate::session::MayHomePaths::new(config.may_dir.clone()).tools_dir();
    let (tools, outcome) = match config
        .tool_environments
        .acquire(&config.session_id, backend)
        .await?
    {
        Acquired::Ready(tools) => (*tools, "adopted"),
        Acquired::Prepare(lease) => {
            let tools = match &config.environment {
                AgentEnvironment::Local => {
                    ToolEnvironments::prepare_host(&config.tool_distribution, &tools_dir).await
                }
                AgentEnvironment::Container {
                    container_id,
                    container_exec,
                    ..
                } => {
                    ToolEnvironments::prepare_container(
                        container_exec.clone(),
                        container_id,
                        &config.tool_distribution,
                        &tools_dir,
                    )
                    .await
                }
            }
            .inspect_err(|error| {
                tracing::error!(
                    stage = "tool_preparation",
                    %error,
                    "tool environment preparation failed"
                );
            })?;
            lease.complete(tools.clone());
            (tools, "prepared")
        }
    };
    tracing::debug!(
        stage = "tool_preparation",
        session_id = %config.session_id,
        outcome,
        elapsed_ms = started.elapsed().as_millis(),
        "session tool environments ready"
    );
    Ok(tools)
}

impl MayAgent {
    /// Create a new `MayAgent`.
    ///
    /// # Errors
    ///
    /// Returns [`MayAgentError`] when the session's environment cannot be
    /// prepared — opening the network audit log, preparing the tools,
    /// building the shell, or registering the skills/subagents directories
    /// fails.
    pub async fn new(config: MayAgentConfig) -> Result<Self, MayAgentError> {
        let is_container = matches!(config.environment, AgentEnvironment::Container { .. });

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

        let skills_dir = if let Some(profile_id) = config.profile_id.as_deref() {
            crate::session::MayHomePaths::new(config.may_dir.clone())
                .profile_home(profile_id)
                .skills_dir()
        } else {
            crate::session::MayHomePaths::new(config.may_dir.clone()).skills_dir()
        };

        // Mount workspace grants into the sandbox: read-only dirs become
        // readable mounts, read-write dirs writable mounts. The launch
        // directory is readable by default — `readable_dirs` carries it — and
        // the sandbox dir is the shell's own working directory, so it needs
        // no grant entry of its own.
        let grants = Grants::new();
        for path in workspace
            .readable_dirs()
            .into_iter()
            .filter(|path| path.as_path() != config.sandbox_dir.as_path())
        {
            grants.add_readable(path);
        }
        // `~/.may/memories` is linked into the session's `~/.may`; it must be
        // writable through the symlink, so it joins the grants.
        let memories_dir = crate::session::MayHomePaths::new(config.may_dir.clone()).memories_dir();
        async_fs::create_dir_all(&memories_dir).await.ok();
        for path in workspace
            .writable_dirs()
            .into_iter()
            .filter(|path| path.as_path() != config.sandbox_dir.as_path())
        {
            grants.add_writable(path);
        }
        grants.add_writable(&memories_dir);
        // The skills the agent loads are read in place.
        grants.add_readable(&skills_dir);

        // Job ids, stored outputs and subagent ids lease from the session's
        // pool, as its questions, tunnels and tasks do.
        let session_ids = config
            .db
            .session_ids(config.session_id.as_str())
            .await
            .map_err(MayAgentError::SessionIds)?;

        let tools = session_tools(
            &config,
            match &config.environment {
                AgentEnvironment::Local => HomeBackend::Local,
                AgentEnvironment::Container {
                    container_id,
                    container_ipc_route,
                    ..
                } => HomeBackend::container(container_id, container_ipc_route),
            },
        )
        .await?;
        let catalog = tools.catalog().clone();

        let permissions = TerminalPermissionHandler::for_agent(
            config.permission_approver.clone(),
            config.network_policy.clone(),
            config.profile_id.clone(),
        )
        .with_ssh_authorizer(super::ssh_authorizer::MaySshAuthorizer::new(
            config.session_id.as_str().to_string(),
            config.ask_user_broker.clone(),
            config.db.clone(),
        ));
        // Local and container commands draw from the pool: a container
        // command's fifo is made in the container's kernel and brokered by
        // aither's relay over its exec, so it works on VM-backed runtimes
        // too. A command located on an SSH server runs on another machine,
        // outside the pool, and takes no local token; aither enforces that,
        // so a session with SSH servers is built with the pool like any other.
        let pool = config
            .job_server
            .connect()
            .map_err(|e| MayAgentError::Sandbox(e.to_string()))?;
        let ssh_servers: Vec<SshServer> = config
            .ssh_servers
            .iter()
            .map(|s| SshServer {
                name: s.name.clone(),
                target: s.target.clone(),
            })
            .collect();
        let refused_reads = RefusedReads::default();
        let budgets = config.limits.shell();
        let assembly = ShellAssembly {
            tools,
            container: match &config.environment {
                AgentEnvironment::Local => None,
                AgentEnvironment::Container {
                    container_id,
                    container_ipc_route,
                    container_exec,
                    ..
                } => Some(
                    ContainerShellRuntime::new(
                        container_id.clone(),
                        container_ipc_route.clone(),
                        container_exec.clone(),
                    )
                    .with_workspace_mount(
                        crate::container::lifecycle::workspace_mount(&config.sandbox_dir),
                    ),
                ),
            },
            working_dir: config.sandbox_dir.clone(),
            grants: grants.clone(),
            priority: config.command_priority.process_priority(),
            audit_log: network_audit,
            permissions: permissions.clone(),
            // Keychain secrets (`/secret`) land as env vars in every spawned
            // command; the store is re-read per call, so additions take
            // effect without rebuilding the agent. `MAY_TASKS` names the
            // session's `tasks.md` as the commands see it: a container mounts
            // the sandbox directory as its workspace.
            env_source: Some(super::command_env::CommandEnv::new(
                crate::secrets::SessionEnv::new(
                    crate::secrets::SecretStore::new(&config.may_dir),
                    config.session_id.as_str(),
                ),
                if is_container {
                    crate::container::lifecycle::container_workspace_path(crate::tasks_md::FILE)
                } else {
                    session_paths.tasks_path().display().to_string()
                },
            )),
            budgets: budgets.clone(),
            interpreters: vec![Interpreter::Shell, Interpreter::Python],
            policies: PolicySet {
                managed_bash: crate::command_policy::runs_managed_bash(is_container),
                refused: refused_reads.clone(),
            },
            slot: ShellSlot::new(),
            ssh_servers,
            ids: Some(session_ids.allocator().clone()),
            // A subagent launched from inside a session clone works in a
            // child clone of it.
            spaces: Some(super::subagent_spaces::SessionSubagentSpaces::new(
                crate::repo_clone::SessionRepos::new(
                    &config.sandbox_dir,
                    config.session_id.as_str(),
                    config.db.clone(),
                )
                .with_host(crate::host_git::HostGit::inherit()),
            )),
            pool: Some(pool),
            connectivity: Some(config.connectivity.clone()),
        };
        // Connected ACP agents start in the sandbox this session's own
        // commands run in; a container session has no host sandbox to give
        // them, so it offers none.
        let sandbox = assembly.sandbox().await?;
        let acp = match config.acp.as_ref() {
            Some(hosts) if !is_container => Some(
                hosts
                    .agents(
                        config.session_id.as_str(),
                        super::acp_sandbox::SessionSandbox::for_session(
                            sandbox.clone(),
                            config.permission_approver.clone(),
                            config
                                .job_server
                                .broker()
                                .map_err(|e| MayAgentError::Sandbox(e.to_string()))?,
                        ),
                    )
                    .map_err(|e| MayAgentError::Sandbox(e.to_string()))?,
            ),
            Some(_) => {
                tracing::warn!(
                    "connected ACP agents are not offered in container sessions: they would run on the host"
                );
                None
            }
            None => None,
        };

        // Read global memory from the memories directory sessions edit via
        // the `~/.may/memories` link/mount.
        let memory_file = crate::session::MayHomePaths::new(config.may_dir.clone()).memory_file();
        let memory_content = async_fs::read_to_string(&memory_file)
            .await
            .unwrap_or_else(|error| {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(error = %error, "failed to load global memory file");
                }
                String::new()
            });
        let harness = build_harness();
        let evidence = build_evidence();
        let environment_block = build_environment(
            &config.model_id,
            &config.environment,
            aither_models::lookup(config.model_id.as_str()),
            config.ssh_servers.clone(),
        );
        let terminal = build_terminal(&config.environment);
        let permissions_block = build_permissions(
            &config.environment,
            config.secrets.names(),
            config.network_policy.as_ref(),
        );
        let time = build_time(
            &config.request_parameters,
            &config.timezone,
            config.location.as_ref(),
        );
        // What every subagent's context starts with besides its definition's
        // prompt: the harness, the evidence rule and the environment it works
        // in.
        let subagent_instructions = vec![
            harness.clone(),
            evidence.clone(),
            environment_block.clone(),
            terminal.clone(),
            permissions_block.clone(),
            time.clone(),
        ];
        let mut runtime_system_context = RuntimeSystemContext::new(memory_file, memory_content)
            .with_instruction(tag::IDENTITY, build_identity())
            .with_instruction(tag::ENVIRONMENT, environment_block)
            .with_instruction(tag::TERMINAL, terminal)
            .with_instruction(tag::PREPARED_TOOLS, build_prepared_tools(&catalog)?)
            .with_instruction(tag::PERMISSIONS, permissions_block)
            .with_instruction(tag::HARNESS, harness)
            .with_instruction(tag::EVIDENCE, evidence)
            .with_instruction(tag::TIME, time);
        if let Some(languages) = build_languages(&config.languages) {
            runtime_system_context =
                runtime_system_context.with_instruction(tag::LANGUAGES, languages);
        }

        // Session repo clones: the writable working copy of each repository
        // the session touches lives under the sandbox; checkouts stay read
        // mounts. The launch directory's repo is cloned eagerly.
        let session_repos = crate::repo_clone::SessionRepos::new(
            session_paths.root(),
            config.session_id.as_str(),
            config.db.clone(),
        );
        let mut repo_clones = Vec::new();
        let staging = crate::session::MayHomePaths::new(config.may_dir.clone()).repo_staging_dir();
        // The session's repository binding names the clone; a GitHub binding
        // needs no checkout at all. Without one, the launch directory's
        // repository decides.
        let binding = config
            .db
            .get_session(config.session_id.as_str())
            .await?
            .and_then(|row| row.repo)
            .map(|stored| crate::session::RepoBinding::parse(&stored))
            .transpose()?;
        let worktree = crate::repo_search::worktree_root(&config.cwd);
        // The first-contact trust decision picks the clone: full and
        // prs-only clone the GitHub remote, local-branch clones the checkout
        // locally; `local` writes the checkout; undecided binds nothing — the
        // session gate asks before the first turn.
        let made = match (&binding, &worktree) {
            (Some(binding), _) => {
                session_repos
                    .ensure_clone_of_binding(binding, &workspace.config, &staging)
                    .await
            }
            (None, Some(worktree)) => {
                session_repos
                    .ensure_clone_of(worktree, &workspace.config, &staging)
                    .await
            }
            (None, None) => Ok(None),
        };
        match made {
            Ok(Some(clone_path)) => repo_clones.push(RepoCloneEntry {
                clone_path,
                source: match (&binding, &worktree) {
                    (_, Some(worktree)) => crate::fs_utils::display_path(worktree),
                    (Some(binding), None) => binding.clone_key(),
                    (None, None) => unreachable!("a clone needs a binding or a checkout"),
                },
            }),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "failed to create session repo clone");
            }
        }
        if let Some(repo_clones) =
            build_repo_clones_context(&repo_clones, &config.environment, &session_paths)
        {
            runtime_system_context =
                runtime_system_context.with_instruction(tag::REPO_CLONES, repo_clones);
        }

        // Rules: the global `~/.may/AGENTS.md` and the launch directory's
        // AGENTS.md are loaded into the stable prefix and nested AGENTS.md
        // files are listed, so the agent never has to find them itself.
        let global_agents_path =
            crate::session::MayHomePaths::new(config.may_dir.clone()).global_agents_md();
        let (global_agents, agents_index) = futures_lite::future::zip(
            agents_md::load_global(&global_agents_path),
            agents_md::discover(&config.cwd),
        )
        .await;
        let agents_label = (!is_container).then(|| config.cwd.display().to_string());
        runtime_system_context = runtime_system_context
            .with_global_agents_source(global_agents_path)
            .with_agents_source(config.cwd.clone(), agents_label.as_deref());
        let agents_blocks = build_agents_md_blocks(
            global_agents.as_ref(),
            &agents_index,
            runtime_system_context.agents_root_source(),
            &[],
        );
        runtime_system_context = runtime_system_context.with_agents_md(agents_blocks);

        let llm = config.llm.clone();

        // Clone LLM for terminal IPC commands before moving it into the builder.
        let terminal_command_llm = llm.clone();
        // `/btw` side questions run through this clone — separate from the
        // agent's own model so a side query never joins the agent's history.
        let btw_llm = ModelSlot::new(llm.clone(), config.request_parameters.clone());
        // Ask-tier models arrive paired with the parameters of their own
        // provider instances.
        let ask_flagship = config.ask_models.flagship.clone();
        let ask_balanced = config.ask_models.balanced.clone();
        let ask_fast = config.ask_models.fast.clone();

        let issue_store = crate::issue::resolve_bound_issue_store(
            binding.as_ref(),
            &config.cwd,
            &session_paths.workspace_config(),
            config.db.clone(),
        )?;
        let publish_gate = config
            .live_config
            .clone()
            .zip(config.permission_approver.clone())
            .map(|(live, approver)| super::publish_approval::PublishGate::new(live, approver));
        let imagine_config = ImagineCommandConfig {
            generator: config.image_generator.clone(),
            artifacts_dir: session_paths.artifacts_dir(),
        };
        let agent_messaging = config.agent_messaging.clone();
        let hook_runtime = config
            .permission_approver
            .as_ref()
            .and_then(|approver| approver.runtime_service.clone());

        let language_guard = LanguageGuard::new(config.languages);
        let subagent_tracker = config.subagent_tracker.clone().unwrap_or_default();
        // The session's own inbox when the daemon keeps one, so answers
        // committed between agent builds still reach `tasks.md`.
        let task_events = hook_runtime
            .as_ref()
            .map_or_else(WorkEvents::new, |runtime| {
                runtime.task_events(config.session_id.as_str())
            });
        let toolkit = ShellToolkit::of(&budgets);
        let cache_lifetime: aither_agent::context::CacheLifetime = config.cache_lifetime.into();
        let keep_alive = matches!(
            config.cache_lifetime,
            crate::cache_lifetime::CacheLifetime::Limited(_)
        )
        .then_some(KeepAlive {
            frequent_idle_threshold: FREQUENT_IDLE_THRESHOLD,
        });

        let mut commands = CommandRegistryBuilder::new();
        let shared_terminal_commands = Box::pin(register_shared_terminal_commands(
            &mut commands,
            SharedTerminalCommandConfig {
                llm: terminal_command_llm,
                subagent_tracker: subagent_tracker.clone(),
                ask_flagship,
                ask_balanced,
                ask_fast,
                session_id: config.session_id.as_str().to_string(),
                may_dir: config.may_dir.clone(),
                sandbox_dir: config.sandbox_dir.clone(),
                cwd: config.cwd.clone(),
                db: config.db.clone(),
                ask_user: hook_runtime.clone(),
                imagine: Some(imagine_config),
                task_runtime: match &config.environment {
                    AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                    AgentEnvironment::Local => TaskRuntimeKind::Local,
                },
                task_broker: config.task_broker.clone(),
                tunnel_broker: config.tunnel_broker.clone(),
                browser_broker: config.browser_broker.clone(),
                register_skills_broker: config.register_skills_broker.clone(),
                request_parameters: config.request_parameters.clone(),
                workspace_request_broker: (!is_container)
                    .then_some(config.workspace_request_broker.clone()),
                subagents: SubagentAssembly {
                    toolkit: toolkit.clone(),
                    cache_lifetime,
                    keep_alive,
                    instructions: subagent_instructions,
                },
                issue_store: issue_store.clone(),
                task_events: task_events.clone(),
                publish_gate,
                workspace_config: workspace.config.clone(),
                agent_messaging,
                languages: Some(language_guard.clone()),
                acp,
                git: crate::host_git::HostGit::inherit(),
                timeout_cap: budgets.limits().timeout_cap,
                tasks_line_limit: config.limits.tasks_lines(),
                #[cfg(target_os = "macos")]
                computer: config.computer.clone(),
                rules_import: config.rules_import.clone(),
            },
        ))
        .await?;
        let registry = commands.build();
        let shell = assembly.build_on(sandbox, registry.clone()).await?;
        // A side request forks the agent before it has run any bound command.
        shared_terminal_commands.fork_launcher.set(&shell);

        // The command table, the capabilities and the efficiency rules are
        // rendered from the commands registered above.
        let command_descriptions = shared_terminal_commands.descriptions.clone();
        let capabilities = build_capabilities(
            &command_descriptions,
            &shared_terminal_commands.usages,
            &config.request_parameters,
            &skills_dir,
        )
        .await?;
        runtime_system_context = runtime_system_context
            .with_instruction(tag::EFFICIENCY, build_efficiency(&command_descriptions))
            .with_instruction(tag::CAPABILITIES, capabilities)
            .with_instruction(
                tag::COMMANDS,
                build_commands(&command_descriptions, registry.command_sets()),
            )
            .with_instruction(
                tag::WORK,
                build_work(&crate::issue::IssueStore::info(&issue_store)),
            )
            .with_instruction(
                tag::SESSION,
                build_session(
                    &config.environment,
                    &session_paths,
                    (!is_container).then(|| config.cwd.display().to_string()),
                    &crate::issue::IssueStore::info(&issue_store),
                ),
            );

        // Another agent's `btw` forks this agent from the requests it
        // publishes, whether it is working or idle.
        let fork_registration = config.agent_messaging.as_ref().map(|messaging| {
            messaging.forks.register(
                config.session_id.as_str(),
                shared_terminal_commands.fork_source.clone(),
                btw_llm.clone(),
                shared_terminal_commands.fork_launcher.clone(),
                toolkit.clone(),
                config.languages,
            )
        });

        let (runtime_system_context, browser_blocks) =
            apply_runtime_context(runtime_system_context, &config, &workspace);
        let runtime_system_context =
            runtime_system_context.with_instruction(tag::BROWSER, browser_blocks);

        let valve = Arc::new(CompactSignal::default());
        let compaction_requested = Arc::new(AtomicBool::new(false));
        let hooks = MayHooks::new(
            hook_runtime.clone(),
            &config.session_id,
            SecretRedactor::for_session(&config.may_dir, config.session_id.as_str()),
            refused_reads,
            shell.jobs(),
            valve.clone(),
            compaction_requested.clone(),
        );
        let effective_max_iterations = if config.max_iterations == 0 {
            usize::MAX
        } else {
            config.max_iterations
        };
        let clock = config
            .clock
            .clone()
            .map_or_else(AnyClock::default, AnyClock::new);
        let reader = config.host_reader.as_ref().map_or_else(
            || {
                SystemReader::new().map_or_else(
                    |error| {
                        tracing::warn!(
                            %error,
                            "this host's load cannot be read; it is not watched"
                        );
                        AnyHostReader::new(UnwatchedHost)
                    },
                    AnyHostReader::new,
                )
            },
            HostReaderSource::reader,
        );
        let monitor = HostLoadMonitor::new(
            reader,
            config.host_load.clone(),
            budgets.limits().foreground_wait,
        )
        .with_disk(
            config.limits.disk(),
            home_volumes(
                AnyDiskReader::new(SystemDisk::new(&config.sandbox_dir)),
                shell.container_probe(),
            ),
        );
        let (conditions, events) = crate::harness::host_load::spawn(
            &DefaultExecutor,
            monitor,
            Some(RunawayGuard::new(shell.job_memory())),
            clock,
        )
        .into_sources();

        let (job_facts_tx, job_facts) = async_channel::unbounded();
        let mut builder = AgentBuilder::new(llm)
            .hooks(hooks)
            .system_layout(tag::layout())
            .request_parameters(config.request_parameters.clone())
            .prompt_cache(session_prompt_cache(
                config.session_id.as_str(),
                config.prompt_cache_epoch,
            ))
            .cache_lifetime(cache_lifetime)
            .context_policy(config.context_policy)
            .max_iterations(effective_max_iterations)
            .connectivity(config.connectivity.clone())
            .approver(permissions)
            .fork_source(shared_terminal_commands.fork_source.clone())
            .subagents(subagent_tracker.runs().clone())
            .context_source(conditions)
            .context_source(events)
            .context_source(JobReports::new(shell.jobs().events(), job_facts_tx.clone()));
        if let Some(clock) = config.clock.clone() {
            builder = builder.clock(clock);
        }
        let spec = crate::tasks_md::schema::document_spec(&config.limits.tasks_lines());
        builder = builder.context_source(LanguageAlerts::new(
            language_guard.clone(),
            Some(DocumentFile::new(shell.working_dir(), spec.file())),
        ));
        if let Some(runtime) = hook_runtime.clone() {
            builder = builder
                .context_source(super::ask_user_tool::OpenQuestionsContext::new(
                    runtime.clone(),
                    config.session_id.as_str(),
                ))
                .context_source(super::ci_alert::CiAlertContext::new(
                    runtime.db(),
                    config.session_id.as_str(),
                ));
        }
        // `tasks.md` follows what the commands above record, and reads the
        // state of the issues and PRs its rows name. Its subagents keep their
        // own under the same schema and stop rule.
        let mut document = WorkingDocument::new(&spec, shell.working_dir(), task_events)
            .with_continue_reminder(crate::tasks_md::schema::continue_reminder())
            .with_work_tracker(AnyWorkTracker::new(
                super::work_tracker::MayWorkTracker::new(
                    issue_store.clone(),
                    config.db.clone(),
                    &config.sandbox_dir,
                    config.session_id.as_str(),
                    config.cwd.clone(),
                ),
            ));
        if let Some(runtime) = hook_runtime {
            document = document.with_blocker_resolver(AnyBlockerResolver::new(
                super::ask_user_tool::AskUserBlockerResolver::new(
                    runtime,
                    config.session_id.as_str(),
                ),
            ));
        }
        builder = builder.context_source(document);
        if let Some(compaction) = config.compaction_llm.clone() {
            builder = builder.compaction_model(compaction.llm, compaction.parameters);
        }
        let builder = toolkit.register(builder, &shell, Some(valve));
        let native_tools = builder.tool_definitions();
        let mut agent = builder.build();
        runtime_system_context.reconcile(agent.context_mut());

        let named_commands = command_descriptions
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        Ok(Self {
            inner: agent,
            sandbox_dir: shell.working_dir().clone(),
            job_events: shell.jobs().events(),
            job_facts_tx,
            job_facts,
            interrupted: false,
            shell,
            grants,
            session_id: config.session_id,
            provider_id: config.provider_id,
            model_id: config.model_id,
            model_context_window: config.model_context_window,
            context_policy: config.context_policy,
            command_descriptions,
            commands: registry,
            named_commands,
            native_tools,
            runtime_system_context,
            subagent_tracker,
            btw_llm,
            language_guard,
            compaction_requested,
            _fork_registration: fork_registration,
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
        let config = Box::pin(Self::session_config(may_config, input)).await?;
        Box::pin(Self::new(config)).await
    }

    /// Resolves the [`MayAgentConfig`] of a session from the May config.
    ///
    /// # Errors
    ///
    /// Returns [`MayAgentError`] when resolving the provider runtime fails
    /// or the SSH attachments for the profile cannot be loaded.
    pub(crate) async fn session_config(
        may_config: &MayConfig,
        input: MayAgentSessionInput<'_>,
    ) -> Result<MayAgentConfig, MayAgentError> {
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
            prompt_cache_epoch,
            profile_id,
            permission_approver,
            live_config,
            agent_messaging,
            profile_persona,
            profile_memories,
            plan_state,
            location,
            task_broker,
            tunnel_broker,
            browser_broker,
            acp_host,
            #[cfg(target_os = "macos")]
            computer,
            register_skills_broker,
            environment,
            tool_distribution,
            tool_environments,
            job_server,
            provider_attempt,
            subagent_tracker,
            connectivity,
            rules_import,
            limits,
            model_context_window,
        } = input;
        let selector = super::provider::parse_model_selector(model)?;
        let redactor = crate::secrets::SecretRedactor::for_session(&may_dir, session_id.as_str());
        let resolved_provider =
            resolve_provider_runtime(may_config, &selector, provider_attempt, &redactor).await?;
        let provider_id = resolved_provider.provider_id;
        let llm = resolved_provider
            .llm
            .with_context_window(model_context_window);
        let request_parameters =
            super::provider::session_request_parameters(may_config, &selector, reasoning_effort);

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

        let ask_models = super::provider::resolve_ask_tier_models(
            may_config,
            &provider_id,
            selector.model_id(),
            &redactor,
        )
        .await?;
        let compaction_llm = super::provider::resolve_compaction_model(
            may_config,
            &provider_id,
            selector.model_id(),
            &redactor,
        )
        .await?;
        let cache_lifetime = may_config
            .get_provider(&provider_id)
            .map(|instance| instance.cache_lifetime)
            .ok_or_else(|| {
                MayAgentError::Llm(format!("provider '{provider_id}' is not configured"))
            })?;
        let secrets = crate::secrets::Secrets::load_for_session(
            &crate::secrets::SecretStore::new(&may_dir),
            may_config,
            session_id.as_str(),
        );
        Ok(MayAgentConfig {
            subagent_tracker,
            connectivity,
            tool_distribution,
            tool_environments,
            job_server,
            command_priority: may_config.sandbox.command_priority,
            llm,
            model_id: selector.model_id().to_string(),
            provider_id,
            ask_models,
            compaction_llm,
            cache_lifetime,
            context_policy: may_config.context.policy(),
            clock: None,
            sandbox_dir,
            may_dir,
            cwd,
            session_id,
            profile_id,
            max_iterations: may_config.max_iterations,
            languages: may_config.language.languages(),
            workspace_request_broker,
            ask_user_broker,
            image_generator: resolve_image_generator_config(may_config),
            ssh_servers,
            permission_approver,
            live_config,
            agent_messaging,
            environment,
            profile_persona,
            profile_memories,
            plan_state,
            network_policy: may_config
                .preferences
                .network_policy
                .compile()
                .map_err(|error| MayAgentError::Sandbox(error.to_string()))?,
            secrets,
            location,
            task_broker,
            tunnel_broker,
            browser_broker,
            acp: acp_host,
            #[cfg(target_os = "macos")]
            computer,
            register_skills_broker,
            timezone: may_config.time.user_timezone(),
            host_load: may_config.host_load.monitor_config(),
            host_reader: None,
            model_context_window,
            request_parameters,
            prompt_cache_epoch,
            db,
            rules_import,
            limits,
        })
    }

    /// Runs a query; [`Run::drive`] hands its events to the caller.
    pub fn run(
        &mut self,
        prompt: &str,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Run<impl Future<Output = Result<(), AgentError>> + '_> {
        self.interrupted = false;
        self.inner.run(prompt, attachments)
    }

    /// Runs a query after persisting per-turn hidden context as a regular
    /// system message.
    pub fn run_with_hidden_context(
        &mut self,
        prompt: &str,
        hidden_context: Option<&str>,
        attachments: impl IntoIterator<Item = LlmAttachment>,
    ) -> Run<impl Future<Output = Result<(), AgentError>> + '_> {
        if let Some(hidden_context) = hidden_context
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            self.inner.push_message(LlmMessage::system(hidden_context));
        }
        self.run(prompt, attachments)
    }

    /// Continues the conversation without a new user message, for the model
    /// to act on what a background command reported. Driven like
    /// [`Self::run`].
    pub fn resume(&mut self) -> Run<impl Future<Output = Result<(), AgentError>> + '_> {
        self.interrupted = false;
        self.inner.resume()
    }

    /// Waits, between runs, for a fact about the session's jobs that the
    /// agent has yet to read: a detached command ended, a monitor reported,
    /// a job blocked on its stdin. The facts are put into the context, and
    /// the wake says whether the model has something to act on, which the
    /// host continues with [`Self::resume`]. Cancel-safe: nothing is taken
    /// until the wait completes.
    pub async fn wait_background(&mut self) -> BackgroundWake {
        let first = self.job_events.recv().await;
        let mut report = JobReport::default();
        report.take(first, &self.job_facts_tx);
        while let Some(event) = self.job_events.try_recv() {
            report.take(event, &self.job_facts_tx);
        }
        for alert in report.alerts {
            self.inner
                .context_mut()
                .push_block_alert(&aither_agent::Block::new("system-reminder", alert));
        }
        BackgroundWake {
            pending: report.pending,
            starts_run: report.needs_model && !self.interrupted,
        }
    }

    /// The user stopped the run: what the session's jobs report from now on
    /// waits for the user's next input instead of starting a run.
    pub const fn interrupt(&mut self) {
        self.interrupted = true;
    }

    /// Where the facts of the session's main-agent jobs queue, for the
    /// clients that show them: the agent tells one when it reads the fact,
    /// whether a run or the idle wait does.
    #[must_use]
    pub fn job_facts(&self) -> async_channel::Receiver<JobFact> {
        self.job_facts.clone()
    }

    /// Whether the last run ended for the `compact` tool's request, which
    /// the caller then carries out; the flag is cleared.
    #[must_use]
    pub fn take_compaction_request(&self) -> bool {
        self.compaction_requested.swap(false, Ordering::AcqRel)
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
            &super::context_report::ToolInventory {
                named_commands: &self.named_commands,
                native_tools: &self.native_tools,
            },
            self.inner.context(),
            &snapshot.metrics,
            snapshot.phase,
            &self.context_policy,
        ))
    }

    /// Verbatim request transcript for `/context dump`.
    ///
    /// Same measurement discipline as [`Self::context_report`]: runtime
    /// blocks are reconciled first, so the transcript shows the state the
    /// next request would be assembled from.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the model profiles cannot be fetched
    /// during initialization.
    pub async fn context_dump(&mut self) -> Result<ContextDump, AgentError> {
        self.sync_runtime_context().await;
        let snapshot = self.inner.snapshot_context_window().await?;
        Ok(ContextDump::assemble(
            &self.model_id,
            &self.provider_id,
            self.session_id.as_str(),
            &snapshot,
        ))
    }

    /// The messages the next request would carry: the system prefix, the
    /// compaction handoff, the kept conversation and the volatile tail.
    ///
    /// A pure read for a side request. Unlike [`Self::context_dump`] it
    /// reconciles no runtime block, so the system prefix is exactly the one
    /// the last request sent and a provider's prefix cache still hits.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the model profiles cannot be fetched
    /// during initialization.
    pub async fn context_messages(&mut self) -> Result<Vec<LlmMessage>, AgentError> {
        Ok(self.inner.snapshot_context_window().await?.messages)
    }

    /// Get session ID.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Shared handle to the session's runtime grants.
    #[must_use]
    pub(crate) fn grants(&self) -> Grants {
        self.grants.clone()
    }

    /// Get session sandbox directory.
    #[must_use]
    pub fn sandbox_dir(&self) -> PathBuf {
        self.sandbox_dir.clone()
    }

    /// The session's shell: its jobs and events.
    #[must_use]
    pub const fn shell(&self) -> &Arc<SessionShell> {
        &self.shell
    }

    /// Background jobs of this session's shell. Cloneable handle — the TUI
    /// polls it for `/jobs` and kills by pid.
    #[must_use]
    pub fn jobs(&self) -> aither_sandbox::Jobs {
        self.shell.jobs()
    }

    /// Registry of in-flight `subagent` command runs.
    /// Cloneable handle — the TUI lists and cancels individual runs.
    #[must_use]
    pub fn subagent_runs(&self) -> aither_agent::SubagentRuns {
        self.subagent_tracker.runs().clone()
    }

    /// The model for side requests (`/btw`, prompt suggestions, recaps):
    /// the agent's provider, running outside the agent so answers never
    /// enter its history, under the prompt-cache key the agent's next
    /// request carries so the provider places both on the same cache shard.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the model profiles cannot be fetched
    /// during initialization.
    pub async fn side_model(&mut self) -> Result<ModelSlot<ProviderLlm>, AgentError> {
        let epoch = self
            .inner
            .snapshot_context_window()
            .await?
            .metrics
            .prompt_cache_epoch;
        Ok(side_slot(&self.btw_llm, self.session_id.as_str(), epoch))
    }

    /// The language guard the agent was built with; its commands and the
    /// session's aux requests share its languages.
    #[must_use]
    pub fn language_guard(&self) -> LanguageGuard {
        self.language_guard.clone()
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

    /// The agent's model as the request log names it: `provider::model`.
    #[must_use]
    pub fn model_selector(&self) -> crate::ModelSelector {
        crate::ModelSelector::new(&self.provider_id, &self.model_id)
    }

    /// Model context window resolved for this agent.
    #[must_use]
    pub const fn model_context_window(&self) -> Option<u32> {
        self.model_context_window
    }

    /// The agent's prompt-cache entry; clones keep observing it after the
    /// agent is gone.
    #[must_use]
    pub fn cache_warmth(&self) -> aither_agent::CacheWarmth {
        self.inner.cache_warmth()
    }

    /// The agent's model, which counts streamed text in tokens
    /// ([`LanguageModel::count_text`]) while a run holds the agent.
    #[must_use]
    pub fn counting_model(&self) -> ProviderLlm {
        self.btw_llm.llm.clone()
    }

    /// Stop every shell job that is still running for this agent.
    pub async fn kill_running_terminal_jobs(&self) -> usize {
        self.shell.jobs().kill_all().await
    }

    pub(crate) async fn export_checkpoint(
        &mut self,
    ) -> Result<aither_agent::AgentCheckpoint, AgentError> {
        self.inner.export_checkpoint().await
    }

    pub(crate) fn push_message(&mut self, message: aither_core::llm::Message) {
        self.inner.push_message(message);
    }

    /// How long from now the idle compaction is due; see
    /// [`Agent::idle_compaction_due_in`].
    pub(crate) fn idle_compaction_due_in(&self) -> Option<std::time::Duration> {
        self.inner.idle_compaction_due_in()
    }

    /// How long from now the idle agent's cache entry is to be refreshed, or
    /// `None` when no refresh is due: the entry is cold or permanent, or
    /// every running background job ends before it cools. See
    /// [`super::keep_warm::refresh_in`].
    pub(crate) async fn keep_warm_due_in(&self) -> Option<std::time::Duration> {
        let aither_agent::context::CacheLifetime::Limited(lifetime) = self.inner.cache_lifetime()
        else {
            return None;
        };
        let cold_in = self.inner.cache_warmth().cold_in()?;
        let jobs = self.shell.jobs().list().await;
        super::keep_warm::refresh_in(Some(cold_in), lifetime, &jobs)
    }

    /// Refreshes the idle agent's cache entry with a one-token request whose
    /// output is dropped; see [`Agent::keep_cache_warm`]. `None` when there
    /// was nothing to refresh.
    pub(crate) async fn keep_cache_warm(
        &mut self,
    ) -> Result<Option<aither_core::llm::Usage>, AgentError> {
        self.inner.keep_cache_warm().await
    }

    /// Compacts the idle session once it is due; see
    /// [`Agent::compact_when_idle`].
    #[cfg(test)]
    pub(crate) async fn compact_when_idle(&mut self) -> Result<Option<CompactResult>, AgentError> {
        self.compact_when_idle_reporting(&CompactionReporter::silent())
            .await
    }

    /// [`Self::compact_when_idle`], reporting its progress to `reporter`.
    pub(crate) async fn compact_when_idle_reporting(
        &mut self,
        reporter: &CompactionReporter,
    ) -> Result<Option<CompactResult>, AgentError> {
        reporter.drive(self.inner.compact_when_idle()).await
    }

    /// Compacts the conversation, reporting its progress to `reporter`.
    pub(crate) async fn compact_reporting(
        &mut self,
        focus: Option<&str>,
        reporter: &CompactionReporter,
    ) -> Result<Option<CompactResult>, AgentError> {
        reporter.drive(self.inner.compact(focus)).await
    }

    #[cfg(test)]
    pub(crate) async fn compact(
        &mut self,
        focus: Option<&str>,
    ) -> Result<Option<CompactResult>, AgentError> {
        self.compact_reporting(focus, &CompactionReporter::silent())
            .await
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

    /// Tells the agent the user's sleep state changed; one alert per change.
    pub(crate) fn tell_sleep_notice(&mut self, notice: crate::sleep::SleepNotice) {
        RuntimeSystemContext::push_sleep_notice(self.inner.context_mut(), notice);
    }

    pub(crate) async fn sync_runtime_context(&mut self) {
        self.runtime_system_context
            .sync_memory_content(self.inner.context_mut())
            .await;
        let handoff = handoff_markdown(self.inner.context());
        self.runtime_system_context
            .sync_agents_md(handoff.as_deref())
            .await;
        // An MCP server may have added or dropped a tool since the last turn.
        self.runtime_system_context.set_commands(build_commands(
            &self.command_descriptions,
            self.commands.command_sets(),
        ));
        self.runtime_system_context
            .reconcile(self.inner.context_mut());
    }
}

/// Returns None if title generation fails.
pub async fn generate_session_title(
    config: &MayConfig,
    user_message: &str,
    assistant_response: &str,
    redactor: &crate::secrets::SecretRedactor,
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
        redactor,
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

/// Rendered markdown of the context's active handoff, if any — the text
/// `AGENTS.md` links are extracted from.
fn handoff_markdown(context: &aither_agent::Context) -> Option<String> {
    context.handoff().map(|handoff| handoff.text().to_string())
}

fn truncate_at_char_boundary(input: &str, max_chars: usize) -> &str {
    input
        .char_indices()
        .nth(max_chars)
        .map_or(input, |(idx, _)| &input[..idx])
}

/// Adds the blocks that depend on the session's profile and environment to
/// `runtime_system_context`, and returns the browser section.
fn apply_runtime_context(
    mut runtime_system_context: RuntimeSystemContext,
    config: &MayAgentConfig,
    workspace: &MayWorkspace,
) -> (RuntimeSystemContext, String) {
    if let Some(persona) = config.profile_persona.as_deref() {
        runtime_system_context = runtime_system_context
            .with_instruction(tag::PROFILE_PERSONA, build_profile_persona(persona));
    }

    runtime_system_context = runtime_system_context.with_profile_memories(
        config
            .profile_memories
            .as_ref()
            .and_then(build_profile_memories),
    );

    if let Some(plan_mode) = build_plan_mode_context(config.plan_state) {
        runtime_system_context = runtime_system_context.with_instruction(tag::PLAN_MODE, plan_mode);
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
    match &config.environment {
        AgentEnvironment::Container {
            browser_cdp_endpoint: cdp_endpoint,
            ..
        } => {
            browser_cdp_endpoint.clone_from(cdp_endpoint);
        }
        AgentEnvironment::Local => {
            runtime_system_context =
                runtime_system_context.with_workdirs(build_workspace_workdirs_context(workspace));
        }
    }

    let browser = build_browser_context(browser_enabled, browser_cdp_endpoint.as_deref());
    (runtime_system_context, browser)
}
