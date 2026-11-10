//! ACP agents connected with `/connect`, run as subagent types.
//!
//! The daemon owns one [`acpsub::Host`] whose configuration is built from the
//! live [`MayConfig`] on every load. [`AcpSubagent`] adapts it to aither's
//! the `subagent` command's external agents: [`AcpSession`] is a
//! [`ChildAgent`], so a connected agent is launched, continued, tracked and
//! cancelled by the `subagent` command like a built-in type.
//!
//! acpsub runs on Tokio. This module is the only place May touches Tokio for
//! it: every acpsub future is polled inside the daemon runtime's context.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use acpsub::{
    AgentConfig, Config, ConfigSource, Defaults, Executor, FileAccess, Host, PermissionHandler,
    PermissionPolicy, Services, Session, SessionSpec, TurnEnd, probe_with,
};
use aither_acp::StopReason;
use aither_agent::{AgentError, ChildAgent, ChildEvents, RunOutcome};
use aither_core::llm::tool::CancelSignal;
use arc_swap::ArcSwap;
use async_lock::{RwLock, RwLockReadGuardArc};
use futures_util::StreamExt as _;
use tokio::runtime::Handle;

use super::acp_sandbox::{ApprovalPermissions, SandboxExecutor, SessionSandbox};
use crate::config::{AcpAgentConfig, MayConfig};
use crate::session::MayHomePaths;
use crate::subagent::{ExternalAgent, ExternalAgents, ExternalStart};

/// An acpsub host whose agents and their commands run in a session sandbox
/// and whose permission requests go to the session's approver.
pub type SandboxedHost = Host<SandboxExecutor, ApprovalPermissions>;

/// A session of a [`SandboxedHost`].
pub type SandboxedSession = Session<SandboxExecutor, ApprovalPermissions>;

/// Whether a session's host is retired, which happens once, when the session
/// is deleted. Opening and closing an agent session hold it for reading, so
/// retiring waits for the ones in flight; after it nothing opens or closes
/// through the host, which has ended its agent sessions and given up its
/// storage itself.
type Lease = Arc<RwLock<bool>>;

/// A session's host and the cell holding the sandbox it starts processes in.
#[derive(Clone)]
struct SessionHost {
    host: SandboxedHost,
    sandbox: Arc<ArcSwap<SessionSandbox>>,
    lease: Lease,
}

/// The daemon's ACP hosts, one per session: a session's registry of resumable
/// agent sessions is its own, and so is the sandbox its processes start in.
pub struct AcpHosts {
    config: Arc<ArcSwap<MayConfig>>,
    layout: MayHomePaths,
    runtime: Handle,
    hosts: std::sync::Mutex<BTreeMap<String, SessionHost>>,
}

impl std::fmt::Debug for AcpHosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpHosts")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl AcpHosts {
    /// Hosts over May's live configuration, storing what acpsub keeps where
    /// `layout` says.
    #[must_use]
    pub const fn new(
        config: Arc<ArcSwap<MayConfig>>,
        layout: MayHomePaths,
        runtime: Handle,
    ) -> Self {
        Self {
            config,
            layout,
            runtime,
            hosts: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// The agents of `session_id`, started in `sandbox` from now on.
    ///
    /// # Errors
    /// Returns acpsub's error when the session's registry file exists but
    /// cannot be parsed.
    ///
    /// # Panics
    /// Panics if the host table was poisoned.
    pub fn agents(
        &self,
        session_id: &str,
        sandbox: SessionSandbox,
    ) -> Result<AcpAgents, acpsub::Error> {
        let session = self.session_host(session_id, sandbox)?;
        Ok(AcpAgents {
            host: session.host,
            runtime: self.runtime.clone(),
            connected: self.config.load().acp_agents.clone(),
            lease: session.lease,
        })
    }

    /// The host of `session_id`, created on first use, with `sandbox`
    /// installed for the processes it starts from now on.
    fn session_host(
        &self,
        session_id: &str,
        sandbox: SessionSandbox,
    ) -> Result<SessionHost, acpsub::Error> {
        let mut hosts = self.hosts.lock().expect("the host table is not poisoned");
        if let Some(session) = hosts.get(session_id) {
            session.sandbox.store(Arc::new(sandbox));
            return Ok(session.clone());
        }
        let cell = Arc::new(ArcSwap::from_pointee(sandbox));
        let source = LiveConfig {
            config: self.config.clone(),
            storage: self.layout.acp_session_dir(session_id),
        };
        let host = Host::from_config_with(
            source.build(),
            source,
            Services {
                executor: SandboxExecutor::new(cell.clone(), self.config.clone()),
                permissions: ApprovalPermissions::new(cell.clone()),
            },
        )?;
        let session = SessionHost {
            host,
            sandbox: cell,
            lease: Arc::new(RwLock::new(false)),
        };
        hosts.insert(session_id.to_string(), session.clone());
        drop(hosts);
        Ok(session)
    }

    /// Retires the host of a deleted session: its agent sessions end, the
    /// host leaves the table and what acpsub stored for the session is
    /// removed.
    ///
    /// # Errors
    /// Returns the error of removing the session's storage.
    ///
    /// # Panics
    /// Panics if the host table was poisoned.
    pub async fn remove_session(&self, session_id: &str) -> std::io::Result<()> {
        let host = self
            .hosts
            .lock()
            .expect("the host table is not poisoned")
            .remove(session_id);
        retire(&self.runtime, host).await;
        remove_storage(&self.layout.acp_session_dir(session_id)).await
    }

    /// [`remove_session`](Self::remove_session) for every session at once,
    /// including the storage of sessions this daemon never hosted.
    ///
    /// # Errors
    /// Returns the error of removing the sessions' storage.
    ///
    /// # Panics
    /// Panics if the host table was poisoned.
    pub async fn remove_all_sessions(&self) -> std::io::Result<()> {
        let hosts =
            std::mem::take(&mut *self.hosts.lock().expect("the host table is not poisoned"));
        retire(&self.runtime, hosts.into_values()).await;
        remove_storage(&self.layout.acp_sessions_dir()).await
    }
}

/// Retires `hosts`: once the agent sessions opening or closing through each
/// one are done, it is marked retired and every agent session still live on
/// it ends.
async fn retire(runtime: &Handle, hosts: impl IntoIterator<Item = SessionHost>) {
    // Ending a session kills its process tree and records the end in the
    // registry; the work is spread like any other the daemon fans out.
    let bound = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let retiring =
        futures_util::stream::iter(hosts).for_each_concurrent(bound, |session| async move {
            // Granted once the opens and closes in flight are done; every
            // later one sees the flag and leaves the host alone.
            *session.lease.write_arc().await = true;
            let state = session.host.state();
            // Out of the live table first, as acpsub's own close takes a session.
            let live = std::mem::take(
                &mut *state
                    .live
                    .lock()
                    .expect("acpsub's live table is not poisoned"),
            )
            .into_values();
            futures_util::stream::iter(live)
                .for_each_concurrent(bound, |sub| async move {
                    acpsub::state::end_session(state, &sub).await;
                })
                .await;
        });
    in_runtime(runtime, retiring).await;
}

/// Removes `dir` and everything under it; a session that never connected an
/// agent has none.
async fn remove_storage(dir: &Path) -> std::io::Result<()> {
    match async_fs::remove_dir_all(dir).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// The connected ACP agents and the host that runs them, as an agent build
/// receives them.
#[derive(Debug, Clone)]
pub struct AcpAgents {
    /// The session's acpsub host.
    pub host: SandboxedHost,
    /// The daemon runtime acpsub's futures are polled in.
    pub runtime: Handle,
    /// The connections to register, keyed by subagent type name.
    pub connected: BTreeMap<String, AcpAgentConfig>,
    lease: Lease,
}

impl AcpAgents {
    /// Every connected agent, for the `subagent` command to run under its
    /// name.
    ///
    /// # Errors
    /// Returns the config error of a name that cannot be a subagent name;
    /// nothing is overridden silently.
    pub fn externals(&self) -> Result<ExternalAgents, crate::config::ConfigError> {
        let mut agents = BTreeMap::new();
        for (name, connection) in &self.connected {
            crate::config::check_acp_agent_name(name)?;
            let backend = AcpSubagent {
                host: self.host.clone(),
                runtime: self.runtime.clone(),
                lease: self.lease.clone(),
                agent: name.clone(),
                model: connection.model.clone(),
                mode: connection.mode.clone(),
            };
            agents.insert(
                name.clone(),
                ExternalAgent {
                    description: connection.description.clone(),
                    backend,
                },
            );
        }
        Ok(ExternalAgents::new(agents))
    }
}

/// A host that knows exactly `agents`, with its storage under `storage`,
/// starting its processes in `sandbox`: the trial that proves a candidate
/// connection works before it is configured.
///
/// # Errors
/// Returns acpsub's error when the registry file exists but cannot be parsed.
pub fn trial_host(
    agents: &BTreeMap<String, AcpAgentConfig>,
    storage: &Path,
    sandbox: SessionSandbox,
) -> Result<SandboxedHost, acpsub::Error> {
    let source = FixedConfig(acpsub_config(agents, storage));
    let cell = Arc::new(ArcSwap::from_pointee(sandbox));
    Host::from_config_with(
        source.0.clone(),
        source,
        Services {
            executor: SandboxExecutor::new(cell.clone(), Arc::default()),
            permissions: ApprovalPermissions::new(cell),
        },
    )
}

/// What a connection looks like to acpsub. Its permission policy answers
/// nothing: May's handler decides every request.
fn acpsub_config(agents: &BTreeMap<String, AcpAgentConfig>, storage: &Path) -> Config {
    let mut defaults = Defaults::new(storage.join("transcripts"), storage.join("registry.json"));
    defaults.permission = PermissionPolicy::Deny;
    agents
        .iter()
        .fold(Config::new(defaults), |config, (name, agent)| {
            config.with_agent(name, launch_config(&agent.command, &agent.args, &agent.env))
        })
}

/// How acpsub starts `command`: in the sandbox, so acpsub serves it no file
/// access of its own; the agent reads and writes from its sandboxed process.
fn launch_config(command: &str, args: &[String], env: &BTreeMap<String, String>) -> AgentConfig {
    let mut launch = AgentConfig::new(command);
    launch.args = args.to_vec();
    launch.env.clone_from(env);
    launch.file_access = FileAccess::Off;
    launch
}

/// The host's configuration source: the daemon's live [`MayConfig`], read on
/// every load.
struct LiveConfig {
    config: Arc<ArcSwap<MayConfig>>,
    storage: PathBuf,
}

impl LiveConfig {
    fn build(&self) -> Config {
        acpsub_config(&self.config.load().acp_agents, &self.storage)
    }
}

impl ConfigSource for LiveConfig {
    fn load(&self) -> impl Future<Output = acpsub::Result<Config>> + Send {
        std::future::ready(Ok(self.build()))
    }
}

/// A configuration source that never changes.
struct FixedConfig(Config);

impl ConfigSource for FixedConfig {
    fn load(&self) -> impl Future<Output = acpsub::Result<Config>> + Send {
        std::future::ready(Ok(self.0.clone()))
    }
}

/// Why a connected agent could not run a subagent turn.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// The caller granted directories the agent cannot be trusted with.
    #[error(
        "the ACP agent `{agent}` writes only inside its own working directory, so it cannot be granted other directories to write or commit in"
    )]
    WorktreeGrant {
        /// The subagent type.
        agent: String,
    },
    /// The run's space holds several working copies, and the agent works in
    /// exactly one directory.
    #[error(
        "the ACP agent `{agent}` works in one directory, but its space holds {count} working copies"
    )]
    WorkingCopies {
        /// The subagent type.
        agent: String,
        /// How many working copies the space holds.
        count: usize,
    },
    /// The session this agent would run for was deleted.
    #[error("the session was deleted, so the ACP agent `{agent}` cannot start for it")]
    SessionDeleted {
        /// The subagent type.
        agent: String,
    },
    /// acpsub could not start the session or run the turn.
    #[error(transparent)]
    Acpsub(#[from] acpsub::Error),
    /// The agent ended the turn with a stop reason other than finishing it.
    #[error("the agent ended its turn without finishing it: {0:?}")]
    Stopped(StopReason),
    /// The turn errored while the agent stayed alive.
    #[error("the agent's turn failed: {0}")]
    Failed(String),
    /// The agent process exited.
    #[error("the agent process exited: {0}")]
    Exited(String),
    /// The agent asked for a permission nobody can answer.
    #[error(
        "the agent asked permission to run `{title}` (request {request_id}), which a subagent cannot be asked to grant"
    )]
    NeedsPermission {
        /// The request's id.
        request_id: String,
        /// The tool call the agent wanted to run.
        title: String,
    },
    /// The agent's provider is rate limited.
    #[error("the agent is rate limited until {resume_at}: {reason}")]
    RateLimited {
        /// When the provider says the quota resets.
        resume_at: jiff::Timestamp,
        /// The provider's own message.
        reason: String,
    },
}

/// The reply of a turn that ended `end`, or what went wrong with it.
fn finished(reply: String, end: TurnEnd) -> Result<String, AcpError> {
    match end {
        TurnEnd::Stopped(StopReason::EndTurn) => Ok(reply),
        TurnEnd::Stopped(reason) => Err(AcpError::Stopped(reason)),
        TurnEnd::Failed(message) => Err(AcpError::Failed(message)),
        TurnEnd::Exited(message) => Err(AcpError::Exited(message)),
        TurnEnd::NeedsPermission { request_id, title } => {
            Err(AcpError::NeedsPermission { request_id, title })
        }
        TurnEnd::RateLimited { resume_at, reason } => {
            Err(AcpError::RateLimited { resume_at, reason })
        }
    }
}

/// A connected agent, opening one acpsub session per `subagent start`.
#[derive(Debug)]
pub struct AcpSubagent {
    host: SandboxedHost,
    runtime: Handle,
    lease: Lease,
    agent: String,
    model: Option<String>,
    mode: Option<String>,
}

impl AcpSubagent {
    /// Opens a session of the agent for a call made with `start`.
    ///
    /// # Errors
    /// Returns why the agent cannot be started for it.
    pub async fn open(&self, start: ExternalStart<'_>) -> Result<AcpSession, AcpError> {
        if !start.worktrees.is_empty() || !start.commit_worktrees.is_empty() {
            return Err(AcpError::WorktreeGrant {
                agent: self.agent.clone(),
            });
        }
        // The agent works in the one working copy its space was prepared
        // with, else where it was launched.
        let working_dir = match start.working_copies {
            [] => start.working_dir,
            [copy] => copy.as_path(),
            copies => {
                return Err(AcpError::WorkingCopies {
                    agent: self.agent.clone(),
                    count: copies.len(),
                });
            }
        };
        // Held while the agent starts, so a retiring host ends it too.
        let retired = self.lease.read_arc().await;
        if *retired {
            return Err(AcpError::SessionDeleted {
                agent: self.agent.clone(),
            });
        }
        let mut spec = SessionSpec::new(&self.agent, working_dir);
        spec.model.clone_from(&self.model);
        spec.mode.clone_from(&self.mode);
        spec.permission = Some(PermissionPolicy::Deny);
        let session = in_runtime(&self.runtime, self.host.spawn(spec)).await?;
        drop(retired);
        Ok(AcpSession {
            agent: self.agent.clone(),
            session: Some(session),
            runtime: self.runtime.clone(),
            lease: self.lease.clone(),
        })
    }
}

/// One conversation with a connected agent.
#[derive(Debug)]
pub struct AcpSession {
    /// The agent's name.
    agent: String,
    /// `None` once [`AcpSession::close`] took it.
    session: Option<SandboxedSession>,
    runtime: Handle,
    lease: Lease,
}

impl AcpSession {
    const fn session(&self) -> &SandboxedSession {
        self.session
            .as_ref()
            .expect("a session is open until it is closed, which consumes it")
    }
}

/// What a turn waits for.
enum Turn {
    Ended(Result<String, AcpError>),
    Cancelled,
}

impl ChildAgent for AcpSession {
    /// Runs one turn of the agent. The agent reports no events of its own
    /// to the parent's stream.
    async fn run(
        &mut self,
        prompt: &str,
        _events: &ChildEvents,
        cancel: CancelSignal,
    ) -> RunOutcome {
        let turn =
            futures_lite::future::or(async { Turn::Ended(self.prompt(prompt).await) }, async {
                cancel.cancelled().await;
                Turn::Cancelled
            })
            .await;
        match turn {
            Turn::Ended(Ok(final_text)) => RunOutcome::Completed { final_text },
            Turn::Ended(Err(error)) => RunOutcome::Failed {
                error: AgentError::ToolExecution {
                    name: self.agent.clone(),
                    error: error.to_string(),
                },
            },
            Turn::Cancelled => {
                self.cancel().await;
                RunOutcome::Cancelled
            }
        }
    }
}

impl AcpSession {
    /// Runs a turn on `prompt` and returns the agent's reply.
    ///
    /// # Errors
    /// Returns how the turn went wrong.
    pub async fn prompt(&self, prompt: &str) -> Result<String, AcpError> {
        let outcome = in_runtime(&self.runtime, self.session().prompt(prompt)).await?;
        finished(outcome.reply, outcome.end)
    }

    /// Asks the agent to stop its turn.
    pub async fn cancel(&self) {
        // A turn that ended before the cancel arrived is not a failure.
        if let Err(error) = in_runtime(&self.runtime, self.session().cancel()).await
            && !matches!(error, acpsub::Error::NotRunning { .. })
        {
            tracing::warn!(%error, "cancelling the ACP agent's turn failed");
        }
    }

    /// Ends the agent's session.
    pub async fn close(mut self) {
        let session = self
            .session
            .take()
            .expect("a session is open until it is closed, which consumes it");
        let lease = self.lease.read_arc().await;
        close_leased(&self.runtime, session, &lease).await;
    }

    /// The model the agent runs, when it says.
    #[must_use]
    pub fn model(&self) -> Option<String> {
        self.session().model().map(|model| {
            model
                .as_str()
                .map_or_else(|| model.to_string(), str::to_string)
        })
    }
}

impl Drop for AcpSession {
    /// A session dropped without [`AcpSession::close`] still ends: its
    /// agent process must not outlive it.
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        // Taken here, before the close is spawned, so a host retiring from
        // now on waits for the close. While one retires no read is granted;
        // it ends this session itself.
        let Some(lease) = self.lease.try_read_arc() else {
            return;
        };
        let runtime = self.runtime.clone();
        self.runtime.spawn(async move {
            close_leased(&runtime, session, &lease).await;
        });
    }
}

/// Closes `session` unless its host retired, which ended it already.
async fn close_leased(
    runtime: &Handle,
    session: SandboxedSession,
    retired: &RwLockReadGuardArc<bool>,
) {
    if !**retired {
        close_session(runtime, session).await;
    }
}

async fn close_session<E: Executor, P: PermissionHandler>(
    runtime: &Handle,
    session: Session<E, P>,
) {
    if let Err(error) = in_runtime(runtime, session.close()).await {
        tracing::warn!(%error, "closing the ACP agent's session failed");
    }
}

/// Polls `future` inside `runtime`'s context, whatever executor drives the
/// caller: acpsub spawns tasks and arms timers while it runs.
fn in_runtime<F: Future>(runtime: &Handle, future: F) -> InRuntime<'_, F> {
    InRuntime {
        runtime,
        future: Box::pin(future),
    }
}

struct InRuntime<'a, F> {
    runtime: &'a Handle,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for InRuntime<'_, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _context = self.runtime.enter();
        self.future.as_mut().poll(cx)
    }
}

/// Why a candidate connection failed its trial session.
#[derive(Debug, thiserror::Error)]
pub enum TrialError {
    /// The scratch directory could not be prepared.
    #[error("cannot prepare the trial directory: {0}")]
    Scratch(#[from] std::io::Error),
    /// The agent refused or failed the session, including the model and mode
    /// checks.
    #[error(transparent)]
    Acpsub(#[from] acpsub::Error),
    /// The agent stopped answering the handshake; its process was killed.
    #[error("the agent did not open a trial session within {0:?}")]
    TimedOut(std::time::Duration),
}

/// Opens a session with `agent` in a fresh temporary directory and closes it,
/// proving the connection works before it is configured.
///
/// The session runs on a host of its own that knows only `agent`, so nothing
/// is registered or stored under the May home. Its process starts in a
/// sandbox that grants that directory and nothing else.
///
/// # Errors
/// Returns [`TrialError`]; [`acpsub::Error::ModelRequired`] and
/// [`acpsub::Error::ModeRequired`] carry the ids the agent advertises.
/// [`TrialError::TimedOut`] follows a handshake still unfinished after
/// `limit`, with the agent's process tree killed.
pub async fn try_connection(
    runtime: &Handle,
    name: &str,
    agent: &AcpAgentConfig,
    limit: std::time::Duration,
    heel: heel::HeelBinary,
) -> Result<(), TrialError> {
    let scratch = blocking::unblock(tempfile::tempdir).await?;
    let work = scratch.path().join("work");
    async_fs::create_dir_all(&work).await?;
    let host = trial_host(
        &BTreeMap::from([(name.to_string(), agent.clone())]),
        &scratch.path().join("acp"),
        SessionSandbox::scratch(&work, heel)?,
    )?;
    let mut spec = SessionSpec::new(name, work);
    spec.model.clone_from(&agent.model);
    spec.mode.clone_from(&agent.mode);
    spec.permission = Some(PermissionPolicy::Deny);
    // The timer is built inside the runtime's context. A handshake dropped at
    // the limit releases acpsub's last handle on the agent, which kills its
    // process tree.
    let session = in_runtime(runtime, async {
        tokio::time::timeout(limit, host.spawn(spec)).await
    })
    .await
    .map_err(|_| TrialError::TimedOut(limit))??;
    close_session(runtime, session).await;
    Ok(())
}

/// What an agent reports about itself in ACP `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    /// The agent's display name, else its name.
    pub name: String,
    /// The agent's version.
    pub version: String,
}

/// Why a command could not be confirmed as an ACP agent.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The command did not start or did not answer `initialize`.
    #[error(transparent)]
    Acpsub(#[from] acpsub::Error),
    /// The command stayed silent.
    #[error("no answer to ACP initialize within {0:?}")]
    TimedOut(std::time::Duration),
    /// The probe's sandbox could not be prepared.
    #[error("cannot prepare the probe's sandbox: {0}")]
    Scratch(std::io::Error),
}

/// Starts `command` in a sandbox granting an empty scratch directory and
/// nothing else, and asks it ACP `initialize`, giving up after `limit`. The
/// process is killed when the probe ends.
///
/// # Errors
/// Returns [`ProbeError`] when the command is not an ACP agent or the
/// sandbox cannot be prepared.
pub async fn probe_agent(
    command: &str,
    args: &[String],
    limit: std::time::Duration,
    heel: heel::HeelBinary,
) -> Result<AgentIdentity, ProbeError> {
    let launch = launch_config(command, args, &BTreeMap::new());
    let scratch = blocking::unblock(tempfile::tempdir)
        .await
        .map_err(ProbeError::Scratch)?;
    let cell = Arc::new(ArcSwap::from_pointee(
        SessionSandbox::scratch(scratch.path(), heel).map_err(ProbeError::Scratch)?,
    ));
    let services = Services {
        executor: SandboxExecutor::new(cell.clone(), Arc::default()),
        permissions: ApprovalPermissions::new(cell),
    };
    let answer = tokio::time::timeout(limit, probe_with(&launch, services))
        .await
        .map_err(|_| ProbeError::TimedOut(limit))??;
    let info = answer.agent_info.unwrap_or_default();
    Ok(AgentIdentity {
        name: info.title.unwrap_or_else(|| {
            if info.name.is_empty() {
                command.to_string()
            } else {
                info.name
            }
        }),
        version: info.version,
    })
}

/// What the main agent reads about a connected agent.
#[derive(askama::Template)]
#[template(path = "acp_agent_description.txt", escape = "none")]
struct DescriptionText<'a> {
    name: &'a str,
    version: &'a str,
}

/// The description of the subagent type an agent is connected as.
///
/// # Panics
/// Never: the template has no fallible expression.
#[must_use]
pub fn describe(identity: &AgentIdentity) -> String {
    use askama::Template as _;
    DescriptionText {
        name: &identity.name,
        version: &identity.version,
    }
    .render()
    .expect("the description template renders")
    .trim()
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aither_agent::SubagentRuns;
    use aither_core::llm::{CallIdentity, ToolCallId};

    use super::*;
    use crate::agent::PermissionApprover;

    fn fake_connection() -> AcpAgentConfig {
        AcpAgentConfig {
            command: "python3".to_string(),
            args: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_acp_agent.py")
                    .to_string_lossy()
                    .into_owned(),
            ],
            env: BTreeMap::new(),
            model: Some("a".to_string()),
            mode: Some("default".to_string()),
            description: "a scripted ACP agent".to_string(),
            grants: Vec::new(),
        }
    }

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// A session sandbox rooted in `root` that reads the fake agent's script
    /// and the `readable` directories and writes the `writable` ones.
    async fn sandbox(
        root: &Path,
        readable: Vec<PathBuf>,
        writable: Vec<PathBuf>,
        approver: Option<PermissionApprover>,
    ) -> SessionSandbox {
        let sandbox = super::super::acp_sandbox::test_sandbox(root).await;
        let grants = sandbox.grants();
        grants.add_readable(fixtures_dir());
        for path in readable {
            grants.add_readable(path);
        }
        for path in writable {
            grants.add_writable(path);
        }
        SessionSandbox::for_session(sandbox, approver, job_broker(root))
    }

    /// A broker over a small pool of its own under `root`.
    fn job_broker(root: &Path) -> aither_sandbox::JobBroker {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        // Inside `root`, which the test's own directory removes; one pool
        // per sandbox, as the root is shared by sandboxes of a test.
        let dir = tempfile::tempdir_in(root).expect("pool dir").keep();
        let pool = aither_sandbox::JobPool::create_fifo(dir.join("pool"), 1).expect("pool");
        let (broker, service) =
            aither_sandbox::job_broker_channel(pool, dir.join("commands")).expect("broker");
        executor_core::Executor::spawn(&executor_core::DefaultExecutor, async move {
            service.serve().await;
        })
        .detach();
        broker
    }

    fn hosts(may_dir: &Path, connected: BTreeMap<String, AcpAgentConfig>) -> AcpHosts {
        // The sandbox spawns on the daemon's global executor.
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let config = MayConfig {
            acp_agents: connected,
            ..MayConfig::default()
        };
        AcpHosts::new(
            Arc::new(ArcSwap::from_pointee(config)),
            MayHomePaths::new(may_dir.to_path_buf()),
            Handle::current(),
        )
    }

    /// The agents of a session that may write everything under `may_dir`.
    async fn agents(may_dir: &Path, connected: BTreeMap<String, AcpAgentConfig>) -> AcpAgents {
        let sandbox = sandbox(
            &may_dir.join("session"),
            Vec::new(),
            vec![may_dir.to_path_buf()],
            None,
        )
        .await;
        hosts(may_dir, connected)
            .agents("session", sandbox)
            .expect("host")
    }

    /// Runs `prompt` on a fake agent in a session whose sandbox is `sandbox`
    /// and returns the turn's reply.
    async fn fake_turn(
        may_dir: &Path,
        sandbox: SessionSandbox,
        cwd: &Path,
        prompt: &str,
    ) -> String {
        fake_turn_with(fake_connection(), may_dir, sandbox, cwd, prompt).await
    }

    /// As [`fake_turn`], with the agent started as `connection` says.
    async fn fake_turn_with(
        connection: AcpAgentConfig,
        may_dir: &Path,
        sandbox: SessionSandbox,
        cwd: &Path,
        prompt: &str,
    ) -> String {
        let acp = hosts(may_dir, BTreeMap::from([("fake".to_string(), connection)]))
            .agents("session", sandbox)
            .expect("host");
        let mut spec = SessionSpec::new("fake", cwd);
        spec.model = Some("a".to_string());
        spec.mode = Some("default".to_string());
        spec.permission = Some(PermissionPolicy::Deny);
        let session = tokio::time::timeout(
            Duration::from_secs(60),
            in_runtime(&acp.runtime, acp.host.spawn(spec)),
        )
        .await
        .expect("the sandboxed agent starts")
        .expect("the agent starts inside the sandbox");
        let outcome = tokio::time::timeout(
            Duration::from_secs(60),
            in_runtime(&acp.runtime, session.prompt(prompt)),
        )
        .await
        .expect("the turn ends")
        .expect("the turn runs");
        close_session(&acp.runtime, session).await;
        outcome.reply
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_acp_terminal_command_writes_only_inside_its_granted_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        let granted = dir.path().join("granted");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&granted).expect("granted dir");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let inside_file = granted.join("inside.txt");
        let outside_file = outside.join("outside.txt");

        let reply = fake_turn(
            dir.path(),
            sandbox(
                &dir.path().join("session"),
                Vec::new(),
                vec![granted.clone()],
                None,
            )
            .await,
            &granted,
            &format!(
                "RUN sh -c 'echo in > {}; echo out > {}'",
                inside_file.display(),
                outside_file.display()
            ),
        )
        .await;

        assert_eq!(
            std::fs::read_to_string(&inside_file).expect("the granted directory is writable"),
            "in\n",
            "{reply}"
        );
        assert!(
            !outside_file.exists(),
            "the command wrote outside its grants: {reply}"
        );
        assert!(reply.contains("exit:"), "{reply}");
        assert!(!reply.contains("exit:0"), "the denied write fails: {reply}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_installed_outside_the_session_keeps_its_state_in_a_granted_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        // Stands in for an install prefix and for `~/.codex`; neither is a
        // session grant.
        let prefix = dir.path().join("prefix");
        let state = dir.path().join("state");
        let outside = dir.path().join("outside");
        for path in [&prefix, &state, &outside, &dir.path().join("work")] {
            std::fs::create_dir_all(path).expect("dir");
        }
        std::fs::write(state.join("state"), "old\n").expect("state file");
        let program = prefix.join("acp-agent");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             cat \"$STATE_DIR/state\" > \"$STATE_DIR/seen\" || exit 7\n\
             echo started >> \"$STATE_DIR/state\" || exit 7\n\
             echo \"$HOME\" > \"$STATE_DIR/home\" || exit 7\n\
             echo leaked > \"$OUTSIDE_FILE\" 2>/dev/null && echo leaked >> \"$STATE_DIR/state\"\n\
             exec python3 \"$1\"\n",
        )
        .expect("agent script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let outside_file = outside.join("leak.txt");
        let connection = AcpAgentConfig {
            command: program.to_string_lossy().into_owned(),
            env: BTreeMap::from([
                (
                    "STATE_DIR".to_string(),
                    state.to_string_lossy().into_owned(),
                ),
                (
                    "OUTSIDE_FILE".to_string(),
                    outside_file.to_string_lossy().into_owned(),
                ),
            ]),
            grants: vec![state.clone()],
            ..fake_connection()
        };
        let mut connection = connection;
        connection.args = fake_connection().args;

        let work = dir.path().join("work");
        let reply = fake_turn_with(
            connection,
            dir.path(),
            sandbox(&dir.path().join("session"), Vec::new(), Vec::new(), None).await,
            &work,
            "say something",
        )
        .await;

        assert!(reply.contains("ab"), "the agent runs: {reply}");
        assert_eq!(
            std::fs::read_to_string(state.join("seen")).expect("the agent read its state"),
            "old\n"
        );
        assert_eq!(
            std::fs::read_to_string(state.join("state")).expect("the agent updated its state"),
            "old\nstarted\n"
        );
        assert_eq!(
            std::fs::read_to_string(state.join("home"))
                .expect("the agent reported its home")
                .trim_end(),
            std::fs::canonicalize(std::env::home_dir().expect("home"))
                .expect("home resolves")
                .to_string_lossy(),
            "the agent runs with the user's real home"
        );
        assert!(
            !outside_file.exists(),
            "the agent wrote outside its grants: {reply}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_permission_request_is_refused_unless_the_approver_grants_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        let unavailable = PermissionApprover::new()
            .for_session("session")
            .without_interactive_approval("this test");
        for approver in [None, Some(unavailable)] {
            let reply = fake_turn(
                dir.path(),
                sandbox(
                    &dir.path().join("session"),
                    Vec::new(),
                    vec![work.clone()],
                    approver,
                )
                .await,
                &work,
                "PERMISSION",
            )
            .await;
            assert!(
                reply.contains("perm:deny-1"),
                "an unanswered request selects the reject option: {reply}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_argument_naming_a_file_outside_the_grants_grants_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let work = dir.path().join("work");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&work).expect("work dir");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let secret = outside.join("hosts.yml");
        std::fs::write(&secret, "oauth_token: hunter2\n").expect("secret file");

        let reply = fake_turn(
            dir.path(),
            sandbox(
                &dir.path().join("session"),
                Vec::new(),
                vec![work.clone()],
                None,
            )
            .await,
            &work,
            &format!("RUN cat {}", secret.display()),
        )
        .await;

        assert!(
            !reply.contains("hunter2"),
            "the command read a file only its argument named: {reply}"
        );
        assert!(reply.contains("exit:"), "{reply}");
        assert!(!reply.contains("exit:0"), "the denied read fails: {reply}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_linked_program_starts_under_its_link_name() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        // `bin/agent` is a link to `prefix/real-agent`, as `cargo` is a link
        // to `rustup`; neither directory is a session grant.
        let prefix = dir.path().join("prefix");
        let bin = dir.path().join("bin");
        let state = dir.path().join("state");
        let work = dir.path().join("work");
        for path in [&prefix, &bin, &state, &work] {
            std::fs::create_dir_all(path).expect("dir");
        }
        let program = prefix.join("real-agent");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             echo \"$0\" > \"$STATE_DIR/argv0\" || exit 7\n\
             echo \"$TMPDIR\" > \"$STATE_DIR/tmpdir\" || exit 7\n\
             exec python3 \"$1\"\n",
        )
        .expect("agent script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let link = bin.join("agent");
        std::os::unix::fs::symlink(&program, &link).expect("link");
        let connection = AcpAgentConfig {
            command: link.to_string_lossy().into_owned(),
            env: BTreeMap::from([(
                "STATE_DIR".to_string(),
                state.to_string_lossy().into_owned(),
            )]),
            grants: vec![state.clone()],
            ..fake_connection()
        };

        let session = dir.path().join("session");
        let reply = fake_turn_with(
            connection,
            dir.path(),
            sandbox(&session, Vec::new(), Vec::new(), None).await,
            &work,
            "say something",
        )
        .await;

        assert!(reply.contains("ab"), "the agent runs: {reply}");
        assert_eq!(
            std::fs::read_to_string(state.join("argv0"))
                .expect("the agent recorded its name")
                .trim_end(),
            link.to_string_lossy(),
            "the agent starts under its link's name"
        );
        assert_eq!(
            Path::new(
                std::fs::read_to_string(state.join("tmpdir"))
                    .expect("the agent recorded its TMPDIR")
                    .trim_end()
            ),
            std::fs::canonicalize(&session)
                .expect("session resolves")
                .join("tmp"),
            "temporary files go inside the sandbox"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sandboxed_agent_is_offered_no_client_file_access() {
        let dir = tempfile::tempdir().expect("temp dir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        let file = work.join("notes.txt");
        std::fs::write(&file, "readable by the agent itself").expect("file");
        let session = async || {
            sandbox(
                &dir.path().join("session"),
                Vec::new(),
                vec![work.clone()],
                None,
            )
            .await
        };

        let caps = fake_turn(dir.path(), session().await, &work, "CAPS").await;
        assert!(
            caps.contains("caps:read=false,write=false"),
            "initialize advertises no fs capability: {caps}"
        );
        let read = fake_turn(
            dir.path(),
            session().await,
            &work,
            &format!("READ {}", file.display()),
        )
        .await;
        assert!(
            read.contains("fserr:Method not found"),
            "acpsub refuses fs/read_text_file: {read}"
        );
        assert!(!read.contains("readable by the agent"), "{read}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_session_ends_its_agents_and_removes_their_storage() {
        let dir = tempfile::tempdir().expect("temp dir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        let hosts = hosts(
            dir.path(),
            BTreeMap::from([("fake".to_string(), fake_connection())]),
        );
        let acp = hosts
            .agents(
                "session",
                sandbox(
                    &dir.path().join("session"),
                    Vec::new(),
                    vec![work.clone()],
                    None,
                )
                .await,
            )
            .expect("host");
        let backend = AcpSubagent {
            host: acp.host.clone(),
            runtime: acp.runtime.clone(),
            lease: acp.lease.clone(),
            agent: "fake".to_string(),
            model: Some("a".to_string()),
            mode: Some("default".to_string()),
        };
        let start = || ExternalStart {
            working_dir: &work,
            worktrees: &[],
            commit_worktrees: &[],
            working_copies: &[],
        };
        let open = || async {
            tokio::time::timeout(Duration::from_secs(60), backend.open(start()))
                .await
                .expect("the agent starts")
                .expect("the session opens")
        };
        let dropped = open().await;
        let kept = open().await;
        let storage = MayHomePaths::new(dir.path().to_path_buf()).acp_session_dir("session");
        assert!(storage.is_dir(), "acpsub stores the session's agents");

        // The dropped session's close is in flight while the session goes.
        drop(dropped);
        tokio::time::timeout(Duration::from_secs(60), hosts.remove_session("session"))
            .await
            .expect("the session's agents end")
            .expect("the storage is removed");

        assert!(!storage.exists(), "the session's ACP storage is gone");
        assert!(
            acp.host.state().live.lock().expect("live table").is_empty(),
            "every agent session of the deleted session ended"
        );
        // A session closed after the delete leaves nothing behind, and no new
        // one starts for the deleted session.
        kept.close().await;
        assert!(!storage.exists(), "a late close recreated the storage");
        let refused = backend
            .open(start())
            .await
            .expect_err("nothing starts for a deleted session");
        assert!(
            matches!(refused, AcpError::SessionDeleted { .. }),
            "{refused}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_connected_agent_runs_as_a_child_and_is_continued_on_its_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        let acp = agents(
            dir.path(),
            BTreeMap::from([("fake".to_string(), fake_connection())]),
        )
        .await;
        let externals = acp.externals().expect("the name is free");
        let (name, description) = externals.described().next().expect("one connected agent");
        assert_eq!(
            (name, description),
            ("fake", "a scripted ACP agent"),
            "the connected agent is listed with its description"
        );
        let mut session = tokio::time::timeout(
            Duration::from_secs(60),
            externals
                .get("fake")
                .expect("connected")
                .backend
                .open(ExternalStart {
                    working_dir: &work,
                    worktrees: &[],
                    commit_worktrees: &[],
                    working_copies: &[],
                }),
        )
        .await
        .expect("the agent starts")
        .expect("the session opens");

        let runs = SubagentRuns::default();
        let caller =
            CallIdentity::new(ToolCallId::new("c1"), aither_core::llm::AgentIdentity::Main);
        for prompt in ["say something", "say it again"] {
            let run = runs.start(&caller, "fake", prompt);
            let outcome =
                tokio::time::timeout(Duration::from_secs(60), run.run(&mut session, prompt))
                    .await
                    .expect("the subagent answers");
            let RunOutcome::Completed { final_text } = outcome else {
                panic!("the turn completes: {outcome:?}");
            };
            assert!(
                final_text.contains("ab"),
                "the reply comes back: {final_text}"
            );
        }
        assert!(runs.list().is_empty(), "every run closed its registration");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worktree_grants_are_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let acp = agents(dir.path(), BTreeMap::new()).await;
        let backend = AcpSubagent {
            host: acp.host,
            runtime: acp.runtime,
            lease: acp.lease,
            agent: "fake".to_string(),
            model: None,
            mode: None,
        };
        let grant = [dir.path().join("tree")];
        for (worktrees, commit_worktrees) in [(&grant[..], &[][..]), (&[][..], &grant[..])] {
            let refused = backend
                .open(ExternalStart {
                    working_dir: dir.path(),
                    worktrees,
                    commit_worktrees,
                    working_copies: &[],
                })
                .await
                .expect_err("a grant is refused before any process starts");
            assert!(
                matches!(refused, AcpError::WorktreeGrant { .. }),
                "{refused}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_colliding_name_is_refused_not_overridden() {
        let dir = tempfile::tempdir().expect("temp dir");
        let acp = agents(
            dir.path(),
            BTreeMap::from([("explore".to_string(), fake_connection())]),
        )
        .await;
        let error = acp
            .externals()
            .map(|_| ())
            .expect_err("the built-in type keeps its name");
        assert!(error.to_string().contains("built-in"), "{error}");
    }

    #[test]
    fn only_a_finished_turn_is_a_reply() {
        assert_eq!(
            finished("done".to_string(), TurnEnd::Stopped(StopReason::EndTurn)).expect("reply"),
            "done"
        );
    }

    #[test]
    fn every_other_turn_end_names_what_happened() {
        let resume_at = jiff::Timestamp::UNIX_EPOCH;
        let cases = [
            (TurnEnd::Stopped(StopReason::Cancelled), "Cancelled"),
            (TurnEnd::Stopped(StopReason::MaxTokens), "MaxTokens"),
            (
                TurnEnd::Failed("model stream dropped".into()),
                "model stream dropped",
            ),
            (TurnEnd::Exited("status 3".into()), "status 3"),
            (
                TurnEnd::NeedsPermission {
                    request_id: "r1".into(),
                    title: "rm -rf".into(),
                },
                "rm -rf",
            ),
            (
                TurnEnd::RateLimited {
                    resume_at,
                    reason: "free tier".into(),
                },
                "1970-01-01T00:00:00Z",
            ),
        ];
        for (end, expected) in cases {
            let error = finished("partial".to_string(), end.clone()).expect_err("not a reply");
            assert!(
                error.to_string().contains(expected),
                "{end:?} reads as {error}"
            );
        }
    }
}
