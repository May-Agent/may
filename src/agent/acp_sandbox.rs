//! Where connected ACP agents run: inside the session's sandbox.
//!
//! acpsub starts two kinds of process, the agent itself and each command the
//! agent runs through ACP `terminal/*`. Both start through
//! [`SandboxExecutor`], in a heel sandbox built like the one May's own
//! terminal commands run in: the session directory plus the session's
//! readable and writable grants, and the session's network policy. The
//! agent's permission requests go through [`ApprovalPermissions`] to the
//! session's [`PermissionApprover`]; nothing is answered without it.
//!
//! A [`SessionSandbox`] is the boundary of one session. The executor and the
//! permission handler read it through a shared cell, so an agent rebuilt for
//! the session installs its grants for every process started afterwards. A
//! running agent keeps the grants it started with, as a running terminal
//! command does.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, OnceLock};

use acpsub::{
    AgentProcess, Executor, PermissionHandler, PermissionRequest, ProcessControl, SpawnSpec,
    TerminalProcess,
};
use aither_acp::{PermissionOptionKind, RequestPermissionOutcome};
use aither_sandbox::permission::PermissionHandler as TerminalPermissions;
use aither_sandbox::{ProcessPriority, TerminalGrants, TerminalMode};
use arc_swap::ArcSwap;
use askama::Template;
use event_listener::Event;
use executor_core::DefaultExecutor;
use heel::{
    Access, Audited, DomainRequest, Grant, NetworkAuditLog, NetworkPolicy, ResolvedPath, Sandbox,
    SandboxConfig, SecurityConfig, StdioConfig,
};

use crate::agent::PermissionApprover;
use crate::config::MayConfig;
use crate::terminal_permission_handler::TerminalPermissionHandler;

/// The network policy of an ACP sandbox: the session's terminal verdicts.
#[derive(Clone)]
struct SessionNetwork {
    handler: Arc<TerminalPermissionHandler>,
}

impl NetworkPolicy for SessionNetwork {
    async fn check(&self, request: &DomainRequest) -> bool {
        self.handler
            .authorize_domain(request.host(), request.port())
            .await
    }
}

type AcpSandbox = Sandbox<Audited<SessionNetwork>>;

/// The boundary of one session's ACP processes.
#[derive(Clone)]
pub struct SessionSandbox {
    /// The sandbox's working directory, granted in full: the session
    /// directory, or a scratch directory for a trial.
    root: PathBuf,
    /// Overrides the working directory a process asks for, when the caller
    /// has none of its own to give (a probe starts in the daemon's).
    launch_dir: Option<PathBuf>,
    /// The session terminal's live grants.
    grants: TerminalGrants,
    /// Judges outbound connections like the session's terminal commands.
    network: Arc<TerminalPermissionHandler>,
    audit: NetworkAuditLog,
    priority: ProcessPriority,
    /// Asks the user about the agent's permission requests; without it every
    /// request is refused.
    approver: Option<PermissionApprover>,
}

impl std::fmt::Debug for SessionSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSandbox")
            .field("root", &self.root)
            .field("grants", &self.grants.snapshot())
            .finish_non_exhaustive()
    }
}

impl SessionSandbox {
    /// The sandbox of a session whose terminal has `grants`.
    #[must_use]
    pub fn for_session(
        root: PathBuf,
        grants: TerminalGrants,
        network: TerminalPermissionHandler,
        audit: NetworkAuditLog,
        priority: ProcessPriority,
        approver: Option<PermissionApprover>,
    ) -> Self {
        Self {
            root,
            launch_dir: None,
            grants,
            network: Arc::new(network),
            audit,
            priority,
            approver,
        }
    }

    /// A sandbox granting nothing but `root`, which every process starts in,
    /// and answering no permission request: a connection under trial belongs
    /// to no session.
    ///
    /// # Errors
    /// Returns the error of opening the network audit log under `root`.
    pub fn scratch(root: &Path) -> io::Result<Self> {
        let audit = NetworkAuditLog::rolling_daily(root.join("network-audit"), 1)
            .map_err(io::Error::other)?;
        Ok(Self {
            root: root.to_path_buf(),
            launch_dir: Some(root.to_path_buf()),
            grants: TerminalGrants::new(),
            network: Arc::new(TerminalPermissionHandler::for_agent(None, None, None)),
            audit,
            priority: ProcessPriority::default(),
            approver: None,
        })
    }

    /// The sandbox a process of `spec` starts in, with the grants of this
    /// moment.
    async fn launch(
        &self,
        spec: &SpawnSpec,
        process: &Process,
    ) -> io::Result<(heel::Child, AcpSandbox)> {
        let grants = self.grants.snapshot();
        let program = resolve_program(&spec.program);
        // The command line names what the process runs: the program, and the
        // files among its arguments (an interpreter's script).
        let named_files: Vec<PathBuf> = spec
            .args
            .iter()
            .map(|arg| spec.cwd.join(arg))
            .filter(|path| path.is_file())
            .collect();
        let grants = grants
            .writable
            .iter()
            .map(|path| (path, Access::WRITE | Access::EXEC))
            .chain(grants.readable.iter().map(|path| (path, Access::READ)))
            .chain(program.as_ref().map(|path| (path, Access::EXEC)))
            .chain(named_files.iter().map(|path| (path, Access::READ)))
            // The host toolchains the session's own commands build with.
            .chain(
                toolchain_homes()
                    .iter()
                    .map(|path| (path, Access::WRITE | Access::EXEC)),
            )
            // The agent's own state, which its commands do not share.
            .chain(
                process
                    .state_dirs()
                    .iter()
                    .map(|path| (path, Access::WRITE | Access::EXEC)),
            )
            .map(|(path, access)| {
                resolved(path)
                    .map(|path| Grant::new(path, access))
                    .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let config = SandboxConfig::builder()
            .network(Audited::new(
                SessionNetwork {
                    handler: self.network.clone(),
                },
                self.audit.clone(),
            ))
            .working_dir(canonical(&self.root))
            .grants(grants)
            .security(SecurityConfig::interactive())
            // The session's own commands run their build outputs.
            .executable_working_dir(true)
            .priority(self.priority)
            .build();
        let sandbox: AcpSandbox = Sandbox::with_config_and_executor(config, DefaultExecutor)
            .await
            .map_err(io::Error::other)?;
        let cwd = self.launch_dir.as_deref().unwrap_or(&spec.cwd);
        let program = program.map_or_else(
            || spec.program.clone(),
            |path| path.to_string_lossy().into_owned(),
        );
        let child = sandbox
            .command(program)
            .args(&spec.args)
            .envs(self.base_environment(process)?)
            .envs(&spec.env)
            .current_dir(cwd)
            .stdin(if process.pipes_stdin() {
                StdioConfig::Piped
            } else {
                StdioConfig::Null
            })
            .stdout(StdioConfig::Piped)
            .stderr(StdioConfig::Piped)
            .spawn()
            .await
            .map_err(io::Error::other)?;
        Ok((child, sandbox))
    }
}

/// Which process is starting.
enum Process {
    /// The agent, with the directories its configuration grants it.
    Agent { state: Vec<PathBuf> },
    /// A command the agent runs.
    Terminal,
}

impl Process {
    const fn pipes_stdin(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }

    fn state_dirs(&self) -> &[PathBuf] {
        match self {
            Self::Agent { state } => state,
            Self::Terminal => &[],
        }
    }
}

impl SessionSandbox {
    /// The environment under the process's own: the host `PATH`, as the
    /// session's terminal commands get it. The agent finds its own config
    /// and login through the user's real home; its commands keep the
    /// session directory as home, like every command of the session.
    fn base_environment(&self, process: &Process) -> io::Result<Vec<(String, String)>> {
        let mut environment = Vec::new();
        if let Some(path) = std::env::var_os("PATH") {
            environment.push(("PATH".to_string(), path.to_string_lossy().into_owned()));
        }
        let root = canonical(&self.root);
        match process {
            Process::Agent { .. } => environment.push((
                "HOME".to_string(),
                canonical(&real_home()?).to_string_lossy().into_owned(),
            )),
            Process::Terminal => environment.extend([
                ("HOME".to_string(), root.to_string_lossy().into_owned()),
                (
                    "XDG_CACHE_HOME".to_string(),
                    root.join(".cache").to_string_lossy().into_owned(),
                ),
                (
                    "XDG_CONFIG_HOME".to_string(),
                    root.join(".config").to_string_lossy().into_owned(),
                ),
            ]),
        }
        Ok(environment)
    }
}

/// The user's home, which the daemon runs as.
fn real_home() -> io::Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| io::Error::other("the user's home directory is unknown"))
}

/// The host toolchain homes that exist, which the session's terminal grants
/// its commands (`CARGO_HOME` and `RUSTUP_HOME`, else `~/.cargo` and
/// `~/.rustup`).
fn toolchain_homes() -> Vec<PathBuf> {
    let home = std::env::home_dir();
    [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")]
        .into_iter()
        .filter_map(|(variable, default)| {
            std::env::var_os(variable)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
        })
        .filter(|path| path.is_dir())
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolved(path: &Path) -> heel::Result<ResolvedPath> {
    ResolvedPath::new(canonical(path))
}

/// The file a command name starts, so the sandbox may execute it: the host
/// finds it on `PATH`, where the sandbox's own search path would not.
fn resolve_program(program: &str) -> Option<PathBuf> {
    which::which(program).ok().map(|path| canonical(&path))
}

/// Starts ACP processes in the sandbox of the session installed in `cell`.
#[derive(Debug, Clone)]
pub struct SandboxExecutor {
    cell: Arc<ArcSwap<SessionSandbox>>,
    config: Arc<ArcSwap<MayConfig>>,
}

/// A process started in the sandbox.
#[derive(Debug)]
pub struct SandboxProcess {
    pid: u32,
    exit: Arc<Exit>,
}

#[derive(Debug, Default)]
struct Exit {
    status: OnceLock<Result<ExitStatus, String>>,
    changed: Event,
}

impl SandboxProcess {
    /// Reaps `child` on the blocking pool and publishes its status. The
    /// sandbox ends its processes when dropped, so it lives until then.
    fn reap(mut child: heel::Child, sandbox: AcpSandbox) -> Self {
        let pid = child.id();
        let exit = Arc::new(Exit::default());
        let published = exit.clone();
        blocking::unblock(move || {
            let _sandbox = sandbox;
            let status = futures_lite::future::block_on(child.wait()).map_err(|e| e.to_string());
            let _ = published.status.set(status);
            published.changed.notify(usize::MAX);
        })
        .detach();
        Self { pid, exit }
    }
}

impl ProcessControl for SandboxProcess {
    fn kill(&self) {
        // The process leads its own group, which keeps the descendants it
        // orphaned; a group that is gone is not an error.
        #[cfg(unix)]
        if self.pid != 0 {
            // SAFETY: kill(2) only delivers a signal to a group this process
            // started; a group already gone answers ESRCH.
            unsafe {
                libc::kill(
                    -libc::pid_t::try_from(self.pid).expect("pids fit pid_t"),
                    libc::SIGKILL,
                );
            }
        }
    }

    async fn wait(&self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.exit.status.get() {
                return status.clone().map_err(io::Error::other);
            }
            let listener = self.exit.changed.listen();
            if let Some(status) = self.exit.status.get() {
                return status.clone().map_err(io::Error::other);
            }
            listener.await;
        }
    }
}

fn pipe(file: Option<File>, name: &str) -> io::Result<async_fs::File> {
    file.map(async_fs::File::from)
        .ok_or_else(|| io::Error::other(format!("{name} was not piped")))
}

impl SandboxExecutor {
    /// An executor starting processes in whatever sandbox `cell` holds when
    /// they start, giving an agent the grants `config` lists for its command
    /// when it starts.
    #[must_use]
    pub const fn new(cell: Arc<ArcSwap<SessionSandbox>>, config: Arc<ArcSwap<MayConfig>>) -> Self {
        Self { cell, config }
    }

    /// The directories the connections of `spec`'s command grant their
    /// agent: connections sharing a command and arguments are one program.
    fn state_dirs(&self, spec: &SpawnSpec) -> io::Result<Vec<PathBuf>> {
        let config = self.config.load();
        let mut matching = config
            .acp_agents
            .values()
            .filter(|agent| (&agent.command, &agent.args) == (&spec.program, &spec.args))
            .peekable();
        if matching.peek().is_none() {
            return Ok(Vec::new());
        }
        let home = canonical(&real_home()?);
        Ok(matching.flat_map(|agent| agent.grants_in(&home)).collect())
    }
}

impl Executor for SandboxExecutor {
    type Process = SandboxProcess;
    type Stdin = async_fs::File;
    type Stdout = async_fs::File;
    type Stderr = async_fs::File;

    async fn spawn_agent(&self, spec: SpawnSpec) -> io::Result<AgentProcess<Self>> {
        let process = Process::Agent {
            state: self.state_dirs(&spec)?,
        };
        let (mut child, sandbox) = self.cell.load_full().launch(&spec, &process).await?;
        let stdin = pipe(child.take_stdin(), "stdin")?;
        let stdout = pipe(child.take_stdout(), "stdout")?;
        let stderr = pipe(child.take_stderr(), "stderr")?;
        Ok(AgentProcess {
            stdin,
            stdout,
            stderr,
            control: SandboxProcess::reap(child, sandbox),
        })
    }

    async fn spawn_terminal(&self, spec: SpawnSpec) -> io::Result<TerminalProcess<Self>> {
        let (mut child, sandbox) = self
            .cell
            .load_full()
            .launch(&spec, &Process::Terminal)
            .await?;
        let stdout = pipe(child.take_stdout(), "stdout")?;
        let stderr = pipe(child.take_stderr(), "stderr")?;
        Ok(TerminalProcess {
            stdout,
            stderr,
            control: SandboxProcess::reap(child, sandbox),
        })
    }
}

/// What the user reads when an ACP agent asks permission.
#[derive(Template)]
#[template(path = "acp_permission_request.txt", escape = "none")]
struct PermissionText<'a> {
    title: &'a str,
    cwd: &'a Path,
}

/// Answers an agent's permission requests through the session's approver:
/// the request the user sees, the history it is recorded in and the yolo
/// switch are those of an `unsafe` terminal request.
#[derive(Debug, Clone)]
pub struct ApprovalPermissions {
    cell: Arc<ArcSwap<SessionSandbox>>,
}

impl ApprovalPermissions {
    /// A handler asking the approver of whatever sandbox `cell` holds.
    #[must_use]
    pub const fn new(cell: Arc<ArcSwap<SessionSandbox>>) -> Self {
        Self { cell }
    }
}

impl PermissionHandler for ApprovalPermissions {
    async fn decide(&self, request: PermissionRequest) -> RequestPermissionOutcome {
        let sandbox = self.cell.load_full();
        let approved = match &sandbox.approver {
            Some(approver) => {
                let script = PermissionText {
                    title: &request.params.tool_call.title,
                    cwd: &request.cwd,
                }
                .render()
                .expect("the permission template renders");
                match approver
                    .clone()
                    .for_subagent()
                    .request_and_wait(TerminalMode::Sandboxed, script)
                    .await
                {
                    Ok(answer) => answer,
                    Err(error) => {
                        tracing::warn!(%error, "an ACP permission request was not approved");
                        false
                    }
                }
            }
            None => false,
        };
        let kinds = if approved {
            [
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            ]
        } else {
            [
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ]
        };
        request
            .params
            .options
            .iter()
            .find(|option| kinds.contains(&option.kind))
            .map_or(RequestPermissionOutcome::Cancelled, |option| {
                RequestPermissionOutcome::Selected {
                    option_id: option.option_id.clone(),
                }
            })
    }
}
