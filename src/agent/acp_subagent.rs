//! ACP agents connected with `/connect`, run as subagent types.
//!
//! The daemon owns one [`acpsub::Host`] whose configuration is built from the
//! live [`MayConfig`] on every load. [`AcpSubagent`] adapts it to aither's
//! [`ExternalSubagent`], so a connected agent is launched, continued, tracked
//! and cancelled by the `subagent` command like a built-in type.
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
    AgentConfig, Config, ConfigSource, Defaults, Host, PermissionPolicy, Services, Session,
    SessionSpec, TurnEnd, probe_with,
};
use aither_acp::StopReason;
use aither_agent::specialized::{ExternalSession, ExternalStart, ExternalSubagent, SubagentTool};
use aither_core::LanguageModel;
use arc_swap::ArcSwap;
use tokio::runtime::Handle;

use super::acp_sandbox::{ApprovalPermissions, SandboxExecutor, SessionSandbox};
use crate::config::{AcpAgentConfig, MayConfig};

/// Where May keeps what acpsub stores, under the May home.
const STORAGE_DIR: &str = "acp";

/// An acpsub host whose agents and their commands run in a session sandbox
/// and whose permission requests go to the session's approver.
pub type SandboxedHost = Host<SandboxExecutor, ApprovalPermissions>;

/// A session of a [`SandboxedHost`].
pub type SandboxedSession = Session<SandboxExecutor, ApprovalPermissions>;

/// A session's host and the cell holding the sandbox it starts processes in.
#[derive(Clone)]
struct SessionHost {
    host: SandboxedHost,
    sandbox: Arc<ArcSwap<SessionSandbox>>,
}

/// The daemon's ACP hosts, one per session: a session's registry of resumable
/// agent sessions is its own, and so is the sandbox its processes start in.
pub struct AcpHosts {
    config: Arc<ArcSwap<MayConfig>>,
    may_dir: PathBuf,
    runtime: Handle,
    hosts: std::sync::Mutex<BTreeMap<String, SessionHost>>,
}

impl std::fmt::Debug for AcpHosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpHosts")
            .field("may_dir", &self.may_dir)
            .finish_non_exhaustive()
    }
}

impl AcpHosts {
    /// Hosts over May's live configuration; storage lives under
    /// `<may_dir>/acp`.
    #[must_use]
    pub fn new(config: Arc<ArcSwap<MayConfig>>, may_dir: &Path, runtime: Handle) -> Self {
        Self {
            config,
            may_dir: may_dir.to_path_buf(),
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
        let host = self.session_host(session_id, sandbox)?;
        Ok(AcpAgents {
            host,
            runtime: self.runtime.clone(),
            connected: self.config.load().acp_agents.clone(),
        })
    }

    /// The host of `session_id`, created on first use, with `sandbox`
    /// installed for the processes it starts from now on.
    fn session_host(
        &self,
        session_id: &str,
        sandbox: SessionSandbox,
    ) -> Result<SandboxedHost, acpsub::Error> {
        let mut hosts = self.hosts.lock().expect("the host table is not poisoned");
        if let Some(session) = hosts.get(session_id) {
            session.sandbox.store(Arc::new(sandbox));
            return Ok(session.host.clone());
        }
        let cell = Arc::new(ArcSwap::from_pointee(sandbox));
        let source = LiveConfig {
            config: self.config.clone(),
            storage: self
                .may_dir
                .join(STORAGE_DIR)
                .join("sessions")
                .join(session_id),
        };
        let host = Host::from_config_with(
            source.build(),
            source,
            Services {
                executor: SandboxExecutor::new(cell.clone(), self.config.clone()),
                permissions: ApprovalPermissions::new(cell.clone()),
            },
        )?;
        hosts.insert(
            session_id.to_string(),
            SessionHost {
                host: host.clone(),
                sandbox: cell,
            },
        );
        drop(hosts);
        Ok(host)
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
}

impl AcpAgents {
    /// Registers every connected agent on `tool` as a subagent type under its
    /// name.
    ///
    /// # Errors
    /// Returns the config error of a name that cannot be a subagent type;
    /// nothing is overridden silently.
    pub fn register<LLM: LanguageModel + Clone + Sync + 'static>(
        &self,
        mut tool: SubagentTool<LLM>,
    ) -> Result<SubagentTool<LLM>, crate::config::ConfigError> {
        for (name, connection) in &self.connected {
            crate::config::check_acp_agent_name(name)?;
            let backend = AcpSubagent {
                host: self.host.clone(),
                runtime: self.runtime.clone(),
                agent: name.clone(),
                model: connection.model.clone(),
                mode: connection.mode.clone(),
            };
            tool = tool.with_external(name.clone(), connection.description.clone(), backend);
        }
        Ok(tool)
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
            let mut launch = AgentConfig::new(&agent.command);
            launch.args.clone_from(&agent.args);
            launch.env.clone_from(&agent.env);
            config.with_agent(name, launch)
        })
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
    agent: String,
    model: Option<String>,
    mode: Option<String>,
}

impl ExternalSubagent for AcpSubagent {
    type Session = AcpSession;
    type Error = AcpError;

    async fn open(&self, start: ExternalStart<'_>) -> Result<AcpSession, AcpError> {
        if !start.worktrees.is_empty() || !start.commit_worktrees.is_empty() {
            return Err(AcpError::WorktreeGrant {
                agent: self.agent.clone(),
            });
        }
        let mut spec = SessionSpec::new(&self.agent, start.working_dir);
        spec.model.clone_from(&self.model);
        spec.mode.clone_from(&self.mode);
        spec.permission = Some(PermissionPolicy::Deny);
        let session = in_runtime(&self.runtime, self.host.spawn(spec)).await?;
        Ok(AcpSession {
            session: Some(session),
            runtime: self.runtime.clone(),
        })
    }
}

/// One conversation with a connected agent.
#[derive(Debug)]
pub struct AcpSession {
    /// `None` once [`ExternalSession::close`] took it.
    session: Option<SandboxedSession>,
    runtime: Handle,
}

impl AcpSession {
    const fn session(&self) -> &SandboxedSession {
        self.session
            .as_ref()
            .expect("a session is open until it is closed, which consumes it")
    }
}

impl ExternalSession for AcpSession {
    type Error = AcpError;

    async fn prompt(&mut self, prompt: &str) -> Result<String, AcpError> {
        let outcome = in_runtime(&self.runtime, self.session().prompt(prompt)).await?;
        finished(outcome.reply, outcome.end)
    }

    async fn cancel(&mut self) {
        // A turn that ended before the cancel arrived is not a failure.
        if let Err(error) = in_runtime(&self.runtime, self.session().cancel()).await
            && !matches!(error, acpsub::Error::NotRunning { .. })
        {
            tracing::warn!(%error, "cancelling the ACP agent's turn failed");
        }
    }

    async fn close(mut self) {
        let session = self
            .session
            .take()
            .expect("a session is open until it is closed, which consumes it");
        close_session(&self.runtime, session).await;
    }

    fn model(&self) -> Option<String> {
        self.session().model().map(|model| {
            model
                .as_str()
                .map_or_else(|| model.to_string(), str::to_string)
        })
    }
}

impl Drop for AcpSession {
    /// A session dropped without [`ExternalSession::close`] still ends: its
    /// agent process must not outlive it.
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let runtime = self.runtime.clone();
            self.runtime.spawn(async move {
                close_session(&runtime, session).await;
            });
        }
    }
}

async fn close_session(runtime: &Handle, session: SandboxedSession) {
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
pub async fn try_connection(
    runtime: &Handle,
    name: &str,
    agent: &AcpAgentConfig,
) -> Result<(), TrialError> {
    let scratch = blocking::unblock(tempfile::tempdir).await?;
    let work = scratch.path().join("work");
    async_fs::create_dir_all(&work).await?;
    let host = trial_host(
        &BTreeMap::from([(name.to_string(), agent.clone())]),
        &scratch.path().join("acp"),
        SessionSandbox::scratch(&work)?,
    )?;
    let mut spec = SessionSpec::new(name, work);
    spec.model.clone_from(&agent.model);
    spec.mode.clone_from(&agent.mode);
    spec.permission = Some(PermissionPolicy::Deny);
    let session = in_runtime(runtime, host.spawn(spec)).await?;
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
) -> Result<AgentIdentity, ProbeError> {
    let mut launch = AgentConfig::new(command);
    launch.args = args.to_vec();
    let scratch = blocking::unblock(tempfile::tempdir)
        .await
        .map_err(ProbeError::Scratch)?;
    let cell = Arc::new(ArcSwap::from_pointee(
        SessionSandbox::scratch(scratch.path()).map_err(ProbeError::Scratch)?,
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

    use aither_agent::specialized::SubagentArgs;
    use aither_core::llm::tool::{ToolContext, ToolResult};
    use aither_core::llm::{Event, LLMRequest, Tool, model::Profile};
    use aither_sandbox::CommandUsage;
    use futures_lite::Stream;

    use super::*;
    use crate::agent::PermissionApprover;

    /// A model that is never asked anything: external subagents bring their own.
    #[derive(Clone)]
    struct NoLlm;

    #[derive(Debug, thiserror::Error)]
    #[error("unused")]
    struct NoLlmError;

    impl aither_core::llm::Classify for NoLlmError {
        fn failure(&self) -> aither_core::llm::Failure {
            aither_core::llm::Failure::permanent()
        }
    }

    impl LanguageModel for NoLlm {
        type Error = NoLlmError;

        fn respond(
            &self,
            _request: LLMRequest,
        ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
            futures_lite::stream::empty()
        }

        fn profile(&self) -> impl Future<Output = Result<Profile, NoLlmError>> + Send {
            std::future::ready(Ok(Profile::new("none", "test", "none", "none", 1_000)))
        }
    }

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
    fn sandbox(
        root: &Path,
        readable: Vec<PathBuf>,
        writable: Vec<PathBuf>,
        approver: Option<PermissionApprover>,
    ) -> SessionSandbox {
        let grants = aither_sandbox::TerminalGrants::new();
        grants.add_readable(fixtures_dir());
        for path in readable {
            grants.add_readable(path);
        }
        for path in writable {
            grants.add_writable(path);
        }
        std::fs::create_dir_all(root).expect("session dir");
        SessionSandbox::for_session(
            root.to_path_buf(),
            grants,
            crate::terminal_permission_handler::TerminalPermissionHandler::for_agent(
                None, None, None,
            ),
            heel::NetworkAuditLog::rolling_daily(root.join("audit"), 1).expect("audit log"),
            aither_sandbox::ProcessPriority::default(),
            approver,
        )
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
            may_dir,
            Handle::current(),
        )
    }

    /// The agents of a session that may write everything under `may_dir`.
    fn agents(may_dir: &Path, connected: BTreeMap<String, AcpAgentConfig>) -> AcpAgents {
        hosts(may_dir, connected)
            .agents(
                "session",
                sandbox(
                    &may_dir.join("session"),
                    Vec::new(),
                    vec![may_dir.to_path_buf()],
                    None,
                ),
            )
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
            ),
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
            sandbox(&dir.path().join("session"), Vec::new(), Vec::new(), None),
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
                ),
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

    async fn call_text(
        tool: &SubagentTool<NoLlm>,
        args: SubagentArgs,
    ) -> aither_core::Result<String> {
        match Tool::call(tool, args, ToolContext::new()).await? {
            ToolResult::Text { text } => Ok(text),
            other => panic!("expected a text result, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_main_agent_starts_and_resumes_a_connected_agent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let acp = agents(
            dir.path(),
            BTreeMap::from([("fake".to_string(), fake_connection())]),
        );
        let tool = acp
            .register(
                SubagentTool::new(NoLlm, dir.path().join("subagents")).with_base_dir(dir.path()),
            )
            .expect("the name is free");
        assert!(
            tool.usage().await.contains("- fake: a scripted ACP agent"),
            "the connected agent is listed with its description"
        );
        let prompt = dir.path().join("prompt.txt");
        std::fs::write(&prompt, "say something").expect("prompt file");
        let start = |prompt: &Path| SubagentArgs {
            name: "external".to_string(),
            prompt_file: prompt.to_string_lossy().into_owned(),
            id: None,
            agent: Some("fake".to_string()),
            worktrees: Vec::new(),
            commit_worktrees: Vec::new(),
        };

        let text = tokio::time::timeout(Duration::from_secs(60), call_text(&tool, start(&prompt)))
            .await
            .expect("the subagent answers")
            .expect("start succeeds");
        assert!(text.contains("ab"), "the agent's reply comes back: {text}");

        let id = text
            .split(|c: char| !c.is_ascii_digit())
            .find_map(|word| word.parse::<u64>().ok())
            .expect("the result carries the continuation id");
        let again = tokio::time::timeout(
            Duration::from_secs(60),
            call_text(
                &tool,
                SubagentArgs {
                    name: "resume".to_string(),
                    prompt_file: prompt.to_string_lossy().into_owned(),
                    id: Some(id),
                    agent: None,
                    worktrees: Vec::new(),
                    commit_worktrees: Vec::new(),
                },
            ),
        )
        .await
        .expect("the resumed subagent answers")
        .expect("resume succeeds");
        assert!(again.contains("ab"), "{again}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worktree_grants_are_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let acp = agents(dir.path(), BTreeMap::new());
        let backend = AcpSubagent {
            host: acp.host,
            runtime: acp.runtime,
            agent: "fake".to_string(),
            model: None,
            mode: None,
        };
        let grant = [dir.path().join("tree")];
        for (worktrees, commit_worktrees) in [(&grant[..], &[][..]), (&[][..], &grant[..])] {
            let refused = ExternalSubagent::open(
                &backend,
                ExternalStart {
                    working_dir: dir.path(),
                    worktrees,
                    commit_worktrees,
                },
            )
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
        );
        let error = acp
            .register(SubagentTool::new(NoLlm, dir.path().join("subagents")))
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
