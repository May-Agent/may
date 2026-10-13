//! `may exec` subcommand implementation.
//!
//! Runs a one-shot terminal command inside a session's home
//! environment (local or container) and renders the combined
//! stdout/stderr/task-id payload back to the caller. When the profile
//! has attached SSH servers, the command may select one via
//! `--ssh-server <name>`.

use std::io::{self, Write};
use std::sync::Arc;

use aither_core::llm::Tool;
use aither_sandbox::{
    ContainerShellRuntime, OutputFormat, ShellRuntimeAvailability, SshServer, TerminalArgs,
    TerminalExecutionMode, TerminalResult, TerminalTool, terminal_tool_factory_channel,
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
    #[error("terminal execution failed: {0}")]
    TerminalCall(aither_core::Error),
    #[error("failed to parse terminal result: {0}")]
    ParseTerminalResult(serde_json::Error),
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
    async fn authorize_connect(
        &self,
        _target: &str,
    ) -> Result<bool, aither_sandbox::SshAuthorizationError> {
        Ok(true)
    }

    async fn authorize_runtime_install(
        &self,
        _target: &str,
        _details: &str,
    ) -> Result<bool, aither_sandbox::SshAuthorizationError> {
        Ok(self.allow_runtime_install)
    }
}

pub async fn run_exec(args: ExecArgs) -> Result<i32, ExecError> {
    let context = load_exec_session_context(&args.session_id).await?;
    let config_path = context.may_dir.join("config.toml");
    let may_config = MayConfig::load_from(&config_path)
        .await
        .map_err(ExecError::LoadConfig)?;

    let environment = resolve_agent_environment_for_session(
        context.profile_environment.clone(),
        context.profile_model.as_ref(),
        context.profile_container_preferences.clone(),
        &context.may_dir,
        &context.sandbox_dir,
        &context.session_id,
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
        args.allow_runtime_install,
    )
    .await?;

    let output = terminal_tool
        .call(
            TerminalArgs {
                description: "execute the requested shell command".to_string(),
                script: args.script,
                mode: shell_mode,
                ssh_server_id: args.ssh_server_id,
                expect: OutputFormat::Text,
                resolution: aither_sandbox::MediaResolution::Auto,
                timeout: args.timeout,
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
    let payload = output.render_for_model().map_err(ExecError::TerminalCall)?;
    let result: TerminalResult =
        serde_json::from_str(&payload).map_err(ExecError::ParseTerminalResult)?;
    render_exec_result(&result)?;
    Ok(result.exit_code)
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
    let profile_model = match profile_id.as_deref() {
        Some(profile_id) => Some(
            db.get_profile(profile_id)
                .await
                .map_err(|source| ExecError::LoadProfile {
                    profile_id: profile_id.to_string(),
                    source,
                })?
                .ok_or_else(|| ExecError::LoadProfile {
                    profile_id: profile_id.to_string(),
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
        &profile_id,
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
    profile_id: &Option<String>,
    profile_environment: Option<&ProfileEnvironment>,
    session_id: &str,
) -> Result<(), ExecError> {
    let layout = match profile_environment {
        Some(ProfileEnvironment::Container { .. }) => {
            may_agent::session::SandboxResourceLayout::ContainerMountPoints
        }
        _ => may_agent::session::SandboxResourceLayout::Symlinked,
    };

    if !store.exists(session_id, profile_id.as_deref()) {
        store
            .create_session_dirs(session_id, profile_id.as_deref(), layout)
            .await
            .map_err(|error| ExecError::InitializeSession(error.into()))?;
    }

    if matches!(
        profile_environment,
        Some(ProfileEnvironment::Container { .. })
    ) {
        store
            .reconcile_container_mount_points(session_id, profile_id.as_deref())
            .await
            .map_err(|error| ExecError::InitializeSession(error.into()))?;
    }

    Ok(())
}

async fn build_exec_terminal_tool(
    context: &ExecSessionContext,
    environment: &may_agent::AgentEnvironment,
    may_config: &MayConfig,
    model: &str,
    allow_runtime_install: bool,
) -> Result<
    TerminalTool<
        TerminalPermissionHandler,
        executor_core::DefaultExecutor,
        aither_sandbox::Configured,
    >,
    ExecError,
> {
    let ssh_servers = resolve_attached_ssh_servers(context, may_config).await?;
    let ssh_available = !ssh_servers.is_empty();
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

    if !ssh_servers.is_empty() {
        shell_sessions = shell_sessions
            .with_ssh_servers(ssh_servers)
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
        _ => terminal_tool.with_shell_sessions(shell_sessions),
    };

    let distribution = may_config.tools.load_distribution().await?;
    let terminal_tool = terminal_tool
        .with_process_priority(may_config.sandbox.command_priority.process_priority())
        .initialize_tools(&distribution, &context.may_dir.join("tools/cache"))
        .await?;
    let resolved_provider = resolve_provider_runtime(may_config, model, 0)
        .await
        .map_err(ExecError::ResolveProviderRuntime)?;
    let (terminal_tool_factory, terminal_tool_factory_receiver) = terminal_tool_factory_channel();
    let tool_dispatchers = may_agent::tool_dispatch::ToolDispatchers::spawn(
        may_agent::tool_dispatch::ToolDispatchConfig {
            may_dir: context.may_dir.clone(),
            cwd: context.sandbox_dir.clone(),
            db: context.db.clone(),
            browser_config: may_config.browser.clone(),
            network_policy: may_config.preferences.network_policy.clone(),
            ask_user_broker: may_agent::ask_user::channel().0,
        },
    )
    .await
    .map_err(ExecError::InitializeToolDispatch)?;
    let registry = build_exec_registry(
        context,
        may_config,
        resolved_provider.llm,
        resolved_provider.search_embedder,
        &tool_dispatchers,
        terminal_tool_factory,
        terminal_tool.job_registry(),
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
    terminal_jobs: aither_sandbox::JobRegistry,
    environment: &may_agent::AgentEnvironment,
) -> Result<Arc<aither_sandbox::ToolRegistry>, ExecError>
where
    LLM: aither_core::LanguageModel + Clone + 'static,
{
    let mut builder = aither_sandbox::ToolRegistryBuilder::new();
    register_shared_terminal_commands(
        &mut builder,
        terminal_jobs,
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
            terminal_tool_factory: Some(terminal_tool_factory),
            // exec runs scripts without a model turn, so no provider-hosted
            // web tools exist here and the commands must stay registered.
            request_parameters: aither_core::llm::model::Parameters::default(),
        },
    )
    .await
    .map_err(ExecError::LoadMcpConnections)?;
    Ok(Arc::new(builder.build(
        SessionPaths::new(context.sandbox_dir.clone()).outputs_dir(),
    )))
}

async fn resolve_attached_ssh_servers(
    context: &ExecSessionContext,
    may_config: &MayConfig,
) -> Result<Vec<SshServer>, ExecError> {
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
    Ok(servers
        .into_iter()
        .map(|s| SshServer {
            name: s.name,
            target: s.target,
        })
        .collect())
}

fn select_exec_model(config: &MayConfig) -> String {
    config
        .last_model
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| config.models.advanced.first().map(ToString::to_string))
        .unwrap_or_else(|| panic!("may exec requires at least one configured model"))
}

fn select_exec_mode(
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

fn render_exec_result(result: &TerminalResult) -> Result<(), ExecError> {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let stdout_text = result.stdout.to_string();
    if !stdout_text.is_empty() {
        stdout
            .write_all(stdout_text.as_bytes())
            .map_err(ExecError::WriteStdout)?;
        if !stdout_text.ends_with('\n') {
            stdout.write_all(b"\n").map_err(ExecError::WriteStdout)?;
        }
        stdout.flush().map_err(ExecError::WriteStdout)?;
    }

    if let Some(stderr_entry) = &result.stderr {
        let stderr_text = stderr_entry.to_string();
        if !stderr_text.is_empty() {
            stderr
                .write_all(stderr_text.as_bytes())
                .map_err(ExecError::WriteStderr)?;
            if !stderr_text.ends_with('\n') {
                stderr.write_all(b"\n").map_err(ExecError::WriteStderr)?;
            }
        }
    }

    if let Some(task_id) = &result.task_id {
        let status = result.status.as_deref().unwrap_or("running");
        stderr
            .write_all(format!("background task: {task_id} ({status})\n").as_bytes())
            .map_err(ExecError::WriteStderr)?;
    }
    stderr.flush().map_err(ExecError::WriteStderr)?;

    Ok(())
}
