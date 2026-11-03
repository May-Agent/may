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
    AgentConfig, Config, ConfigSource, Defaults, Host, PermissionPolicy, Session, SessionSpec,
    TurnEnd,
};
use aither_acp::StopReason;
use aither_agent::specialized::{
    ExternalSession, ExternalStart, ExternalSubagent, SubagentTool, SubagentType,
};
use aither_core::LanguageModel;
use arc_swap::ArcSwap;
use tokio::runtime::Handle;

use crate::config::{AcpAgentConfig, MayConfig};

/// Where May keeps what acpsub stores, under the May home.
const STORAGE_DIR: &str = "acp";

/// The connected ACP agents and the host that runs them, as an agent build
/// receives them.
#[derive(Debug, Clone)]
pub struct AcpAgents {
    /// The daemon's acpsub host.
    pub host: Host,
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
            tool = tool.with_type(
                name.clone(),
                SubagentType::external(connection.description.clone(), backend),
            );
        }
        Ok(tool)
    }
}

/// The acpsub host over May's live configuration: transcripts and the session
/// registry live under `<may_dir>/acp`.
///
/// # Errors
/// Returns acpsub's error when the registry file exists but cannot be parsed.
pub fn live_host(config: Arc<ArcSwap<MayConfig>>, may_dir: &Path) -> Result<Host, acpsub::Error> {
    let source = LiveConfig {
        config,
        may_dir: may_dir.to_path_buf(),
    };
    Host::from_config(source.build(), source)
}

/// A host that knows exactly `agents`, with its storage under `storage`: the
/// trial that proves a candidate connection works before it is configured.
///
/// # Errors
/// Returns acpsub's error when the registry file exists but cannot be parsed.
pub fn trial_host(
    agents: &BTreeMap<String, AcpAgentConfig>,
    storage: &Path,
) -> Result<Host, acpsub::Error> {
    let source = FixedConfig(acpsub_config(agents, storage));
    Host::from_config(source.0.clone(), source)
}

/// What a connection looks like to acpsub.
fn acpsub_config(agents: &BTreeMap<String, AcpAgentConfig>, storage: &Path) -> Config {
    let mut defaults = Defaults::new(storage.join("transcripts"), storage.join("registry.json"));
    defaults.permission = PermissionPolicy::Allow;
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
    may_dir: PathBuf,
}

impl LiveConfig {
    fn build(&self) -> Config {
        acpsub_config(
            &self.config.load().acp_agents,
            &self.may_dir.join(STORAGE_DIR),
        )
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
    host: Host,
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
        spec.permission = Some(PermissionPolicy::Allow);
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
    session: Option<Session>,
    runtime: Handle,
}

impl AcpSession {
    const fn session(&self) -> &Session {
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

async fn close_session(runtime: &Handle, session: Session) {
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
/// is registered or stored under the May home.
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
    )?;
    let mut spec = SessionSpec::new(name, work);
    spec.model.clone_from(&agent.model);
    spec.mode.clone_from(&agent.mode);
    spec.permission = Some(PermissionPolicy::Allow);
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
}

/// Starts `command` and asks it ACP `initialize`, giving up after `limit`.
/// The process is killed when the probe ends.
///
/// # Errors
/// Returns [`ProbeError`] when the command is not an ACP agent.
pub async fn probe_agent(
    command: &str,
    args: &[String],
    limit: std::time::Duration,
) -> Result<AgentIdentity, ProbeError> {
    let mut launch = AgentConfig::new(command);
    launch.args = args.to_vec();
    let answer = tokio::time::timeout(limit, acpsub::probe(&launch))
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
    use futures_lite::Stream;

    use super::*;

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
        }
    }

    fn agents(may_dir: &Path, connected: BTreeMap<String, AcpAgentConfig>) -> AcpAgents {
        let config = MayConfig {
            acp_agents: connected.clone(),
            ..MayConfig::default()
        };
        AcpAgents {
            host: live_host(Arc::new(ArcSwap::from_pointee(config)), may_dir).expect("host"),
            runtime: Handle::current(),
            connected,
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
            .register(SubagentTool::new(NoLlm).with_base_dir(dir.path()))
            .expect("the name is free");
        assert!(
            tool.type_descriptions()
                .contains(&("fake", "a scripted ACP agent"))
        );
        let prompt = dir.path().join("prompt.txt");
        std::fs::write(&prompt, "say something").expect("prompt file");
        let start = |prompt: &Path| SubagentArgs::Start {
            subagent: "fake".to_string(),
            prompt_file: prompt.to_string_lossy().into_owned(),
            model: None,
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
                SubagentArgs::Resume {
                    id,
                    prompt_file: prompt.to_string_lossy().into_owned(),
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
            .register(SubagentTool::new(NoLlm).with_builtins())
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
