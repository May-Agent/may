//! `may exec` subcommand implementation.
//!
//! Runs a one-shot terminal command inside a session's home
//! environment (local or container) and renders the combined
//! stdout/stderr/task-id payload back to the caller. When the profile
//! has attached SSH servers, the command may select one via
//! `--ssh-server <name>`.

use std::io::{self, Write};
use std::sync::Arc;

use aither_sandbox::{
    ContainerShellRuntime, Grants, Interpreter, MediaResolution, RunOutcome, SandboxMode,
    ScriptResult, ShellArgs, SshServer,
};
use may_agent::ipc::{CommandRegistryBuilder, SessionShell, ShellSlot};
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
    #[error("failed to resolve May home directory: {0}")]
    MissingHomeDirectory(may_agent::config::ConfigError),
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
    #[error("failed to register terminal commands: {0}")]
    RegisterCommands(may_agent::TerminalCommandsError),
    #[error("failed to read the current working directory: {0}")]
    CurrentDir(std::io::Error),
    #[error("failed to resolve the session issue backend: {0}")]
    ResolveIssueBackend(may_agent::issue::IssueError),
    #[error("failed to load workspace config: {0}")]
    WorkspaceConfig(may_agent::workspace::WorkspaceConfigError),
    #[error("failed to resolve provider runtime: {0}")]
    ResolveProviderRuntime(may_agent::agent::MayAgentError),
    #[error("failed to load May config: {0}")]
    LoadConfig(may_agent::config::ConfigError),
    #[error("failed to compile network policy: {0}")]
    CompileNetworkPolicy(aither_sandbox::NetworkPolicyError),
    #[error("failed to create terminal tool: {0}")]
    CreateTerminalTool(aither_sandbox::TerminalError),
    #[error("failed to restore the session's model-facing ids: {0}")]
    SessionIds(may_agent::db::ChatDbError),
    #[error("failed to start the May daemon: {0}")]
    StartDaemon(may_agent::daemon::lifecycle::LifecycleError),
    #[error("failed to connect to the daemon for the session's ids: {0}")]
    ConnectDaemonIds(std::io::Error),
    #[error("failed to connect to the daemon's jobserver pool: {0}")]
    ConnectJobserver(aither_sandbox::JobserverError),
    #[error("failed to initialize baseline tools: {0}")]
    InitializeBaselineTools(#[from] may_agent::tools::ToolRuntimeError),
    #[error("failed to initialize tool dispatch runtime: {0}")]
    InitializeToolDispatch(may_agent::tool_dispatch::ToolDispatchError),
    #[error("ssh server '{0}' is not attached to the session's profile")]
    SshServerNotAttached(String),
    #[error("terminal execution failed: {0}")]
    TerminalCall(aither_core::Error),
    #[error("failed to build the shell: {0}")]
    BuildShell(may_agent::shell::ShellAssemblyError),
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
    )
    .await
    .map_err(ExecError::ResolveEnvironment)?;

    let model = select_exec_model(&may_config);
    let sandbox = select_exec_mode(&args, &environment);
    let shell = Box::pin(build_exec_shell(
        &context,
        &environment,
        &may_config,
        &model,
        args.ssh_server_id.as_deref(),
        tool_distribution,
    ))
    .await?;

    // `may exec` reports the final result, so the command runs detached
    // under its `--timeout` kill bound and the call waits for completion.
    let job = shell
        .run_background(ShellArgs {
            description: "execute the requested shell command".to_string(),
            script: args.script,
            interpreter: Interpreter::Shell,
            sandbox,
            location: args.ssh_server_id,
            resolution: MediaResolution::Auto,
            max_lines: Some(args.max_lines),
            raw: args.raw,
            timeout: args.timeout,
            background: true,
        })
        .await
        .map_err(ExecError::TerminalCall)?;
    let task_id = job.task_id().to_owned();
    let result = job
        .wait()
        .await
        .map_err(|error| ExecError::TerminalCall(anyhow::anyhow!(error.to_string())))?;
    render_exec_result(&task_id, &result)?;
    Ok(exit_code(&result.outcome))
}

async fn load_exec_session_context(session_id: &str) -> Result<ExecSessionContext, ExecError> {
    let may_dir = MayConfig::may_dir().map_err(ExecError::MissingHomeDirectory)?;
    // The daemon owns the session's model-facing ids and the machine's job
    // pool, so it is started on demand; every id exec leases comes from the
    // daemon's pool of the session.
    let daemon = may_agent::daemon::DaemonPaths::new(may_dir.clone());
    // Persistence comes first, as for every client, so the daemon a run
    // starts never migrates the database alongside it.
    let (store, db, _default_profile_id) = initialize_session_persistence(&may_dir)
        .await
        .map_err(ExecError::InitializeSession)?;
    // A missing session fails before anything starts a daemon for it.
    let session_row = db
        .get_session(session_id)
        .await
        .map_err(|error| ExecError::InitializeSession(error.into()))?
        .ok_or_else(|| ExecError::SessionNotFound(session_id.to_string()))?;
    may_agent::daemon::lifecycle::ensure_running(&daemon, |line| {
        let _ = writeln!(io::stderr(), "[may] {line}");
    })
    .await
    .map_err(ExecError::StartDaemon)?;
    let db = db.lease_ids_from(
        may_agent::client::ids::DaemonIds::connect(&daemon.socket())
            .await
            .map_err(ExecError::ConnectDaemonIds)?,
    );
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
            let (attachments, cloud) = futures_lite::future::try_zip(
                db.list_profile_remote_servers(&model.id),
                db.profile_cloud(&model.id),
            )
            .await
            .map_err(|source| ExecError::LoadProfile {
                profile_id: profile_id
                    .clone()
                    .expect("profile id present when model exists"),
                source,
            })?;
            Some(
                ProfileInfo::from_db_parts(model, preferences.clone(), attachments, cloud)
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

async fn build_exec_shell(
    context: &ExecSessionContext,
    environment: &may_agent::AgentEnvironment,
    may_config: &MayConfig,
    model: &may_agent::ModelSelector,
    ssh_server_id: Option<&str>,
    tool_distribution: Arc<may_agent::tools::ToolDistribution>,
) -> Result<Arc<SessionShell>, ExecError> {
    // Only the targeted server is registered, so a local exec never depends
    // on the state of the profile's other attached servers.
    let ssh_server = match ssh_server_id {
        Some(server_id) => Some(resolve_targeted_ssh_server(context, may_config, server_id).await?),
        None => None,
    };
    let ssh_available = ssh_server.is_some();

    let runtime_service = std::sync::Arc::new(may_agent::session::SessionRuntimeService::new(
        context.db.clone(),
        may_agent::ui_event_bus::UiEventBus::new(),
        may_agent::MayHomePaths::new(context.may_dir.clone()),
    ));
    let network_policy = may_config
        .preferences
        .network_policy
        .compile()
        .map_err(ExecError::CompileNetworkPolicy)?;

    let session_ids = context
        .db
        .session_ids(context.session_id.as_str())
        .await
        .map_err(ExecError::SessionIds)?;
    let permissions = TerminalPermissionHandler::for_exec(
        context.session_id.as_str().to_string(),
        runtime_service,
        network_policy,
        context
            .profile_model
            .as_ref()
            .map(|profile| profile.id.clone()),
    );
    let audit_log = aither_sandbox::open_network_audit_log(
        context
            .may_dir
            .join(may_agent::config::NETWORK_AUDIT_LOG_SUBDIR),
        may_agent::config::NETWORK_AUDIT_MAX_FILES,
    )
    .await
    .map_err(ExecError::CreateTerminalTool)?;
    // Local commands draw their jobs from the daemon's machine-wide pool, as
    // the daemon's own sessions do; a container or an SSH server has none.
    let pool = match environment {
        may_agent::AgentEnvironment::Local if !ssh_available => Some(
            may_agent::jobserver::connect_running(&context.may_dir)
                .map_err(ExecError::ConnectJobserver)?,
        ),
        _ => None,
    };
    let container = match environment {
        may_agent::AgentEnvironment::Container {
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
            .with_workspace_mount(may_agent::container::lifecycle::workspace_mount(
                &context.sandbox_dir,
            )),
        ),
        may_agent::AgentEnvironment::Local => None,
    };

    let tools_dir = may_agent::MayHomePaths::new(context.may_dir.clone()).tools_dir();
    let (tools, (resolved_provider, tool_dispatchers)) = futures_lite::future::try_zip(
        async {
            match environment {
                may_agent::AgentEnvironment::Container {
                    container_id,
                    container_exec,
                    ..
                } => {
                    may_agent::tools::ToolEnvironments::prepare_container(
                        container_exec.clone(),
                        container_id,
                        &tool_distribution,
                        &tools_dir,
                    )
                    .await
                }
                may_agent::AgentEnvironment::Local => {
                    may_agent::tools::ToolEnvironments::prepare_host(&tool_distribution, &tools_dir)
                        .await
                }
            }
            .map_err(ExecError::InitializeBaselineTools)
        },
        futures_lite::future::try_zip(
            async {
                resolve_provider_runtime(
                    may_config,
                    model,
                    0,
                    &may_agent::secrets::SecretRedactor::new(&context.may_dir),
                )
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
    let budgets = may_agent::shell::ShellBudgets::of(&may_config.shell);
    let commands = Box::pin(build_exec_commands(
        context,
        may_config,
        &budgets,
        resolved_provider.llm,
        &tool_dispatchers,
        environment,
    ))
    .await?;
    let grants = Grants::new();
    grants.add_writable(&context.sandbox_dir);
    may_agent::shell::ShellAssembly {
        tools,
        container,
        working_dir: context.sandbox_dir.clone(),
        grants,
        priority: may_config.sandbox.command_priority.process_priority(),
        audit_log,
        permissions,
        env_source: None,
        budgets,
        interpreters: vec![Interpreter::Shell],
        policies: may_agent::command_policy::PolicySet {
            managed_bash: may_agent::command_policy::runs_managed_bash(matches!(
                environment,
                may_agent::AgentEnvironment::Container { .. }
            )),
            refused: may_agent::command_policy::RefusedReads::default(),
        },
        slot: ShellSlot::new(),
        ssh_servers: ssh_server.into_iter().collect(),
        ids: Some(session_ids.allocator().clone()),
        spaces: may_agent::agent::subagent_spaces::SessionSubagentSpaces::without_clones(),
        pool,
        connectivity: None,
    }
    .build(commands)
    .await
    .map_err(ExecError::BuildShell)
}

async fn build_exec_commands<LLM>(
    context: &ExecSessionContext,
    may_config: &MayConfig,
    budgets: &may_agent::shell::ShellBudgets,
    llm: LLM,
    tool_dispatchers: &may_agent::tool_dispatch::ToolDispatchers,
    environment: &may_agent::AgentEnvironment,
) -> Result<may_agent::ipc::CommandRegistry, ExecError>
where
    LLM: aither_core::LanguageModel + Clone + Sync + 'static,
{
    let mut builder = CommandRegistryBuilder::new();
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
            #[cfg(target_os = "macos")]
            computer: None,
            rules_import: None,
            services: may_agent::SharedServices::new(&context.may_dir),
            llm,
            ask_flagship: None,
            ask_balanced: None,
            ask_fast: None,
            session_id: context.session_id.as_str().to_string(),
            may_dir: context.may_dir.clone(),
            sandbox_dir: context.sandbox_dir.clone(),
            cwd: cwd.clone(),
            db: context.db.clone(),
            ask_user: None,
            imagine: None,
            task_runtime: match environment {
                may_agent::AgentEnvironment::Container { .. } => TaskRuntimeKind::Container,
                may_agent::AgentEnvironment::Local => TaskRuntimeKind::Local,
            },
            task_broker: Some(tool_dispatchers.task.clone()),
            tunnel_broker: Some(tool_dispatchers.tunnel.clone()),
            browser_broker: tool_dispatchers.browser.clone(),
            register_skills_broker: None,
            workspace_request_broker: None,
            // exec launches no subagent of its own; the assembly is only
            // what the `subagent` command names.
            subagents: may_agent::terminal_commands::SubagentAssembly {
                toolkit: may_agent::shell::ShellToolkit::of(budgets),
                cache_lifetime: may_agent::cache_lifetime::CacheLifetime::default().into(),
                keep_alive: None,
                instructions: Vec::new(),
            },
            // exec runs scripts without a model turn, so no provider-hosted
            // web tools exist here and the commands must stay registered.
            request_parameters: aither_core::llm::model::Parameters::default(),
            // exec has no UI reader; the tracker still records harmlessly.
            subagent_tracker: may_agent::agent::SubagentTracker::default(),
            issue_store,
            // exec has no agent and so no `tasks.md` to follow its changes.
            task_events: may_agent::harness::working_document::WorkEvents::new(),
            // `may exec` runs the user's own script: nobody to ask.
            publish_gate: None,
            workspace_config: may_agent::workspace::WorkspaceConfig::load(
                &session_paths.workspace_config(),
            )
            .map_err(ExecError::WorkspaceConfig)?,
            // exec is no main agent: it has no turn boundary to deliver at.
            agent_messaging: None,
            languages: None,
            acp: None,
            git: may_agent::host_git::HostGit::inherit(),
            timeout_cap: budgets.limits().timeout_cap,
            tasks_line_limit: may_agent::tasks_md::schema::LineLimit::new(
                may_config.tasks.line_limit,
            ),
        },
    )
    .await
    .map_err(ExecError::RegisterCommands)?;
    Ok(builder.build())
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

fn select_exec_mode(args: &ExecArgs, _environment: &may_agent::AgentEnvironment) -> SandboxMode {
    args.mode.unwrap_or_default()
}

/// Process exit code reported for a command outcome: a run killed at its
/// time bound reports `-1`.
const fn exit_code(outcome: &RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Exited { code } => *code,
        RunOutcome::Killed(_) => -1,
    }
}

fn render_exec_result(task_id: &str, result: &ScriptResult) -> Result<(), ExecError> {
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
        RunOutcome::Exited { .. } => {}
        RunOutcome::Killed(_) => stderr
            .write_all(format!("background task: {task_id} (killed)\n").as_bytes())
            .map_err(ExecError::WriteStderr)?,
    }
    stderr.flush().map_err(ExecError::WriteStderr)?;

    Ok(())
}
