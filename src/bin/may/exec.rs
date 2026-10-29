//! `may exec` subcommand implementation.
//!
//! Runs a one-shot terminal command inside a session's home
//! environment (local or container) and renders the combined
//! stdout/stderr/task-id payload back to the caller. When the profile
//! has attached SSH servers, the command may select one via
//! `--ssh-server <name>`.

use std::io::{self, Write};
use std::sync::Arc;

use aither_core::llm::{CommandOutcome, CommandResult, Tool, ToolResult};
use aither_sandbox::{
    ContainerShellRuntime, OutputFormat, ShellRuntimeAvailability, SshServer, TerminalArgs,
    TerminalExecutionMode, TerminalTool, terminal_tool_factory_channel,
};
use may_agent::session::{SessionPaths, initialize_session_persistence};
use may_agent::{
    ChatDb, MayConfig, ProfileEnvironment, ProfileInfo, SessionId, SessionStore,
    SharedTerminalCommandConfig, TaskRuntimeKind, TerminalPermissionHandler,
    register_shared_terminal_commands, resolve_agent_environment_for_session,
    resolve_provider_runtime,
};

use super::cli::ExecArgs;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to resolve May home directory")]
    MissingHomeDirectory,
    #[error("failed to initialize session persistence: {0}")]
    InitializeSession(may_agent::session::SessionError),
    #[error("session '{0}' was not found")]
    SessionNotFound(String),
    #[error("failed to load profile '{profile_id}': {source}")]
    LoadProfile {
        profile_id: String,
        source: may_agent::db::ChatDbError,
    },
    #[error("invalid profile '{profile_id}': {source}")]
    InvalidProfile {
        profile_id: String,
        source: may_agent::db::ChatDbError,
    },
    #[error("failed to resolve agent environment: {0}")]
    ResolveEnvironment(may_agent::ResolveAgentEnvironmentError),
    #[error("failed to load MCP terminal commands: {0}")]
    LoadMcpConnections(may_agent::mcp::McpConnectionLoadError),
    #[error("failed to read the current working directory: {0}")]
    CurrentDir(std::io::Error),
    #[error("failed to resolve the session issue backend: {0}")]
    ResolveIssueBackend(may_agent::issue::IssueError),
    #[error("failed to resolve provider runtime: {0}")]
    ResolveProviderRuntime(may_agent::agent::MayAgentError),
    #[error("failed to load May config: {0}")]
    LoadConfig(may_agent::config::ConfigError),
    #[error("failed to compile network policy: {0}")]
    CompileNetworkPolicy(aither_sandbox::NetworkPolicyError),
    #[error("failed to create terminal tool: {0}")]
    CreateTerminalTool(aither_sandbox::TerminalError),
    #[error("failed to initialize baseline tools: {0}")]
    InitializeBaselineTools(#[from] aither_sandbox::ToolRuntimeError),
    #[error("failed to initialize tool dispatch runtime: {0}")]
    InitializeToolDispatch(may_agent::tool_dispatch::ToolDispatchError),
    #[error("failed to configure ssh servers: {0}")]
    ConfigureSshServers(String),
    #[error("ssh server '{0}' is not attached to the session's profile")]
    SshServerNotAttached(String),
    #[error("terminal execution failed: {0}")]
    TerminalCall(aither_core::Error),
    #[error("terminal tool did not return a command result")]
    NotACommandResult,
    #[error("failed to write stdout: {0}")]
    WriteStdout(std::io::Error),
    #[error("failed to write stderr: {0}")]
    WriteStderr(std::io::Error),
}

#[derive(Debug)]
struct ExecSessionContext {
    db: ChatDb,
    may_dir: std::path::PathBuf,
    session_id: SessionId,
    sandbox_dir: std::path::PathBuf,
    profile_environment: Option<ProfileEnvironment>,
    profile_model: Option<may_agent::db::entity::profiles::Model>,
    profile_container_preferences: Option<may_agent::profile::ContainerProfilePreferences>,
}

#[derive(Debug, Clone, Copy)]
struct ExecSshAuthorizer {
    allow_runtime_install: bool,
}

impl aither_sandbox::SshSessionAuthorizer for ExecSshAuthorizer {
    fn authorize_connect(
        &self,
        _target: &str,
    ) -> impl std::future::Future<Output = Result<bool, aither_sandbox::SshAuthorizationError>> + Send
    {
        std::future::ready(Ok(true))
    }

    fn authorize_runtime_install(
        &self,
        _target: &str,
        _details: &str,
    ) -> impl std::future::Future<Output = Result<bool, aither_sandbox::SshAuthorizationError>> + Send
    {
        std::future::ready(Ok(self.allow_runtime_install))
    }
}

pub async fn run_exec(args: ExecArgs) -> Result<i32, ExecError> {
    let context = load_exec_session_context(&args.session_id).await?;
    let config_path = context.may_dir.join("config.toml");
    let may_config = MayConfig::load_from(&config_path)
        .await
        .map_err(ExecError::LoadConfig)?;

    let tool_distribution = Arc::new(may_agent::tools::embedded_distribution()?);
    let environment = resolve_agent_environment_for_session(
        context.profile_environment.clone(),
        context.profile_model.as_ref(),
        context.profile_container_preferences.clone(),
        &context.may_dir,
        &context.sandbox_dir,
        &context.session_id,
        &tool_distribution,
    )
    .await
    .map_err(ExecError::ResolveEnvironment)?;

    let model = select_exec_model(&may_config);
    let shell_mode = select_exec_mode(&args, &environment);
    let terminal_tool = build_exec_terminal_tool(
        &context,
        &environment,
        &may_config,
        &model,
        args.ssh_server_id.as_deref(),
        args.allow_runtime_install,
        tool_distribution,
    )
    .await?
    .with_foreground_limit(std::time::Duration::from_secs(args.timeout));

    let output = terminal_tool
        .call(
            TerminalArgs {
                description: "execute the requested shell command".to_string(),
                script: args.script,
                mode: shell_mode,
                ssh_server_id: args.ssh_server_id,
                expect: OutputFormat::Text,
                resolution: aither_sandbox::MediaResolution::Auto,
                max_lines: args.max_lines,
                raw: args.raw,
            },
            aither_core::llm::ToolContext::new(),
        )
        .await
        .map_err(ExecError::TerminalCall)?;

    if output.is_error() {
        return Err(ExecError::TerminalCall(anyhow::anyhow!(
            output
                .error_message()
                .unwrap_or("terminal tool returned an error")
                .to_string()
        )));
    }
    let ToolResult::Command(result) = output else {
        return Err(ExecError::NotACommandResult);
    };
    render_exec_result(&result)?;
    Ok(exit_code(&result.outcome))
}

async fn load_exec_session_context(session_id: &str) -> Result<ExecSessionContext, ExecError> {
    let home_dir = dirs::home_dir().ok_or(ExecError::MissingHomeDirectory)?;
    let may_dir = home_dir.join(".may");
    let (store, db, _default_profile_id) = initialize_session_persistence(&may_dir)
        .await
        .map_err(ExecError::InitializeSession)?;
    let session_row = db
        .get_session(session_id)
        .await
        .map_err(|error| ExecError::InitializeSession(error.into()))?
        .ok_or_else(|| ExecError::SessionNotFound(session_id.to_string()))?;
    let profile_id = session_row.profile_id.clone();
    let profile_model = match profile_id {
        Some(ref profile_id) => Some(
            db.get_profile(profile_id)
                .await
                .map_err(|source| ExecError::LoadProfile {
                    profile_id: profile_id.clone(),
                    source,
                })?
                .ok_or_else(|| ExecError::LoadProfile {
                    profile_id: profile_id.clone(),
                    source: may_agent::db::ChatDbError::InvalidProfileEnvironment(
                        "profile is missing".to_string(),
                    ),
                })?,
        ),
        None => None,
    };

    let profile_preferences = match profile_model.as_ref() {
        Some(model) => Some(db.load_profile_preferences(model).await.map_err(|source| {
            ExecError::LoadProfile {
                profile_id: profile_id
                    .clone()
                    .expect("profile id present when model exists"),
                source,
            }
        })?),
        None => None,
    };
    let profile_environment = match (profile_model.clone(), profile_preferences.as_ref()) {
        (Some(model), Some(preferences)) => {
            let attachments =
                db.list_profile_remote_servers(&model.id)
                    .await
                    .map_err(|source| ExecError::LoadProfile {
                        profile_id: profile_id
                            .clone()
                            .expect("profile id present when model exists"),
                        source,
                    })?;
            Some(
                ProfileInfo::from_db_parts(model, preferences.clone(), attachments)
                    .map_err(|source| ExecError::InvalidProfile {
                        profile_id: profile_id
                            .clone()
                            .expect("profile id present when model exists"),
                        source,
                    })?
                    .environment
                    .into_kind(),
            )
        }
        (None, None) => None,
        _ => panic!("profile preferences must exist exactly when profile model exists"),
    };

    ensure_exec_session_layout(
        &store,
        profile_id.as_deref(),
        profile_environment.as_ref(),
        session_id,
    )
    .await?;

    Ok(ExecSessionContext {
        db,
        may_dir,
        session_id: SessionId::from_string(session_id),
        sandbox_dir: store.sandbox_dir(session_id, profile_id.as_deref()),
        profile_environment,
        profile_model,
        profile_container_preferences: profile_preferences
            .and_then(|preferences| preferences)
            .and_then(|preferences| preferences.container),
    })
}

async fn ensure_exec_session_layout(
    store: &SessionStore,
    profile_id: Option<&str>,
    profile_environment: Option<&ProfileEnvironment>,
    session_id: &str,
) -> Result<(), ExecError> {
    let layout = match profile_environment {
        Some(ProfileEnvironment::Container { .. }) => {
            may_agent::session::SandboxResourceLayout::ContainerMountPoints
        }
        _ => may_agent::session::SandboxResourceLayout::Symlinked,
    };

    if !store.exists(session_id, profile_id) {
        store
            .create_session_dirs(session_id, profile_id, layout)
            .await
            .map_err(|error| ExecError::InitializeSession(error.into()))?;
    }

    if matches!(
        profile_environment,
        Some(ProfileEnvironment::Container { .. })
    ) {
        store
            .reconcile_container_mount_points(session_id, profile_id)
            .await
            .map_err(|error| ExecError::InitializeSession(error.into()))?;
    }

    Ok(())
}

async fn build_exec_terminal_tool(
    context: &ExecSessionContext,
    environment: &may_agent::AgentEnvironment,
    may_config: &MayConfig,
    model: &may_agent::ModelSelector,
    ssh_server_id: Option<&str>,
    allow_runtime_install: bool,
    tool_distribution: Arc<aither_sandbox::ToolDistribution>,
) -> Result<
    TerminalTool<
        TerminalPermissionHandler,
        executor_core::DefaultExecutor,
        aither_sandbox::Configured,
    >,
    ExecError,
> {
    // Only the targeted server is registered, so a local exec never depends
    // on the state of the profile's other attached servers.
    let ssh_server = match ssh_server_id {
        Some(server_id) => Some(resolve_targeted_ssh_server(context, may_config, server_id).await?),
        None => None,
    };
    let ssh_available = ssh_server.is_some();
    let mut shell_sessions = aither_sandbox::ShellSessionRegistry::new(match environment {
        may_agent::AgentEnvironment::Container { .. } => ShellRuntimeAvailability {
            local: false,
            container: true,
            ssh: ssh_available,
        },
        may_agent::AgentEnvironment::Local => ShellRuntimeAvailability {
            local: true,
            container: false,
            ssh: ssh_available,
        },
    });

    if let Some(ssh_server) = ssh_server {
        shell_sessions = shell_sessions
            .with_ssh_servers(vec![ssh_server])
            .map_err(ExecError::ConfigureSshServers)?
            .with_ssh_authorizer(Arc::new(ExecSshAuthorizer {
                allow_runtime_install,
            }));
    }

    let runtime_service = std::sync::Arc::new(may_agent::session::SessionRuntimeService::new(
        context.db.clone(),
        may_agent::ui_event_bus::UiEventBus::new(),
    ));
    let network_policy = may_config
        .preferences
        .network_policy
        .compile()
        .map_err(ExecError::CompileNetworkPolicy)?;

    let terminal_tool: TerminalTool<TerminalPermissionHandler, executor_core::DefaultExecutor> =
        TerminalTool::new_exact(
            &context.sandbox_dir,
            TerminalPermissionHandler::for_exec(
                context.session_id.as_str().to_string(),
                runtime_service,
                network_policy,
                context
                    .profile_model
                    .as_ref()
                    .map(|profile| profile.id.clone()),
            ),
            executor_core::DefaultExecutor,
        )
        .await
        .map_err(ExecError::CreateTerminalTool)?
        .with_network_audit_log(
            aither_sandbox::open_network_audit_log(
                context
                    .may_dir
                    .join(may_agent::config::NETWORK_AUDIT_LOG_SUBDIR),
                may_agent::config::NETWORK_AUDIT_MAX_FILES,
            )
            .await
            .map_err(ExecError::CreateTerminalTool)?,
        );

    let terminal_tool = match environment {
        may_agent::AgentEnvironment::Container {
            container_id,
            container_ipc_route,
            container_exec,
            ..
        } => terminal_tool
            .with_shell_sessions(shell_sessions)
            .with_container_runtime(ContainerShellRuntime::new(
                container_id.clone(),
                container_ipc_route.clone(),
                container_exec.clone(),
            )),
        may_agent::AgentEnvironment::Local => terminal_tool.with_shell_sessions(shell_sessions),
    };

    let tools_dir = may_agent::MayHomePaths::new(context.may_dir.clone()).tools_dir();
    let (terminal_tool, (resolved_provider, tool_dispatchers)) = futures_lite::future::try_zip(
        async {
            terminal_tool
                .with_process_priority(may_config.sandbox.command_priority.process_priority())
                .initialize_tools(&tool_distribution, &tools_dir)
                .await
                .map_err(ExecError::InitializeBaselineTools)
        },
        futures_lite::future::try_zip(
            async {
                resolve_provider_runtime(may_config, model, 0)
                    .await
                    .map_err(ExecError::ResolveProviderRuntime)
            },
            async {
                may_agent::tool_dispatch::ToolDispatchers::spawn(
                    may_agent::tool_dispatch::ToolDispatchConfig {
                        may_dir: context.may_dir.clone(),
                        cwd: context.sandbox_dir.clone(),
                        db: context.db.clone(),
                        browser_config: may_config.browser.clone(),
                        network_policy: may_config.preferences.network_policy.clone(),
                        ask_user_broker: may_agent::ask_user::channel().0,
                        tool_distribution: tool_distribution.clone(),
                    },
                )
                .await
                .map_err(ExecError::InitializeToolDispatch)
            },
        ),
    )
    .await?;
    let (terminal_tool_factory, terminal_tool_factory_receiver) = terminal_tool_factory_channel();
    let registry = build_exec_registry(
        context,
        may_config,
        resolved_provider.llm,
        resolved_provider.search_embedder,
        &tool_dispatchers,
        terminal_tool_factory,
        environment,
    )
    .await?;
    let terminal_tool = terminal_tool.with_registry(registry);
    terminal_tool.start_factory_service(terminal_tool_factory_receiver);
    Ok(terminal_tool)
}

async fn build_exec_registry<LLM>(
    context: &ExecSessionContext,
    _may_config: &MayConfig,
    llm: LLM,
    search_embedder: may_agent::embedding_provider::EmbeddingModelResolution,
    tool_dispatchers: &may_agent::tool_dispatch::ToolDispatchers,
    terminal_tool_factory: aither_sandbox::TerminalToolFactory,
    environment: &may_agent::AgentEnvironment,
) -> Result<Arc<aither_sandbox::ToolRegistry>, ExecError>
where
    LLM: aither_core::LanguageModel + Clone + 'static,
{
    let mut builder = aither_sandbox::ToolRegistryBuilder::new();
    let session_paths = SessionPaths::new(context.sandbox_dir.clone());
    // Issues belong to the project `may exec` runs in, as for the TUI and
    // UI server, never to the session sandbox.
    let cwd = std::env::current_dir().map_err(ExecError::CurrentDir)?;
    let issue_store = may_agent::issue::resolve_issue_store(
        &cwd,
        &session_paths.workspace_config(),
        context.db.clone(),
    )
    .map_err(ExecError::ResolveIssueBackend)?;
    register_shared_terminal_commands(
        &mut builder,
        SharedTerminalCommandConfig {
            llm,
            ask_flagship: None,
            ask_balanced: None,
            ask_fast: None,
            session_id: context.session_id.as_str().to_string(),
            may_dir: context.may_dir.clone(),
            sandbox_dir: context.sandbox_dir.clone(),
            search_embedder,
            ask_user_broker: None,
            imagine: None,
            task_runtime: match environment {
                may_agent::AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                may_agent::AgentEnvironment::Local => TaskRuntimeKind::Local,
            },
            task_broker: Some(tool_dispatchers.task.clone()),
            notify_broker: Some(tool_dispatchers.notify.clone()),
            browser_broker: tool_dispatchers.browser.clone(),
            register_skills_broker: None,
            register_subagents_broker: None,
            workspace_request_broker: None,
            subagent_runs: aither_agent::specialized::SubagentRuns::default(),
            terminal_tool_factory: Some(terminal_tool_factory),
            // exec runs scripts without a model turn, so no provider-hosted
            // web tools exist here and the commands must stay registered.
            request_parameters: aither_core::llm::model::Parameters::default(),
            // exec has no UI reader; the tracker still records harmlessly.
            subagent_tracker: may_agent::agent::SubagentTracker::default(),
            issue_store,
        },
    )
    .await
    .map_err(ExecError::LoadMcpConnections)?;
    Ok(Arc::new(builder.build(
        SessionPaths::new(context.sandbox_dir.clone()).outputs_dir(),
    )))
}

/// Resolves `server_id` among the SSH servers attached to the session's profile.
async fn resolve_targeted_ssh_server(
    context: &ExecSessionContext,
    may_config: &MayConfig,
    server_id: &str,
) -> Result<SshServer, ExecError> {
    let profile_id = context.profile_model.as_ref().map(|p| p.id.as_str());
    let servers = may_agent::profile::resolve_attached_ssh_servers(
        &context.db,
        profile_id,
        &may_config.ssh_servers,
    )
    .await
    .map_err(|source| ExecError::InvalidProfile {
        profile_id: profile_id.unwrap_or_default().to_string(),
        source,
    })?;
    servers
        .into_iter()
        .map(|s| SshServer {
            name: s.name,
            target: s.target,
        })
        .find(|server| server.id() == server_id)
        .ok_or_else(|| ExecError::SshServerNotAttached(server_id.to_string()))
}

fn select_exec_model(config: &MayConfig) -> may_agent::ModelSelector {
    config
        .last_model
        .clone()
        .or_else(|| config.models.advanced.first().cloned())
        .unwrap_or_else(|| panic!("may exec requires at least one configured model"))
}

const fn select_exec_mode(
    args: &ExecArgs,
    _environment: &may_agent::AgentEnvironment,
) -> TerminalExecutionMode {
    if let Some(mode) = args.mode {
        return mode;
    }
    if args.ssh_server_id.is_some() {
        return TerminalExecutionMode::Ssh;
    }
    TerminalExecutionMode::Default
}

/// Process exit code reported for a command outcome. A background task has
/// not finished, so it reports success; a run killed at its time bound reports
/// `-1`.
const fn exit_code(outcome: &CommandOutcome) -> i32 {
    match outcome {
        CommandOutcome::Exited { code } => *code,
        CommandOutcome::Running { .. } => 0,
        CommandOutcome::Killed { .. } => -1,
    }
}

fn render_exec_result(result: &CommandResult) -> Result<(), ExecError> {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let stdout_text = may_agent::command_output::stream_text(&result.stdout);
    if !stdout_text.is_empty() {
        stdout
            .write_all(stdout_text.as_bytes())
            .map_err(ExecError::WriteStdout)?;
        if !stdout_text.ends_with('\n') {
            stdout.write_all(b"\n").map_err(ExecError::WriteStdout)?;
        }
        stdout.flush().map_err(ExecError::WriteStdout)?;
    }

    if let Some(stderr_stream) = &result.stderr {
        let stderr_text = may_agent::command_output::stream_text(stderr_stream);
        if !stderr_text.is_empty() {
            stderr
                .write_all(stderr_text.as_bytes())
                .map_err(ExecError::WriteStderr)?;
            if !stderr_text.ends_with('\n') {
                stderr.write_all(b"\n").map_err(ExecError::WriteStderr)?;
            }
        }
    }

    match &result.outcome {
        CommandOutcome::Exited { .. } => {}
        CommandOutcome::Running { task_id, .. } => stderr
            .write_all(format!("background task: {task_id} (running)\n").as_bytes())
            .map_err(ExecError::WriteStderr)?,
        CommandOutcome::Killed { task_id, .. } => stderr
            .write_all(format!("background task: {task_id} (killed)\n").as_bytes())
            .map_err(ExecError::WriteStderr)?,
    }
    stderr.flush().map_err(ExecError::WriteStderr)?;

    Ok(())
}
