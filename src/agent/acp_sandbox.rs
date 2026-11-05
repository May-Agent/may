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

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, OnceLock};

use acpsub::{
    AgentProcess, Executor, PermissionHandler, PermissionRequest, ProcessControl, SpawnSpec,
    TerminalProcess,
};
use aither_acp::{PermissionOption, PermissionOptionKind, RequestPermissionOutcome};
use aither_sandbox::permission::PermissionHandler as TerminalPermissions;
use aither_sandbox::{ProcessPriority, TerminalGrants, TerminalMode};
use arc_swap::ArcSwap;
use askama::Template;
use event_listener::Event;
use executor_core::DefaultExecutor;
use heel::{
    Access, Audited, DenyAll, DomainRequest, Grant, NetworkAuditLog, NetworkPolicy, ResolvedPath,
    Sandbox, SandboxConfig, SecurityConfig, StdioConfig,
};

use crate::agent::PermissionApprover;
use crate::config::MayConfig;
use crate::terminal_permission_handler::TerminalPermissionHandler;

/// Judges a session's ACP connections like its terminal commands' own.
#[derive(Clone)]
pub struct SessionNetwork {
    handler: Arc<TerminalPermissionHandler>,
}

impl std::fmt::Debug for SessionNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionNetwork").finish_non_exhaustive()
    }
}

impl NetworkPolicy for SessionNetwork {
    async fn check(&self, request: &DomainRequest) -> bool {
        self.handler
            .authorize_domain(request.host(), request.port())
            .await
    }
}

/// The network policy of a session's ACP sandbox: the session's terminal
/// verdicts, recorded in its network audit log.
pub type SessionPolicy = Audited<SessionNetwork>;

/// The boundary of one session's ACP processes; `N` decides their network.
#[derive(Clone)]
pub struct SessionSandbox<N> {
    /// The sandbox's working directory, granted in full: the session
    /// directory, or a scratch directory for a probe or trial.
    root: PathBuf,
    /// Overrides the working directory a process asks for, when the caller
    /// has none of its own to give (a probe starts in the daemon's).
    launch_dir: Option<PathBuf>,
    /// The session terminal's live grants.
    grants: TerminalGrants,
    network: N,
    priority: ProcessPriority,
    /// Asks the user about the agent's permission requests; without it every
    /// request is refused.
    approver: Option<PermissionApprover>,
}

impl<N> std::fmt::Debug for SessionSandbox<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSandbox")
            .field("root", &self.root)
            .field("grants", &self.grants.snapshot())
            .finish_non_exhaustive()
    }
}

impl SessionSandbox<SessionPolicy> {
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
            network: Audited::new(
                SessionNetwork {
                    handler: Arc::new(network),
                },
                audit,
            ),
            priority,
            approver,
        }
    }
}

impl SessionSandbox<DenyAll> {
    /// A sandbox granting nothing but `root`, which every process starts in,
    /// with no network and answering no permission request: a command under
    /// probe or trial belongs to no session.
    #[must_use]
    pub fn scratch(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            launch_dir: Some(root.to_path_buf()),
            grants: TerminalGrants::new(),
            network: DenyAll,
            priority: ProcessPriority::default(),
            approver: None,
        }
    }
}

/// A process started in an ACP sandbox. On Linux it runs under heel's
/// supervisor, which owns its whole tree whatever session or group a
/// descendant moves to.
#[cfg(target_os = "linux")]
type AcpChild = heel::supervisor::SupervisedChild<heel::supervisor::Foreground>;
#[cfg(not(target_os = "linux"))]
type AcpChild = heel::Child;

/// The budget a supervised tree's teardown gets to see its processes exit,
/// as aither gives May's own commands.
#[cfg(target_os = "linux")]
const SUPERVISED_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl<N: NetworkPolicy + Clone> SessionSandbox<N> {
    /// The sandbox a process of `spec` starts in, with the grants of this
    /// moment.
    async fn launch(
        &self,
        spec: &SpawnSpec,
        process: &Process,
    ) -> io::Result<(AcpChild, Sandbox<N>)> {
        let snapshot = self.grants.snapshot();
        let root = canonical(&self.root)?;
        let cwd = self.launch_dir.as_deref().unwrap_or(&spec.cwd);
        // The program starts by the path `PATH` gives, so a link starts under
        // its own name (`cargo` is a link to `rustup`, which dispatches on
        // it). A grant never names a link, so its target is what the sandbox
        // may execute.
        let program = resolve_program(&spec.program, cwd)?;
        let target = canonical(&program)?;
        let links = process.link_dirs(&program)?;
        let scripts = process.named_files(spec, cwd);
        let toolchains = toolchain_homes();
        let grants = snapshot
            .writable
            .iter()
            .map(|path| (path, Access::WRITE | Access::EXEC))
            .chain(snapshot.readable.iter().map(|path| (path, Access::READ)))
            .chain(std::iter::once((&target, Access::EXEC)))
            .chain(links.iter().map(|path| (path, Access::READ)))
            .chain(scripts.iter().map(|path| (path, Access::READ)))
            // The host toolchains the session's own commands build with.
            .chain(
                toolchains
                    .iter()
                    .map(|(_, path)| (path, Access::WRITE | Access::EXEC)),
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
        let tmp = root.join("tmp");
        async_fs::create_dir_all(&tmp).await?;
        let environment = base_environment(&root, &tmp, &toolchains, process)?;
        let config = SandboxConfig::builder()
            .network(self.network.clone())
            .working_dir(root)
            .grants(grants)
            .security(SecurityConfig::interactive())
            // The session's own commands run their build outputs.
            .executable_working_dir(true)
            .priority(self.priority);
        #[cfg(target_os = "linux")]
        let config = config.supervised_spawn(true);
        let sandbox = Sandbox::with_config_and_executor(config.build(), DefaultExecutor)
            .await
            .map_err(io::Error::other)?;
        let program = program.into_os_string().into_string().map_err(|program| {
            io::Error::other(format!(
                "the program path is not UTF-8: {}",
                program.to_string_lossy()
            ))
        })?;
        let command = sandbox
            .command(program)
            .args(&spec.args)
            .envs(environment)
            .envs(&spec.env)
            .current_dir(cwd)
            .stdin(if process.pipes_stdin() {
                StdioConfig::Piped
            } else {
                StdioConfig::Null
            })
            .stdout(StdioConfig::Piped)
            .stderr(StdioConfig::Piped);
        #[cfg(target_os = "linux")]
        let child = command
            .spawn_supervised(heel::supervisor::Foreground {
                reap_timeout: SUPERVISED_REAP_TIMEOUT,
            })
            .await;
        #[cfg(not(target_os = "linux"))]
        let child = command.spawn().await;
        let child = child.map_err(io::Error::other)?;
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

    /// The files the agent's own command line names (an interpreter's
    /// script), which it reads: that command is the user's configuration. A
    /// terminal command's arguments come from the agent and grant nothing.
    fn named_files(&self, spec: &SpawnSpec, cwd: &Path) -> Vec<PathBuf> {
        match self {
            Self::Agent { .. } => spec
                .args
                .iter()
                .map(|arg| cwd.join(arg))
                .filter(|path| path.is_file())
                .collect(),
            Self::Terminal => Vec::new(),
        }
    }

    /// The directories holding the links `program` starts through, which
    /// the sandbox reads to follow them: a grant cannot name a link itself.
    /// The agent's command is the user's configuration; a terminal command's
    /// comes from the agent and opens nothing beyond its target, so one
    /// started through a link the sandbox cannot read fails to start.
    fn link_dirs(&self, program: &Path) -> io::Result<Vec<PathBuf>> {
        match self {
            Self::Agent { .. } => link_dirs(program),
            Self::Terminal => Ok(Vec::new()),
        }
    }
}

/// The resolved directory of every link on the way from `program` to the
/// file it names. `program` resolves, so the chain ends.
fn link_dirs(program: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let mut path = program.to_path_buf();
    while std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
        let dir = path
            .parent()
            .expect("a link is an entry of a directory")
            .to_path_buf();
        let target = std::fs::read_link(&path)?;
        dirs.push(canonical(&dir)?);
        path = dir.join(target);
    }
    Ok(dirs)
}

/// The environment under the process's own: the host `PATH`, the host
/// toolchain homes and a temporary directory inside the sandbox, as the
/// session's terminal commands get them. The agent finds its own config and
/// login through the user's real home; its commands keep the sandbox root
/// (`root`, resolved) as home, like every command of the session.
fn base_environment(
    root: &Path,
    tmp: &Path,
    toolchains: &[(&'static str, PathBuf)],
    process: &Process,
) -> io::Result<Vec<(&'static str, String)>> {
    let homes: Vec<(&'static str, OsString)> = match process {
        Process::Agent { .. } => vec![("HOME", canonical(&real_home()?)?.into_os_string())],
        Process::Terminal => vec![
            ("HOME", root.as_os_str().to_owned()),
            ("XDG_CACHE_HOME", root.join(".cache").into_os_string()),
            ("XDG_CONFIG_HOME", root.join(".config").into_os_string()),
        ],
    };
    std::env::var_os("PATH")
        .map(|path| ("PATH", path))
        .into_iter()
        .chain(
            ["TMPDIR", "TMP", "TEMP"]
                .into_iter()
                .map(|name| (name, tmp.as_os_str().to_owned())),
        )
        // Cargo and rustup find the homes the grants cover, not ones under
        // a remapped `HOME`.
        .chain(
            toolchains
                .iter()
                .map(|(name, path)| (*name, path.as_os_str().to_owned())),
        )
        .chain(homes)
        .map(|(name, value)| {
            value
                .into_string()
                .map(|value| (name, value))
                .map_err(|value| {
                    io::Error::other(format!("{name} is not UTF-8: {}", value.to_string_lossy()))
                })
        })
        .collect()
}

/// The user's home, which the daemon runs as.
fn real_home() -> io::Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| io::Error::other("the user's home directory is unknown"))
}

/// The host toolchain homes that exist, by the variable that names each:
/// `CARGO_HOME` and `RUSTUP_HOME`, else `~/.cargo` and `~/.rustup`, as
/// aither-sandbox grants and sets them for the session's own commands.
fn toolchain_homes() -> Vec<(&'static str, PathBuf)> {
    let home = std::env::home_dir();
    [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")]
        .into_iter()
        .filter_map(|(variable, default)| {
            std::env::var_os(variable)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
                .map(|path| (variable, path))
        })
        .filter(|(_, path)| path.is_dir())
        .collect()
}

/// `path` with every link resolved; a path that cannot be resolved is an
/// error naming it.
fn canonical(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot resolve {}: {error}", path.display()),
        )
    })
}

fn resolved(path: &Path) -> io::Result<ResolvedPath> {
    ResolvedPath::new(canonical(path)?).map_err(io::Error::other)
}

/// The file a command name starts, as the host's `PATH` finds it from `cwd`,
/// where the sandbox's own search path would not. A link stays a link.
fn resolve_program(program: &str, cwd: &Path) -> io::Result<PathBuf> {
    which::which_in(program, std::env::var_os("PATH"), cwd).map_err(|error| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot find the program `{program}`: {error}"),
        )
    })
}

/// Starts ACP processes in the sandbox of the session installed in `cell`.
#[derive(Debug, Clone)]
pub struct SandboxExecutor<N> {
    cell: Arc<ArcSwap<SessionSandbox<N>>>,
    config: Arc<ArcSwap<MayConfig>>,
}

/// A process started in the sandbox.
#[derive(Debug)]
pub struct SandboxProcess {
    /// Ends the whole tree under heel's supervisor.
    #[cfg(target_os = "linux")]
    tree: heel::supervisor::TreeControl,
    /// The process group the process leads.
    #[cfg(not(target_os = "linux"))]
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
    fn reap<N: NetworkPolicy>(mut child: AcpChild, sandbox: Sandbox<N>) -> Self {
        #[cfg(target_os = "linux")]
        let tree = child.tree_control();
        #[cfg(not(target_os = "linux"))]
        let pid = child.id();
        let exit = Arc::new(Exit::default());
        let published = exit.clone();
        blocking::unblock(move || {
            let _sandbox = sandbox;
            let status = futures_lite::future::block_on(exit_status(&mut child));
            let _ = published.status.set(status);
            published.changed.notify(usize::MAX);
        })
        .detach();
        Self {
            #[cfg(target_os = "linux")]
            tree,
            #[cfg(not(target_os = "linux"))]
            pid,
            exit,
        }
    }
}

/// The exit status of the command under the supervisor.
#[cfg(target_os = "linux")]
async fn exit_status(child: &mut AcpChild) -> Result<ExitStatus, String> {
    child
        .wait()
        .await
        .map(|exit| exit.status)
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
async fn exit_status(child: &mut AcpChild) -> Result<ExitStatus, String> {
    child.wait().await.map_err(|error| error.to_string())
}

impl ProcessControl for SandboxProcess {
    #[cfg(target_os = "linux")]
    fn kill(&self) {
        // The supervisor freezes, kills and reaps the whole tree; a tree
        // already gone reports nothing left to kill.
        let tree = self.tree.clone();
        blocking::unblock(move || {
            if let Err(error) = futures_lite::future::block_on(tree.kill()) {
                tracing::warn!(%error, "an ACP process tree outlived its kill");
            }
        })
        .detach();
    }

    #[cfg(not(target_os = "linux"))]
    fn kill(&self) {
        // The process leads its own group, which keeps the descendants it
        // orphaned; a group that is gone is not an error.
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

impl<N> SandboxExecutor<N> {
    /// An executor starting processes in whatever sandbox `cell` holds when
    /// they start, giving an agent the grants `config` lists for its command
    /// when it starts.
    #[must_use]
    pub const fn new(
        cell: Arc<ArcSwap<SessionSandbox<N>>>,
        config: Arc<ArcSwap<MayConfig>>,
    ) -> Self {
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
        let home = canonical(&real_home()?)?;
        Ok(matching.flat_map(|agent| agent.grants_in(&home)).collect())
    }
}

impl<N: NetworkPolicy + Clone> Executor for SandboxExecutor<N> {
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
pub struct ApprovalPermissions<N> {
    cell: Arc<ArcSwap<SessionSandbox<N>>>,
}

impl<N> ApprovalPermissions<N> {
    /// A handler asking the approver of whatever sandbox `cell` holds.
    #[must_use]
    pub const fn new(cell: Arc<ArcSwap<SessionSandbox<N>>>) -> Self {
        Self { cell }
    }
}

impl<N: Send + Sync + 'static> PermissionHandler for ApprovalPermissions<N> {
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
        choose(&request.params.options, approved)
    }
}

/// The option answering one request: the user approved or refused this one
/// call, so a "once" option wins over an "always" one wherever the agent
/// lists it, and "always" is taken only when "once" is not offered.
fn choose(options: &[PermissionOption], approved: bool) -> RequestPermissionOutcome {
    let preference = if approved {
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
    preference
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
        .map_or(RequestPermissionOutcome::Cancelled, |option| {
            RequestPermissionOutcome::Selected {
                option_id: option.option_id.clone(),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configured agent command's script is read; a terminal command's
    /// argument naming the same file grants nothing.
    #[test]
    fn only_the_agent_command_names_files_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.py"), "").unwrap();
        let spec = SpawnSpec {
            program: "python3".to_string(),
            args: vec!["agent.py".to_string(), "--acp".to_string()],
            env: std::collections::BTreeMap::new(),
            cwd: dir.path().to_path_buf(),
        };
        let agent = Process::Agent { state: Vec::new() };
        assert_eq!(
            agent.named_files(&spec, dir.path()),
            vec![dir.path().join("agent.py")]
        );
        assert_eq!(
            Process::Terminal.named_files(&spec, dir.path()),
            Vec::<PathBuf>::new()
        );
    }

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption {
            option_id: id.to_string(),
            name: id.to_string(),
            kind,
            meta: None,
        }
    }

    fn selected(outcome: &RequestPermissionOutcome) -> Option<&str> {
        match outcome {
            RequestPermissionOutcome::Selected { option_id } => Some(option_id),
            RequestPermissionOutcome::Cancelled => None,
        }
    }

    #[test]
    fn an_answer_selects_the_once_option_even_when_always_comes_first() {
        let options = [
            option("allow-always", PermissionOptionKind::AllowAlways),
            option("reject-always", PermissionOptionKind::RejectAlways),
            option("allow-once", PermissionOptionKind::AllowOnce),
            option("reject-once", PermissionOptionKind::RejectOnce),
        ];
        assert_eq!(selected(&choose(&options, true)), Some("allow-once"));
        assert_eq!(selected(&choose(&options, false)), Some("reject-once"));
    }

    #[test]
    fn an_answer_selects_always_only_when_once_is_not_offered() {
        let options = [
            option("allow-always", PermissionOptionKind::AllowAlways),
            option("reject-always", PermissionOptionKind::RejectAlways),
        ];
        assert_eq!(selected(&choose(&options, true)), Some("allow-always"));
        assert_eq!(selected(&choose(&options, false)), Some("reject-always"));
        let allow_only = [option("allow-once", PermissionOptionKind::AllowOnce)];
        assert_eq!(selected(&choose(&allow_only, false)), None);
    }
}
