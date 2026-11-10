//! Where connected ACP agents run: inside the session's sandbox.
//!
//! acpsub starts two kinds of process, the agent itself and each command the
//! agent runs through ACP `terminal/*`. Both start through
//! [`SandboxExecutor`] on the [`Sandbox`] of the session's shell, so they run
//! with the session's live grants, environment and network policy exactly as
//! its own commands do. The agent's permission requests go through
//! [`ApprovalPermissions`] to the session's [`PermissionApprover`]; nothing
//! is answered without it.
//!
//! A [`SessionSandbox`] is the boundary of one session. The executor and the
//! permission handler read it through a shared cell, so an agent rebuilt for
//! the session installs its sandbox for every process started afterwards. A
//! running agent keeps the grants it started with, as a running terminal
//! command does.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::Arc;
use std::task::{Context, Poll};

use acpsub::{
    AgentProcess, Executor, PermissionHandler, PermissionRequest, ProcessControl,
    SpawnSpec as AcpSpawn, TerminalProcess,
};
use aither_acp::{PermissionOption, PermissionOptionKind, RequestPermissionOutcome};
use aither_sandbox::{
    Backend, ChildExit, CommandFifo, Grants, HostLaunch, JobBroker, SandboxMode, SandboxedChild,
    SpawnGrant, SpawnSpec, TreeLifetime,
};
use arc_swap::ArcSwap;
use askama::Template;
use async_channel::{Receiver, Sender};
use executor_core::{DefaultExecutor, Executor as _};
use futures_lite::Stream;
use futures_lite::io::{AsyncRead, AsyncWrite};
use heel::{Access, Grant, HeelBinary, ResolvedPath};

use crate::agent::PermissionApprover;
use crate::config::MayConfig;

/// The boundary of one session's ACP processes.
#[derive(Clone)]
pub struct SessionSandbox {
    /// The sandbox the session's shell spawns in.
    sandbox: aither_sandbox::Sandbox,
    /// Overrides the working directory a process asks for, when the caller
    /// has none of its own to give (a probe starts in the daemon's).
    launch_dir: Option<PathBuf>,
    /// Asks the user about the agent's permission requests; without it every
    /// request is refused.
    approver: Option<PermissionApprover>,
    /// The machine-wide jobserver pool the agent's commands draw jobs from;
    /// a scratch sandbox has none.
    jobs: Option<JobBroker>,
}

impl std::fmt::Debug for SessionSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSandbox")
            .field("root", &self.sandbox.working_dir())
            .field("grants", &self.sandbox.grants().snapshot())
            .finish_non_exhaustive()
    }
}

impl SessionSandbox {
    /// The boundary of the session whose shell spawns in `sandbox`.
    #[must_use]
    pub const fn for_session(
        sandbox: aither_sandbox::Sandbox,
        approver: Option<PermissionApprover>,
        jobs: JobBroker,
    ) -> Self {
        Self {
            sandbox,
            launch_dir: None,
            approver,
            jobs: Some(jobs),
        }
    }

    /// A sandbox granting nothing but `root`, which every process starts in,
    /// with no network and answering no permission request: a command under
    /// probe or trial belongs to no session. Its supervisor is run by `heel`.
    ///
    /// # Errors
    /// Fails when a host toolchain home cannot be resolved.
    pub fn scratch(root: &Path, heel: HeelBinary) -> io::Result<Self> {
        let toolchains = toolchain_homes();
        let tmp = root.join("tmp");
        let environment = ["TMPDIR", "TMP", "TEMP"]
            .into_iter()
            .map(|name| (name, tmp.as_os_str()))
            .chain(
                toolchains
                    .iter()
                    .map(|(name, path)| (*name, path.as_os_str())),
            )
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| (name.to_string(), value.to_string()))
                    .ok_or_else(|| io::Error::other(format!("{name} is not UTF-8")))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let toolchain_grants = toolchains
            .iter()
            .map(|(_, path)| {
                resolved(path).map(|path| Grant::new(path, Access::WRITE | Access::EXEC))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let grants = Grants::new();
        grants.add_writable(root);
        // A scratch process only ever runs a program, never a script, so the
        // backend's shell is never started.
        let sandbox = aither_sandbox::Sandbox::new(
            Backend::Host {
                heel,
                launch: HostLaunch::new("bash"),
            },
            DefaultExecutor,
            root,
        )
        .with_grants(grants)
        .with_environment(environment)
        .with_fixed_grants(toolchain_grants);
        Ok(Self {
            sandbox,
            launch_dir: Some(root.to_path_buf()),
            approver: None,
            jobs: None,
        })
    }
}

/// Which process is starting.
enum Process {
    /// The agent, with the directories its configuration grants it.
    Agent { state: Vec<PathBuf> },
    /// A command the agent runs.
    Terminal,
}

impl SessionSandbox {
    /// Starts `spec` as `process` in the sandbox with the grants of this
    /// moment.
    async fn launch(
        &self,
        spec: &AcpSpawn,
        process: &Process,
    ) -> io::Result<(SandboxedChild, Option<CommandFifo>)> {
        let cwd = self.launch_dir.as_deref().unwrap_or(&spec.cwd);
        let tmp = self.sandbox.working_dir().join("tmp");
        async_fs::create_dir_all(&tmp).await?;
        let mut env: Vec<(String, String)> = Vec::new();
        let mut grants: Vec<SpawnGrant> = Vec::new();
        let program = match process {
            Process::Agent { state } => {
                // The agent runs on the host's tools and finds its own config
                // and login through the user's real home; its commands keep
                // the session's.
                let search_path = std::env::var_os("PATH").unwrap_or_default();
                let program = resolve_program(&spec.program, &search_path, cwd)?;
                // The program starts by the path `PATH` gives, so a link
                // starts under its own name (`cargo` is a link to `rustup`,
                // which dispatches on it). A grant never names a link, so
                // its target is what the sandbox may execute.
                let target = canonical(&program)?;
                // The files the agent's own command line names (an
                // interpreter's script), which it reads: that command is the
                // user's configuration.
                let named = spec
                    .args
                    .iter()
                    .map(|arg| cwd.join(arg))
                    .filter(|path| path.is_file());
                // The directories holding the links the program starts
                // through, which the sandbox reads to follow them.
                let links = link_dirs(&program)?;
                let host_grants = std::iter::once((target, Access::EXEC))
                    .chain(links.into_iter().map(|path| (path, Access::READ)))
                    .chain(named.map(|path| (path, Access::READ)))
                    .chain(
                        state
                            .iter()
                            .cloned()
                            .map(|path| (path, Access::WRITE | Access::EXEC)),
                    );
                for (path, access) in host_grants {
                    grants.push(SpawnGrant::Host(Grant::new(resolved(&path)?, access)));
                }
                env.push(("PATH".to_string(), os_text("PATH", search_path)?));
                env.push((
                    "HOME".to_string(),
                    os_text("HOME", canonical(&real_home()?)?.into_os_string())?,
                ));
                os_text("the program path", program.into_os_string())?
            }
            Process::Terminal => spec.program.clone(),
        };
        // A command the agent runs draws its jobs from the machine-wide
        // pool through a fifo of its own; the agent itself starts none.
        let job_fifo = match (&self.jobs, process) {
            (Some(jobs), Process::Terminal) => {
                Some(jobs.open_fifo().await.map_err(io::Error::other)?)
            }
            _ => None,
        };
        if let Some(fifo) = &job_fifo {
            // The command writes the tokens it is done with back to its fifo.
            grants.push(SpawnGrant::Host(Grant::new(
                resolved(fifo.path())?,
                Access::WRITE,
            )));
            env.extend(fifo.environment());
        }
        env.extend(spec.env.clone());
        let stdio = aither_sandbox::Stdio {
            stdin: match process {
                Process::Agent { .. } => aither_sandbox::Pipe::Piped,
                Process::Terminal => aither_sandbox::Pipe::Null,
            },
            ..aither_sandbox::Stdio::default()
        };
        let child = self
            .sandbox
            .spawn(
                SpawnSpec::new(program)
                    .args(&spec.args)
                    .env(env)
                    .grants(grants)
                    .cwd(cwd)
                    .stdio(stdio)
                    .lifetime(TreeLifetime::Foreground),
            )
            .await
            .map_err(io::Error::other)?;
        Ok((child, job_fifo))
    }
}

/// `value` as text; the variable or path `what` names is not UTF-8
/// otherwise.
fn os_text(what: &str, value: OsString) -> io::Result<String> {
    value.into_string().map_err(|value| {
        io::Error::other(format!("{what} is not UTF-8: {}", value.to_string_lossy()))
    })
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
fn resolve_program(program: &str, search_path: &OsStr, cwd: &Path) -> io::Result<PathBuf> {
    which::which_in(program, Some(search_path), cwd).map_err(|error| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot find the program `{program}`: {error}"),
        )
    })
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
    child: Arc<SandboxedChild>,
}

impl SandboxProcess {
    /// Holds `child` and the pool `fifo` of its commands until the whole
    /// tree has exited; the broker takes the tokens back once it has.
    fn start(child: SandboxedChild, fifo: Option<CommandFifo>) -> Self {
        let child = Arc::new(child);
        let held = child.clone();
        DefaultExecutor
            .spawn(async move {
                let _fifo = fifo;
                held.wait().await;
            })
            .detach();
        Self { child }
    }
}

impl ProcessControl for SandboxProcess {
    fn kill(&self) {
        // The whole tree ends, descendants the process orphaned included; a
        // tree already gone is not an error.
        let child = self.child.clone();
        DefaultExecutor
            .spawn(async move {
                if let Err(error) = child.kill().await {
                    tracing::warn!(%error, "an ACP process tree outlived its kill");
                }
            })
            .detach();
    }

    async fn wait(&self) -> io::Result<ExitStatus> {
        match self.child.wait().await {
            ChildExit::Exited(status) => Ok(status),
            ChildExit::Killed => Err(io::Error::other("the process was killed")),
            ChildExit::Failed(error) => Err(io::Error::other(error)),
        }
    }
}

/// A process's output as the byte stream acpsub reads.
#[derive(Debug)]
pub struct ChunkReader {
    /// Pinned on the heap: the channel's stream is not `Unpin`, and the
    /// reader must be.
    chunks: Pin<Box<Receiver<Vec<u8>>>>,
    pending: Vec<u8>,
    offset: usize,
}

impl ChunkReader {
    fn new(chunks: Receiver<Vec<u8>>) -> Self {
        Self {
            chunks: Box::pin(chunks),
            pending: Vec::new(),
            offset: 0,
        }
    }
}

impl AsyncRead for ChunkReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            if this.offset < this.pending.len() {
                let count = buf.len().min(this.pending.len() - this.offset);
                buf[..count].copy_from_slice(&this.pending[this.offset..this.offset + count]);
                this.offset += count;
                return Poll::Ready(Ok(count));
            }
            match this.chunks.as_mut().poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    this.pending = chunk;
                    this.offset = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(0)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// A process's input as the byte stream acpsub writes; closing it, or
/// dropping it, closes the process's standard input.
#[derive(Debug)]
pub struct ChunkWriter {
    chunks: Sender<Vec<u8>>,
}

impl AsyncWrite for ChunkWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // The channel is unbounded, so a send never waits.
        match self.chunks.try_send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.chunks.close();
        Poll::Ready(Ok(()))
    }
}

fn piped<T>(stream: Option<T>, name: &str) -> io::Result<T> {
    stream.ok_or_else(|| io::Error::other(format!("{name} was not piped")))
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
    fn state_dirs(&self, spec: &AcpSpawn) -> io::Result<Vec<PathBuf>> {
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

impl Executor for SandboxExecutor {
    type Process = SandboxProcess;
    type Stdin = ChunkWriter;
    type Stdout = ChunkReader;
    type Stderr = ChunkReader;

    async fn spawn_agent(&self, spec: AcpSpawn) -> io::Result<AgentProcess<Self>> {
        let process = Process::Agent {
            state: self.state_dirs(&spec)?,
        };
        let (mut child, job_fifo) = self.cell.load_full().launch(&spec, &process).await?;
        let stdin = ChunkWriter {
            chunks: piped(child.take_stdin(), "stdin")?,
        };
        let stdout = ChunkReader::new(piped(child.take_stdout(), "stdout")?);
        let stderr = ChunkReader::new(piped(child.take_stderr(), "stderr")?);
        Ok(AgentProcess {
            stdin,
            stdout,
            stderr,
            control: SandboxProcess::start(child, job_fifo),
        })
    }

    async fn spawn_terminal(&self, spec: AcpSpawn) -> io::Result<TerminalProcess<Self>> {
        let (mut child, job_fifo) = self
            .cell
            .load_full()
            .launch(&spec, &Process::Terminal)
            .await?;
        let stdout = ChunkReader::new(piped(child.take_stdout(), "stdout")?);
        let stderr = ChunkReader::new(piped(child.take_stderr(), "stderr")?);
        Ok(TerminalProcess {
            stdout,
            stderr,
            control: SandboxProcess::start(child, job_fifo),
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
                    .request_and_wait(SandboxMode::Sandboxed, script)
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

/// The sandbox of a session rooted in `root` that reads the fixtures, writes
/// its root and runs the host's programs.
#[cfg(test)]
pub(crate) async fn test_sandbox(root: &Path) -> aither_sandbox::Sandbox {
    let heel = crate::tools::test_heel().await;
    let grants = Grants::new();
    grants.add_readable(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"));
    grants.add_writable(root);
    std::fs::create_dir_all(root).expect("session dir");
    let tmp = root.join("tmp");
    let path = std::env::var("PATH").expect("PATH is set and UTF-8");
    aither_sandbox::Sandbox::new(
        Backend::Host {
            heel,
            launch: HostLaunch::new("bash"),
        },
        DefaultExecutor,
        root,
    )
    .with_grants(grants)
    .with_environment(vec![
        ("PATH".to_string(), path),
        ("TMPDIR".to_string(), tmp.to_string_lossy().into_owned()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_agent_spec(cwd: &Path) -> AcpSpawn {
        AcpSpawn {
            program: "python3".to_string(),
            args: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_acp_agent.py")
                    .to_string_lossy()
                    .into_owned(),
            ],
            env: std::collections::BTreeMap::new(),
            cwd: cwd.to_path_buf(),
        }
    }

    /// How a test ends the fake agent.
    #[derive(Debug, Clone, Copy)]
    enum Ending {
        /// Its stdin closes, so it exits by itself.
        StdinCloses,
        /// It is killed while it still reads stdin, as acpsub ends an agent
        /// whose handshake failed.
        Killed,
    }

    /// Starts the fake agent, ends it as `ending` says, and returns what the
    /// executor reports for its exit.
    async fn exit_status_after(
        sandbox: SessionSandbox,
        cwd: &Path,
        ending: Ending,
    ) -> Result<ExitStatus, String> {
        // The sandbox spawns on the daemon's global executor.
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let executor = SandboxExecutor::new(
            Arc::new(ArcSwap::from_pointee(sandbox)),
            Arc::<ArcSwap<MayConfig>>::default(),
        );
        let agent = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            executor.spawn_agent(fake_agent_spec(cwd)),
        )
        .await
        .map_err(|_| "the agent did not start within 60s".to_string())?
        .map_err(|error| format!("the agent did not start: {error}"))?;
        let AgentProcess {
            stdin,
            stdout: _stdout,
            stderr: _stderr,
            control,
        } = agent;
        let _stdin = match ending {
            Ending::StdinCloses => {
                drop(stdin);
                None
            }
            Ending::Killed => {
                control.kill();
                Some(stdin)
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), control.wait())
            .await
            .map_err(|_| format!("the agent did not exit within 30s ({ending:?})"))?
            .map_err(|error| format!("waiting for the agent failed: {error} ({error:?})"))
    }

    /// A pool of `size` tokens in a directory of its own.
    async fn pool(size: usize) -> (tempfile::TempDir, crate::jobserver::JobServer) {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let dir = tempfile::tempdir().expect("pool dir");
        let server = crate::jobserver::JobServer::start(
            dir.path(),
            std::num::NonZeroUsize::new(size).expect("a pool has tokens"),
        )
        .await
        .expect("pool starts");
        (dir, server)
    }

    async fn session_sandbox(root: &Path, server: &crate::jobserver::JobServer) -> SessionSandbox {
        SessionSandbox::for_session(
            test_sandbox(root).await,
            None,
            server.broker().expect("pool broker"),
        )
    }

    /// A command the agent runs through ACP holds the pool's tokens while it
    /// runs and gives them back when it ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_acp_terminal_command_draws_from_the_shared_pool() {
        const SIZE: usize = 3;
        let (_pool_dir, server) = pool(SIZE).await;
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("session");
        let sandbox = session_sandbox(&root, &server).await;
        let report = root.join("tmp/held");
        let executor = SandboxExecutor::new(
            Arc::new(ArcSwap::from_pointee(sandbox)),
            Arc::<ArcSwap<MayConfig>>::default(),
        );
        assert_eq!(server.tokens_in_pool(), SIZE);
        let mut spec = fake_agent_spec(&root);
        spec.args = vec![
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/job_probe.py")
                .to_string_lossy()
                .into_owned(),
            report.to_string_lossy().into_owned(),
        ];
        let command = executor.spawn_terminal(spec).await.expect("command starts");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let held = loop {
            if let Ok(held) = std::fs::read_to_string(&report) {
                break held;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the command never reported its tokens"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(held, SIZE.to_string(), "the command drew every token");
        assert_eq!(server.tokens_in_pool(), 0, "they left the shared pool");

        command.control.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while server.tokens_in_pool() != SIZE {
            assert!(
                std::time::Instant::now() < deadline,
                "the tokens did not return to the pool"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_scratch_sandbox_agent_reports_its_exit_status() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sandbox = SessionSandbox::scratch(dir.path(), crate::tools::test_heel().await)
            .expect("the scratch sandbox");
        let status = exit_status_after(sandbox, dir.path(), Ending::StdinCloses)
            .await
            .unwrap_or_else(|error| panic!("scratch sandbox: {error}"));
        assert!(status.success(), "scratch sandbox exit status: {status:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_killed_scratch_sandbox_agent_reports_its_exit_status() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sandbox = SessionSandbox::scratch(dir.path(), crate::tools::test_heel().await)
            .expect("the scratch sandbox");
        let status = exit_status_after(sandbox, dir.path(), Ending::Killed)
            .await
            .unwrap_or_else(|error| panic!("killed in the scratch sandbox: {error}"));
        assert!(
            !status.success(),
            "a killed agent did not succeed: {status:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_killed_session_sandbox_agent_reports_its_exit_status() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("session");
        let (_pool_dir, server) = pool(1).await;
        let sandbox = session_sandbox(&root, &server).await;
        let status = exit_status_after(sandbox, &root, Ending::Killed)
            .await
            .unwrap_or_else(|error| panic!("killed in a session sandbox: {error}"));
        assert!(
            !status.success(),
            "a killed agent did not succeed: {status:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_sandbox_agent_reports_its_exit_status() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("session");
        let (_pool_dir, server) = pool(1).await;
        let sandbox = session_sandbox(&root, &server).await;
        let status = exit_status_after(sandbox, &root, Ending::StdinCloses)
            .await
            .unwrap_or_else(|error| panic!("session sandbox: {error}"));
        assert!(status.success(), "session sandbox exit status: {status:?}");
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
