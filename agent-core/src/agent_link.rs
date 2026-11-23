//! Main agents discover each other and exchange messages.
//!
//! The daemon hosts every session, so "live" is what its runtime service
//! sees: any session that is not archived is reachable. A message is stored
//! in the recipient's pending-message queue in `history.db` first, so a busy
//! or restarting recipient does not lose it; the wake callback then nudges
//! the daemon to dispatch what is already stored.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cache_lifetime::CacheLifetime;
use crate::db::ChatDbError;
use crate::session::{
    MayHomePaths, PendingMessageState, PendingUserMessage, PendingUserMessageMode,
    SessionDirectory, SessionRuntimeService,
};
use crate::xml::{XmlBlock, XmlBlockError};

/// Heading of the `tasks.md` section that lists a session's roots.
const ROOTS_HEADING: &str = "## Roots";

/// Whether an agent is working or waiting for input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// A turn is in flight or the session is blocked on an interaction.
    Running,
    /// Nothing is in flight.
    Idle,
}

impl std::fmt::Display for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Idle => "idle",
        })
    }
}

/// What a live main agent reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    /// Session the agent runs.
    pub session_id: String,
    /// Session's display name.
    pub name: String,
    /// Directory the agent was started in.
    pub cwd: PathBuf,
    /// Whether the agent is working or waiting.
    pub activity: Activity,
    /// The session's `## Roots` list, empty while it has none.
    pub roots: Vec<String>,
}

/// Failure of agent discovery or of a message exchange.
#[derive(Debug, thiserror::Error)]
pub enum AgentLinkError {
    /// Storage failed.
    #[error(transparent)]
    Db(#[from] ChatDbError),
    /// `tasks.md` could not be read.
    #[error("failed to read tasks.md: {0}")]
    Tasks(#[source] std::io::Error),
    /// A session's stored repository binding is malformed.
    #[error(transparent)]
    Binding(#[from] crate::session::BindingError),
    /// No live agent matches the name.
    #[error("no live agent named \"{0}\"; run `agent list`")]
    NoSuchAgent(String),
    /// Several live agents match the name.
    #[error("{0}")]
    AmbiguousAgent(String),
    /// A remote peer machine could not be reached or refused the request.
    #[error("remote peer \"{machine}\" could not complete the agent request: {reason}")]
    PeerUnavailable {
        /// Target machine name.
        machine: String,
        /// Failure detail.
        reason: String,
    },
    /// The sender addressed itself.
    #[error("an agent cannot send a message to itself")]
    SendToSelf,
    /// A container session named a file outside its mounted sandbox.
    #[error("{0} is outside the session workspace")]
    OutsideSandbox(String),
    /// The message body is empty.
    #[error("the message in {0} is empty")]
    EmptyMessage(PathBuf),
    /// The prompt file could not be read.
    #[error("failed to read prompt file {path}: {source}")]
    PromptFile {
        /// File path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The recipient is idle past its prompt-cache lifetime.
    #[error("{}", cold_recipient_message(.0))]
    ColdRecipient(String),
    /// The agent asked itself a btw question.
    #[error("an agent cannot ask itself a btw question")]
    AskSelf,
    /// The btw question is empty.
    #[error("the btw question is empty")]
    EmptyQuestion,
    /// A btw recipient is idle past its prompt-cache lifetime.
    #[error("{}", cold_btw_recipient_message(.0))]
    ColdBtwRecipient(String),
    /// The btw recipient's agent has sent no request since it was built, so
    /// there is no context to fork.
    #[error(
        "\"{0}\" has sent no request since its agent was started, so it has no context to answer from; ask again once it has worked"
    )]
    NoForkContext(String),
    /// The btw recipient's context is too close to its compaction point to
    /// hold a fork's question and answer.
    #[error(
        "\"{0}\" is about to compact its context, so a fork of it cannot answer; ask again after it compacts"
    )]
    NearCompaction(String),
    /// The fork answering a btw question failed.
    #[error("the fork answering the btw question failed: {0}")]
    Fork(String),
    /// The delivery text could not be wrapped as hidden context.
    #[error(transparent)]
    Context(#[from] XmlBlockError),
}

/// The directory of sessions this daemon hosts: who is live, and how to
/// nudge one to dispatch what was stored for it.
#[derive(Clone)]
pub struct AgentDirectory {
    runtime_service: Arc<SessionRuntimeService>,
    /// `~/.may`.
    may_dir: PathBuf,
    /// Nudges the daemon to dispatch a session's stored pending work.
    wake: Arc<dyn Fn(&str) + Send + Sync>,
    /// Optional cross-machine peer fabric (Tailscale peers + cloud homes).
    peers: Option<AnyPeerFabric>,
}

impl std::fmt::Debug for AgentDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentDirectory").finish_non_exhaustive()
    }
}

impl AgentDirectory {
    /// Sessions over `runtime_service`; `wake` is called once per stored
    /// message, after it is in the queue.
    #[must_use]
    pub fn new(
        runtime_service: Arc<SessionRuntimeService>,
        may_dir: PathBuf,
        wake: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime_service,
            may_dir,
            wake: Arc::new(wake),
            peers: None,
        }
    }

    /// Attaches a cross-machine [`PeerFabric`] so `agent list`, `agent send`,
    /// and `btw` span same-owner Tailscale peers and cloud homes.
    #[must_use]
    pub fn with_peers(mut self, peers: impl PeerFabric) -> Self {
        self.peers = Some(AnyPeerFabric::new(peers));
        self
    }

    /// Attaches an already-wrapped [`AnyPeerFabric`].
    #[must_use]
    pub fn with_any_peers(mut self, peers: AnyPeerFabric) -> Self {
        self.peers = Some(peers);
        self
    }

    /// Returns the attached cross-machine [`AnyPeerFabric`], if configured.
    #[must_use]
    pub const fn peers(&self) -> Option<&AnyPeerFabric> {
        self.peers.as_ref()
    }

    /// Every live agent, sorted by name: every session that is not archived.
    ///
    /// # Errors
    /// [`AgentLinkError`] on storage failure or an unreadable `tasks.md`.
    pub async fn live(&self) -> Result<Vec<AgentStatus>, AgentLinkError> {
        Ok(self
            .agent_tasks()
            .await?
            .into_iter()
            .map(|agent| agent.status)
            .collect())
    }

    /// Local sessions formatted as [`RemoteAgentEntry`] rows for `GET /api/peers/agents`.
    ///
    /// # Errors
    /// [`AgentLinkError`] on storage failure or an unreadable `tasks.md`.
    pub async fn local_peer_entries(
        &self,
        machine: &str,
    ) -> Result<Vec<RemoteAgentEntry>, AgentLinkError> {
        Ok(self
            .agent_tasks()
            .await?
            .into_iter()
            .map(|task| RemoteAgentEntry {
                machine: machine.to_string(),
                status: task.status,
                suspended: false,
                repo: task.binding.map(|binding| binding.to_stored()),
            })
            .collect())
    }

    /// Every live agent with the live `tasks.md` of its session, sorted like
    /// [`Self::live`].
    ///
    /// # Errors
    /// [`AgentLinkError`] on storage failure or an unreadable `tasks.md`.
    pub async fn agent_tasks(&self) -> Result<Vec<AgentTasks>, AgentLinkError> {
        let mut live = Vec::new();
        for session in self.runtime_service.db().list_sessions().await? {
            if session.archived {
                continue;
            }
            if let Some(peers) = &self.peers
                && peers.is_cloud_shadow(&session.id)
            {
                continue;
            }
            let home = MayHomePaths::new(self.may_dir.clone());
            let paths = session.profile_id.as_deref().map_or_else(
                || home.session_paths(&session.id),
                |profile_id| home.profile_home(profile_id).session_paths(&session.id),
            );
            let tasks = paths.tasks_content().await.map_err(AgentLinkError::Tasks)?;
            let state = self.runtime_service.get(&session.id).await?.state;
            let binding = session
                .repo
                .as_deref()
                .map(crate::session::RepoBinding::parse)
                .transpose()?;
            live.push(AgentTasks {
                binding,
                status: AgentStatus {
                    name: session.title.clone().unwrap_or_else(|| session.id.clone()),
                    session_id: session.id.clone(),
                    cwd: SessionDirectory::of(&session, &self.may_dir).into_path(),
                    activity: if state.is_idle() {
                        Activity::Idle
                    } else {
                        Activity::Running
                    },
                    roots: tasks.as_deref().map_or_else(Vec::new, parse_roots),
                },
                workspace_config: paths.workspace_config(),
                tasks_path: paths.tasks_path(),
                tasks,
            });
        }
        live.sort_by(|a, b| {
            a.status
                .name
                .cmp(&b.status.name)
                .then_with(|| a.status.session_id.cmp(&b.status.session_id))
        });
        Ok(live)
    }

    /// The daemon's chat database.
    #[must_use]
    pub fn db(&self) -> crate::db::ChatDb {
        self.runtime_service.db()
    }

    /// Ask the daemon to dispatch what is stored for `session_id`.
    pub fn wake(&self, session_id: &str) {
        (self.wake)(session_id);
    }

    /// Store `text` in the queue of `session_id` as a steer and wake it.
    ///
    /// # Errors
    /// Fails when the text cannot be parsed or queued.
    pub async fn deliver(&self, session_id: &str, text: String) -> Result<(), AgentLinkError> {
        let message = PendingUserMessage {
            id: None,
            state: PendingMessageState::default(),
            error: None,
            content: String::new(),
            attachments: Vec::new(),
            hidden_context_xml: Some(XmlBlock::parse(text)?),
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: PendingUserMessageMode::Steer,
        };
        self.runtime_service
            .enqueue(session_id, message, PendingUserMessageMode::Steer)
            .await?;
        self.wake(session_id);
        Ok(())
    }
}

/// A live agent with its session's live `tasks.md`.
#[derive(Debug, Clone)]
pub struct AgentTasks {
    /// The agent's status snapshot.
    pub status: AgentStatus,
    /// The repository the session is bound to, if it is.
    pub binding: Option<crate::session::RepoBinding>,
    /// The session's `workspace.toml`.
    pub workspace_config: PathBuf,
    /// The session's `tasks.md`.
    pub tasks_path: PathBuf,
    /// Contents of `tasks.md`; `None` while the file is absent.
    pub tasks: Option<String>,
}

async fn status_name(
    runtime_service: &SessionRuntimeService,
    session_id: &str,
) -> Result<String, AgentLinkError> {
    Ok(runtime_service
        .db()
        .get_session(session_id)
        .await?
        .and_then(|session| session.title)
        .unwrap_or_else(|| session_id.to_string()))
}

/// The list items of the `## Roots` section of a `tasks.md`.
fn parse_roots(tasks: &str) -> Vec<String> {
    tasks
        .lines()
        .skip_while(|line| line.trim() != ROOTS_HEADING)
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.trim().strip_prefix("- "))
        .map(|root| root.trim().to_string())
        .filter(|root| !root.is_empty())
        .collect()
}

/// What the `agent` command needs from the hosting session.
#[derive(Debug, Clone)]
pub struct AgentMessaging {
    /// The daemon's session directory.
    pub directory: AgentDirectory,
    /// Queue and runtime state of the shared `history.db`.
    pub runtime_service: Arc<SessionRuntimeService>,
    /// How long a provider id keeps the main conversation's cache warm.
    pub cache_lifetime: CacheLifetimes,
    /// The fork source and model of every session agent, for `btw`.
    pub forks: ForkHosts,
}

/// Resolved local or remote agent recipient.
#[derive(Debug, Clone)]
pub(crate) enum ResolvedRecipient {
    Local(AgentStatus),
    Remote(RemoteAgentEntry),
}

#[derive(Template)]
#[template(path = "agent_ambiguous.txt", escape = "none")]
struct AmbiguousAgentText<'a> {
    target: &'a str,
    candidates: &'a [String],
}

fn ambiguous_agent_error(target: &str, candidates: &[String]) -> AgentLinkError {
    let rendered = AmbiguousAgentText { target, candidates }
        .render()
        .unwrap_or_default()
        .trim_end()
        .to_string();
    AgentLinkError::AmbiguousAgent(rendered)
}

impl AgentMessaging {
    /// Whether `session_id` is idle with a cold prompt cache, so that waking
    /// it would pay for its whole prompt again. A working session is never
    /// cold.
    ///
    /// # Errors
    /// [`AgentLinkError`] on storage failure.
    pub async fn is_cold(
        &self,
        session_id: &str,
        activity: Activity,
    ) -> Result<bool, AgentLinkError> {
        if activity == Activity::Running {
            return Ok(false);
        }
        Ok(self
            .runtime_service
            .db()
            .prompt_cache_is_cold(session_id, jiff::Timestamp::now(), |provider| {
                self.cache_lifetime.of(provider)
            })
            .await?)
    }
}

/// Tag names that frame text for the recipient; text from another agent must
/// not be able to open or close them.
const FRAMING_TAGS: [&str; 6] = [
    "system-reminder",
    "agent-message",
    "agent-btw",
    "task-message",
    "board-message",
    "board-notice",
];

/// Text written by another agent, escaped for display.
///
/// Displaying it turns the `<` of any opening
/// or closing `system-reminder` or `agent-message` tag into `&lt;`, so it can
/// neither close its own block nor open a harness one; all other text,
/// code included, is verbatim.
#[derive(Debug, Clone, Copy)]
pub struct UntrustedText<'a>(pub &'a str);

impl std::fmt::Display for UntrustedText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rest = self.0;
        while let Some(at) = rest.find('<') {
            let (before, tag) = rest.split_at(at);
            f.write_str(before)?;
            let name = tag[1..].strip_prefix('/').unwrap_or_else(|| &tag[1..]);
            let framing = FRAMING_TAGS.iter().any(|framing| {
                name.get(..framing.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(framing))
            });
            f.write_str(if framing { "&lt;" } else { "<" })?;
            rest = &tag[1..];
        }
        f.write_str(rest)
    }
}

/// A session name rendered safe inside a double-quoted attribute.
#[derive(Debug, Clone, Copy)]
pub struct AttributeText<'a>(pub &'a str);

impl std::fmt::Display for AttributeText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ch in self.0.chars() {
            match ch {
                '&' => f.write_str("&amp;")?,
                '<' => f.write_str("&lt;")?,
                '>' => f.write_str("&gt;")?,
                '"' => f.write_str("&quot;")?,
                '\n' | '\r' => f.write_str(" ")?,
                other => write!(f, "{other}")?,
            }
        }
        Ok(())
    }
}

#[derive(Template)]
#[template(path = "agent_message.txt", escape = "none")]
struct AgentMessageText<'a> {
    sender: AttributeText<'a>,
    sender_id: AttributeText<'a>,
    body: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "agent_cold.txt", escape = "none")]
struct ColdRecipientText<'a> {
    recipient: &'a str,
}

fn cold_recipient_message(recipient: &str) -> String {
    ColdRecipientText { recipient }
        .render()
        .expect("cold recipient template must render")
}

#[derive(Template)]
#[template(path = "agent_btw_cold.txt", escape = "none")]
struct ColdBtwRecipientText<'a> {
    recipient: &'a str,
}

fn cold_btw_recipient_message(recipient: &str) -> String {
    ColdBtwRecipientText { recipient }
        .render()
        .expect("cold btw recipient template must render")
}

/// How long each provider keeps the main conversation's prompt cache warm,
/// read live from the daemon's configuration.
#[derive(Clone)]
pub struct CacheLifetimes(Arc<dyn Fn(&str) -> CacheLifetime + Send + Sync>);

impl CacheLifetimes {
    /// Lifetimes looked up by provider id through `lookup`.
    #[must_use]
    pub fn new(lookup: impl Fn(&str) -> CacheLifetime + Send + Sync + 'static) -> Self {
        Self(Arc::new(lookup))
    }

    fn of(&self, provider_id: &str) -> CacheLifetime {
        (self.0)(provider_id)
    }
}

impl std::fmt::Debug for CacheLifetimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheLifetimes").finish_non_exhaustive()
    }
}

#[derive(Template)]
#[template(path = "agent_sent.txt", escape = "none")]
struct AgentSentText<'a> {
    recipient: &'a str,
}

struct ListedAgent {
    name: String,
    address: String,
    state: String,
    cwd: String,
    roots: Vec<String>,
    is_self: bool,
}

#[derive(Template)]
#[template(path = "agent_list.txt", escape = "none")]
struct AgentListText {
    agents: Vec<ListedAgent>,
    issues: Vec<PeerNodeIssue>,
}

impl AgentMessaging {
    /// Resolves `target` (`<machine>:<session-id>`, bare `<session-id>`, or
    /// unique `<title>`) across local and reachable peer/cloud sessions.
    pub(crate) async fn resolve_recipient(
        &self,
        sender_session_id: &str,
        target: &str,
    ) -> Result<(ResolvedRecipient, String), AgentLinkError> {
        let Some(peers) = self.directory.peers() else {
            let live = self.directory.live().await?;
            let matches: Vec<&AgentStatus> = live
                .iter()
                .filter(|agent| agent.session_id == target || agent.name == target)
                .collect();
            return match matches.as_slice() {
                [] => Err(AgentLinkError::NoSuchAgent(target.to_string())),
                [single] => Ok((ResolvedRecipient::Local((*single).clone()), String::new())),
                many => {
                    let candidates: Vec<String> =
                        many.iter().map(|a| a.session_id.clone()).collect();
                    Err(ambiguous_agent_error(target, &candidates))
                }
            };
        };

        let (local_res, remote) =
            futures_lite::future::zip(self.directory.live(), peers.list_remote(sender_session_id))
                .await;
        let local = local_res?;
        let local_machine = if remote.local_machine.is_empty() {
            peers.local_machine()
        } else {
            remote.local_machine
        };

        if let Some((machine, bare_id)) = split_qualified_target(target) {
            if machine == local_machine {
                let found = local
                    .into_iter()
                    .find(|agent| agent.session_id == bare_id)
                    .ok_or_else(|| AgentLinkError::NoSuchAgent(target.to_string()))?;
                return Ok((ResolvedRecipient::Local(found), local_machine));
            }
            if let Some(found) = remote
                .agents
                .into_iter()
                .find(|entry| entry.machine == machine && entry.status.session_id == bare_id)
            {
                return Ok((ResolvedRecipient::Remote(found), local_machine));
            }
            if let Some(issue) = remote.issues.iter().find(|issue| issue.target == machine) {
                return Err(AgentLinkError::PeerUnavailable {
                    machine: machine.to_string(),
                    reason: issue
                        .detail
                        .clone()
                        .unwrap_or_else(|| issue.state.to_string()),
                });
            }
            return Err(AgentLinkError::NoSuchAgent(target.to_string()));
        }

        // Exact bare session_id matches take precedence over display-title collisions,
        // unless multiple machines share the same bare id.
        let mut matches: Vec<(String, ResolvedRecipient)> = Vec::new();
        for agent in local {
            if agent.session_id == target || agent.name == target {
                matches.push((
                    format!("{local_machine}:{}", agent.session_id),
                    ResolvedRecipient::Local(agent),
                ));
            }
        }
        for entry in remote.agents {
            if entry.status.session_id == target || entry.status.name == target {
                matches.push((entry.qualified_id(), ResolvedRecipient::Remote(entry)));
            }
        }

        if matches.is_empty() {
            return Err(AgentLinkError::NoSuchAgent(target.to_string()));
        }
        if matches.len() == 1 {
            let (_, resolved) = matches.swap_remove(0);
            return Ok((resolved, local_machine));
        }

        // If `target` uniquely matches a bare `session_id` (while another session merely has `target` as title),
        // keep exact id semantics only when there is a single bare-id match.
        let id_only: Vec<(String, ResolvedRecipient)> = matches
            .iter()
            .filter(|(_, r)| match r {
                ResolvedRecipient::Local(a) => a.session_id == target,
                ResolvedRecipient::Remote(e) => e.status.session_id == target,
            })
            .cloned()
            .collect();
        if id_only.len() == 1 {
            let (_, resolved) = id_only.into_iter().next().unwrap_or_else(|| unreachable!());
            return Ok((resolved, local_machine));
        }

        let mut candidates: Vec<String> = matches.into_iter().map(|(addr, _)| addr).collect();
        candidates.sort();
        candidates.dedup();
        Err(ambiguous_agent_error(target, &candidates))
    }

    /// Store `body` in the queue of the live agent addressed by `target` (name,
    /// session id, or `<machine>:<session-id>`) and wake it; an idle recipient
    /// whose prompt cache is cold (or whose codespace is suspended) is refused
    /// and nothing is stored. Returns the recipient's name.
    ///
    /// # Errors
    /// See [`AgentLinkError`].
    ///
    /// # Panics
    /// When the compile-time message template fails to render.
    pub async fn send(
        &self,
        sender_session_id: &str,
        target: &str,
        body: &str,
    ) -> Result<String, AgentLinkError> {
        let (resolved, local_machine) = self.resolve_recipient(sender_session_id, target).await?;
        match resolved {
            ResolvedRecipient::Local(recipient) => {
                if recipient.session_id == sender_session_id {
                    return Err(AgentLinkError::SendToSelf);
                }
                let recipient_cold = self
                    .is_cold(&recipient.session_id, recipient.activity)
                    .await?;
                if crate::agent::wake::decide(crate::agent::wake::WakeCause::AgentMessage {
                    recipient_cold,
                }) == crate::agent::wake::WakeDecision::RefuseColdRecipient
                {
                    return Err(AgentLinkError::ColdRecipient(recipient.name.clone()));
                }
                let sender = status_name(&self.runtime_service, sender_session_id).await?;
                let text = AgentMessageText {
                    sender: AttributeText(&sender),
                    sender_id: AttributeText(sender_session_id),
                    body: UntrustedText(body),
                }
                .render()
                .expect("agent message template must render");
                self.directory.deliver(&recipient.session_id, text).await?;
                Ok(recipient.name)
            }
            ResolvedRecipient::Remote(entry) => {
                if entry.suspended {
                    return Err(AgentLinkError::ColdRecipient(entry.status.name));
                }
                let Some(peers) = self.directory.peers() else {
                    return Err(AgentLinkError::NoSuchAgent(target.to_string()));
                };
                let sender_name = status_name(&self.runtime_service, sender_session_id).await?;
                let sender_address = format!("{local_machine}:{sender_session_id}");
                peers
                    .send_remote(
                        &entry.machine,
                        &entry.status.session_id,
                        PeerSendRequest {
                            sender_name,
                            sender_address,
                            body: body.to_string(),
                        },
                    )
                    .await
            }
        }
    }

    /// Delivers a cross-machine message arriving from a peer daemon onto local
    /// `target_session_id`, enforcing this host's cold-cache refusal boundary.
    ///
    /// # Errors
    /// Returns [`AgentLinkError`] when the target is absent, archived, or cold.
    pub async fn deliver_from_peer(
        &self,
        target_session_id: &str,
        request: PeerSendRequest,
    ) -> Result<String, AgentLinkError> {
        let live = self.directory.live().await?;
        let recipient = live
            .into_iter()
            .find(|agent| agent.session_id == target_session_id)
            .ok_or_else(|| AgentLinkError::NoSuchAgent(target_session_id.to_string()))?;
        let recipient_cold = self
            .is_cold(&recipient.session_id, recipient.activity)
            .await?;
        if crate::agent::wake::decide(crate::agent::wake::WakeCause::AgentMessage {
            recipient_cold,
        }) == crate::agent::wake::WakeDecision::RefuseColdRecipient
        {
            return Err(AgentLinkError::ColdRecipient(recipient.name));
        }
        let text = AgentMessageText {
            sender: AttributeText(&request.sender_name),
            sender_id: AttributeText(&request.sender_address),
            body: UntrustedText(&request.body),
        }
        .render()
        .map_err(|e| AgentLinkError::Fork(e.to_string()))?;
        self.directory.deliver(&recipient.session_id, text).await?;
        Ok(recipient.name)
    }
}

/// See which other main agents run on this machine and message them.
///
/// Subcommands:
/// - `agent list`
/// - `agent send <name> <prompt-file>` stores the file's text as a message to the
///   agent named `<name>` (or addressed by its id) and returns at once; the agent
///   receives it at its next turn boundary, and an idle agent is woken by it. A
///   reply is a message sent back the same way.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentArgs {
    /// List the live agents with their working directory, state and roots.
    List,
    /// Send the text of a file to another agent.
    Send {
        /// Name or id of the recipient, as `agent list` shows it.
        name: String,
        /// File holding the message, as the terminal spells its path.
        prompt_file: String,
    },
}

/// The `agent` terminal command; reserved for the main agent.
#[derive(Debug, Clone)]
pub struct AgentTool {
    messaging: AgentMessaging,
    session_id: String,
    sandbox_dir: PathBuf,
    /// Where the sandbox appears inside the session's container, if it has one.
    workspace_mount: Option<aither_sandbox::MountRoot>,
    languages: Option<crate::harness::language::LanguageGuard>,
}

fn normalize_relative_segments(
    relative: &Path,
    prompt_file: &str,
) -> Result<PathBuf, AgentLinkError> {
    let outside = || AgentLinkError::OutsideSandbox(prompt_file.to_string());
    let mut cleaned = PathBuf::new();
    for part in relative.components() {
        match part {
            std::path::Component::Normal(segment) => cleaned.push(segment),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return Err(outside()),
        }
    }
    if cleaned.as_os_str().is_empty() {
        return Err(outside());
    }
    Ok(cleaned)
}

fn symlink_target_escapes_lexically(target: &Path) -> bool {
    target.is_absolute()
        || target.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

/// The host file behind a path as the session's terminal spells it.
///
/// Both local and container roots are canonicalized on the host and every
/// path component is checked so symlinks inside the workspace cannot escape
/// to host files.
///
/// # Errors
/// Fails when the path cannot be resolved inside the sandbox.
pub fn resolve_prompt_path(
    sandbox_dir: &Path,
    mount: Option<&aither_sandbox::MountRoot>,
    prompt_file: &str,
) -> Result<PathBuf, AgentLinkError> {
    let outside = || AgentLinkError::OutsideSandbox(prompt_file.to_string());
    let root_dir = mount.map_or(sandbox_dir, |m| m.host_root.as_path());
    let requested = Path::new(prompt_file);
    let canonical_root = std::fs::canonicalize(root_dir).map_err(|source| {
        if mount.is_some()
            || requested.is_absolute()
            || requested
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            outside()
        } else {
            AgentLinkError::PromptFile {
                path: root_dir.join(requested),
                source,
            }
        }
    })?;

    let raw_relative = if let Some(mount) = mount {
        if requested.is_absolute() {
            requested
                .strip_prefix(mount.container_root.as_str())
                .map_err(|_| outside())?
                .to_path_buf()
        } else {
            requested.to_path_buf()
        }
    } else if requested.is_absolute() {
        if let Ok(rel) = requested.strip_prefix(sandbox_dir) {
            rel.to_path_buf()
        } else if let Ok(rel) = requested.strip_prefix(&canonical_root) {
            rel.to_path_buf()
        } else if let (Some(parent), Some(file_name)) = (requested.parent(), requested.file_name())
            && let Ok(canonical_parent) = std::fs::canonicalize(parent)
            && let Ok(parent_rel) = canonical_parent.strip_prefix(&canonical_root)
        {
            parent_rel.join(file_name)
        } else {
            return Err(outside());
        }
    } else {
        requested.to_path_buf()
    };

    let relative = normalize_relative_segments(&raw_relative, prompt_file)?;
    let mut current = canonical_root.clone();
    for part in relative.components() {
        let std::path::Component::Normal(segment) = part else {
            return Err(outside());
        };
        current.push(segment);
        if let Ok(meta) = std::fs::symlink_metadata(&current)
            && meta.file_type().is_symlink()
        {
            if let Ok(resolved) = std::fs::canonicalize(&current) {
                if !resolved.starts_with(&canonical_root) {
                    return Err(outside());
                }
                current = resolved;
            } else {
                let link_target = std::fs::read_link(&current).map_err(|_| outside())?;
                if symlink_target_escapes_lexically(&link_target) {
                    return Err(outside());
                }
            }
        }
    }

    if let Ok(resolved) = std::fs::canonicalize(&current) {
        if !resolved.starts_with(&canonical_root) {
            return Err(outside());
        }
        return Ok(resolved);
    }
    if !current.starts_with(&canonical_root) {
        return Err(outside());
    }
    Ok(current)
}

/// Async counterpart of [`resolve_prompt_path`] using non-blocking filesystem metadata.
///
/// # Errors
/// Fails when the path cannot be resolved inside the sandbox.
pub async fn resolve_prompt_path_async(
    sandbox_dir: &Path,
    mount: Option<&aither_sandbox::MountRoot>,
    prompt_file: &str,
) -> Result<PathBuf, AgentLinkError> {
    let outside = || AgentLinkError::OutsideSandbox(prompt_file.to_string());
    let root_dir = mount.map_or(sandbox_dir, |m| m.host_root.as_path());
    let requested = Path::new(prompt_file);
    let canonical_root = async_fs::canonicalize(root_dir).await.map_err(|source| {
        if mount.is_some()
            || requested.is_absolute()
            || requested
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            outside()
        } else {
            AgentLinkError::PromptFile {
                path: root_dir.join(requested),
                source,
            }
        }
    })?;

    let raw_relative = if let Some(mount) = mount {
        if requested.is_absolute() {
            requested
                .strip_prefix(mount.container_root.as_str())
                .map_err(|_| outside())?
                .to_path_buf()
        } else {
            requested.to_path_buf()
        }
    } else if requested.is_absolute() {
        if let Ok(rel) = requested.strip_prefix(sandbox_dir) {
            rel.to_path_buf()
        } else if let Ok(rel) = requested.strip_prefix(&canonical_root) {
            rel.to_path_buf()
        } else if let (Some(parent), Some(file_name)) = (requested.parent(), requested.file_name())
            && let Ok(canonical_parent) = async_fs::canonicalize(parent).await
            && let Ok(parent_rel) = canonical_parent.strip_prefix(&canonical_root)
        {
            parent_rel.join(file_name)
        } else {
            return Err(outside());
        }
    } else {
        requested.to_path_buf()
    };

    let relative = normalize_relative_segments(&raw_relative, prompt_file)?;
    let mut current = canonical_root.clone();
    for part in relative.components() {
        let std::path::Component::Normal(segment) = part else {
            return Err(outside());
        };
        current.push(segment);
        if let Ok(meta) = async_fs::symlink_metadata(&current).await
            && meta.file_type().is_symlink()
        {
            if let Ok(resolved) = async_fs::canonicalize(&current).await {
                if !resolved.starts_with(&canonical_root) {
                    return Err(outside());
                }
                current = resolved;
            } else {
                let link_target = async_fs::read_link(&current).await.map_err(|_| outside())?;
                if symlink_target_escapes_lexically(&link_target) {
                    return Err(outside());
                }
            }
        }
    }

    if let Ok(resolved) = async_fs::canonicalize(&current).await {
        if !resolved.starts_with(&canonical_root) {
            return Err(outside());
        }
        return Ok(resolved);
    }
    if !current.starts_with(&canonical_root) {
        return Err(outside());
    }
    Ok(current)
}

impl AgentTool {
    /// The command for the session `session_id` whose sandbox is `sandbox_dir`.
    #[must_use]
    pub const fn new(
        messaging: AgentMessaging,
        session_id: String,
        sandbox_dir: PathBuf,
        workspace_mount: Option<aither_sandbox::MountRoot>,
    ) -> Self {
        Self {
            messaging,
            session_id,
            sandbox_dir,
            workspace_mount,
            languages: None,
        }
    }

    /// Refuse a message outside the working language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: crate::harness::language::LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
    }
}

impl Tool for AgentTool {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("agent")
    }

    type Arguments = AgentArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let text = match args {
            AgentArgs::List => {
                let (local_res, remote) = if let Some(peers) = self.messaging.directory.peers() {
                    let (local, remote) = futures_lite::future::zip(
                        self.messaging.directory.live(),
                        peers.list_remote(&self.session_id),
                    )
                    .await;
                    (local, Some(remote))
                } else {
                    (self.messaging.directory.live().await, None)
                };
                let mut agents: Vec<ListedAgent> = local_res?
                    .into_iter()
                    .map(|status| ListedAgent {
                        cwd: crate::fs_utils::display_path(&status.cwd),
                        is_self: status.session_id == self.session_id,
                        name: status.name,
                        address: status.session_id,
                        state: status.activity.to_string(),
                        roots: status.roots,
                    })
                    .collect();
                let issues = if let Some(remote) = remote {
                    for entry in remote.agents {
                        let address = entry.qualified_id();
                        let location = entry.repo.as_deref().map_or_else(
                            || crate::fs_utils::display_path(&entry.status.cwd),
                            |repo| {
                                format!(
                                    "{} ({repo})",
                                    crate::fs_utils::display_path(&entry.status.cwd)
                                )
                            },
                        );
                        agents.push(ListedAgent {
                            name: entry.status.name,
                            address,
                            state: if entry.suspended {
                                "suspended".to_string()
                            } else {
                                entry.status.activity.to_string()
                            },
                            cwd: location,
                            roots: entry.status.roots,
                            is_self: false,
                        });
                    }
                    remote.issues
                } else {
                    Vec::new()
                };
                AgentListText { agents, issues }
                    .render()
                    .expect("agent list template must render")
            }
            AgentArgs::Send { name, prompt_file } => {
                let path = resolve_prompt_path_async(
                    &self.sandbox_dir,
                    self.workspace_mount.as_ref(),
                    &prompt_file,
                )
                .await?;
                let body = async_fs::read_to_string(&path).await.map_err(|source| {
                    AgentLinkError::PromptFile {
                        path: path.clone(),
                        source,
                    }
                })?;
                if body.trim().is_empty() {
                    return Err(AgentLinkError::EmptyMessage(path).into());
                }
                if let Some(guard) = &self.languages {
                    guard
                        .check_working(body.trim(), crate::harness::language::WorkingText::Internal)
                        .map_err(crate::agent::notices::language_refused)?;
                }
                let recipient = self
                    .messaging
                    .send(&self.session_id, &name, body.trim())
                    .await?;
                AgentSentText {
                    recipient: &recipient,
                }
                .render()
                .expect("agent sent template must render")
            }
        };
        Ok(ToolResult::text(text))
    }
}

mod btw;
pub mod peers;

pub use btw::{
    BtwAnswer, BtwArgs, BtwCommand, BtwTool, ForkHosts, ForkLauncher, ForkRegistration, SideContext,
};
pub use peers::{
    AnyPeerFabric, CLOUD_MACHINE, PeerAgentsResponse, PeerBtwRequest, PeerBtwResponse,
    PeerErrorBody, PeerErrorKind, PeerFabric, PeerNodeIssue, PeerNodeState, PeerSendRequest,
    PeerSendResponse, RemoteAgentEntry, RemoteListing, sanitize_machine_name,
    split_qualified_target,
};

#[cfg(test)]
mod tests;
