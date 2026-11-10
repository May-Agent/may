//! `btw`: a main agent asks another a side question and gets the answer at
//! once.
//!
//! Every main agent publishes each request it sends to its
//! [`aither_agent::ForkSource`], whatever it is doing; [`ForkHosts`] holds
//! each live session's source with its model. A question is answered by an
//! agent built from the recipient's latest request with
//! [`aither_agent::AgentBuilder::forked_from`], so it starts from exactly
//! what the recipient's context holds, reads the recipient's warm prompt
//! cache, and never touches the recipient's own agent: a running turn keeps
//! running, and nothing the fork does reaches the recipient's context or
//! history. The fork's terminal is gated to refuse every command, so it
//! answers from its context and changes nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use aither_agent::AgentOptions as _;
use aither_core::llm::{AgentIdentity, Message, SubagentIdentity, Tool, ToolContext, ToolResult};
use aither_sandbox::{
    AnyCommandGate, CommandGate, GateDecision, SubagentGrant, TerminalBinding, TerminalToolFactory,
};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Activity, AgentLinkError, AgentMessaging, AttributeText, UntrustedText, status_name};
use crate::agent::{ModelSlot, ProviderLlm};

/// The subagent name a btw fork's tool calls carry.
const BTW_FORK: &str = "btw";

/// What a session's agent offers the forks that answer its btw questions.
#[derive(Clone)]
struct ForkHost {
    /// Tells this registration from the one of an agent that replaced it.
    id: u64,
    source: aither_agent::ForkSource,
    /// The session's own model; the fork's request carries the parameters
    /// and prompt-cache identity of the request it forks.
    model: ModelSlot<ProviderLlm>,
    /// The terminal the session's agent launches forks' terminals from.
    launcher: ForkLauncher,
    /// The session's communication and working languages.
    languages: crate::harness::language::Languages,
}

/// The terminal launcher of a session's main agent, filled when the agent's
/// terminal is built.
#[derive(Debug, Clone, Default)]
pub struct ForkLauncher(Arc<OnceLock<TerminalToolFactory>>);

impl ForkLauncher {
    fn set(&self, launcher: &TerminalToolFactory) {
        // A terminal bound later belongs to the same agent; the first
        // launcher stays.
        let _ = self.0.set(launcher.clone());
    }
}

/// What a fork does with its context.
enum ForkTask<'a> {
    /// Answers `prompt`.
    Answer(&'a str),
    /// Reads the messages its next request would carry.
    ReadContext,
}

enum ForkOutcome {
    Answer(String),
    Context {
        messages: Vec<Message>,
        cache_epoch: u32,
    },
}

impl ForkHost {
    /// Runs `task` on a read-only fork of `snapshot` whose terminal is
    /// launched from `launcher`, then removes the fork's space.
    async fn run_fork(
        &self,
        snapshot: &aither_agent::ForkSnapshot,
        launcher: &TerminalToolFactory,
        task: ForkTask<'_>,
    ) -> Result<ForkOutcome, AgentLinkError> {
        let terminal = launcher
            .create(
                SubagentGrant::new(),
                None,
                Some(AnyCommandGate::new(ReadOnlyFork)),
                None,
            )
            .await
            .map_err(|error| AgentLinkError::Fork(error.to_string()))?;
        let space = terminal.working_dir().clone();
        let mut fork = aither_agent::AgentBuilder::new(self.model.llm.clone())
            .forked_from(snapshot, terminal)
            .identity(AgentIdentity::Subagent(SubagentIdentity::new(
                BTW_FORK, None,
            )))
            .build();
        let outcome = match task {
            ForkTask::Answer(prompt) => fork.query(prompt).await.map(ForkOutcome::Answer),
            ForkTask::ReadContext => {
                fork.snapshot_context_window()
                    .await
                    .map(|window| ForkOutcome::Context {
                        messages: window.messages,
                        cache_epoch: window.metrics.prompt_cache_epoch,
                    })
            }
        };
        drop(fork);
        // The fork's space held nothing anyone reads again.
        if let Err(error) = async_fs::remove_dir_all(&space).await {
            tracing::warn!(space = %space.display(), %error, "failed to remove a btw fork's space");
        }
        outcome.map_err(|error| AgentLinkError::Fork(error.to_string()))
    }
}

/// A live session agent's context for a side request, read from a fork.
#[derive(Debug)]
pub struct SideContext {
    /// The messages the agent's next request would carry.
    pub messages: Vec<Message>,
    /// The agent's model under the prompt-cache key its requests carry.
    pub model: ModelSlot<ProviderLlm>,
    /// The agent's communication and working languages.
    pub languages: crate::harness::language::Languages,
}

/// The fork source and model of every session agent the daemon hosts.
#[derive(Clone, Default)]
pub struct ForkHosts {
    hosts: Arc<Mutex<HashMap<String, ForkHost>>>,
    next_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for ForkHosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkHosts").finish_non_exhaustive()
    }
}

impl ForkHosts {
    fn hosts(&self) -> std::sync::MutexGuard<'_, HashMap<String, ForkHost>> {
        self.hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Makes the agent publishing to `source` on `llm` the one answering
    /// `session_id`'s btw questions, until the returned registration drops
    /// or a later agent of the session registers.
    #[must_use]
    pub fn register(
        &self,
        session_id: &str,
        source: aither_agent::ForkSource,
        model: ModelSlot<ProviderLlm>,
        launcher: ForkLauncher,
        languages: crate::harness::language::Languages,
    ) -> ForkRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.hosts().insert(
            session_id.to_string(),
            ForkHost {
                id,
                source,
                model,
                launcher,
                languages,
            },
        );
        ForkRegistration {
            hosts: self.clone(),
            session_id: session_id.to_string(),
            id,
        }
    }

    fn get(&self, session_id: &str) -> Option<ForkHost> {
        self.hosts().get(session_id).cloned()
    }
}

/// A session agent's entry in [`ForkHosts`], removed when the agent drops.
pub struct ForkRegistration {
    hosts: ForkHosts,
    session_id: String,
    id: u64,
}

impl std::fmt::Debug for ForkRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkRegistration")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl Drop for ForkRegistration {
    fn drop(&mut self) {
        let mut hosts = self.hosts.hosts();
        if hosts
            .get(&self.session_id)
            .is_some_and(|host| host.id == self.id)
        {
            hosts.remove(&self.session_id);
        }
    }
}

#[derive(Template)]
#[template(path = "agent_btw_refused.txt", escape = "none")]
struct ForkCommandRefused;

/// Refuses every command of a btw fork's terminal: the fork answers from
/// its context and runs nothing.
struct ReadOnlyFork;

impl CommandGate for ReadOnlyFork {
    fn check(&self, _command: &str) -> impl Future<Output = GateDecision> + Send {
        std::future::ready(GateDecision::Block {
            reason: ForkCommandRefused
                .render()
                .expect("btw refusal template must render"),
        })
    }
}

#[derive(Template)]
#[template(path = "agent_btw_question.txt", escape = "none")]
struct BtwQuestionText<'a> {
    sender: AttributeText<'a>,
    sender_id: AttributeText<'a>,
    question: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "agent_btw_answer.txt", escape = "none")]
struct BtwAnswerText<'a> {
    recipient: AttributeText<'a>,
    recipient_id: AttributeText<'a>,
    answer: UntrustedText<'a>,
}

/// The answer to a btw question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwAnswer {
    /// The recipient's name.
    pub recipient: String,
    /// The recipient's session id.
    pub recipient_id: String,
    /// What the fork answered.
    pub answer: String,
}

impl AgentMessaging {
    /// Answers `question` from a read-only fork of the context of the live
    /// agent addressed by `target`, made from the request it sent last,
    /// whether it is working or idle. The fork's terminal comes from
    /// `launcher`, the asking agent's, and refuses every command; the fork
    /// is discarded once it answers. An idle recipient whose prompt cache is
    /// cold is refused before any model call.
    ///
    /// # Errors
    /// See [`AgentLinkError`].
    ///
    /// # Panics
    /// When the compile-time question template fails to render.
    pub async fn btw(
        &self,
        sender_session_id: &str,
        target: &str,
        question: &str,
        launcher: &TerminalToolFactory,
    ) -> Result<BtwAnswer, AgentLinkError> {
        let live = self.directory.live().await?;
        let recipient = super::addressed(&live, target)?;
        if recipient.session_id == sender_session_id {
            return Err(AgentLinkError::AskSelf);
        }
        if self
            .is_cold(&recipient.session_id, recipient.activity)
            .await?
        {
            return Err(AgentLinkError::ColdBtwRecipient(recipient.name.clone()));
        }
        let host = self
            .forks
            .get(&recipient.session_id)
            .ok_or_else(|| AgentLinkError::NoForkContext(recipient.name.clone()))?;
        let snapshot = host
            .source
            .latest()
            .ok_or_else(|| AgentLinkError::NoForkContext(recipient.name.clone()))?;
        if !snapshot.allows_fork() {
            return Err(AgentLinkError::NearCompaction(recipient.name.clone()));
        }
        let sender = status_name(&self.runtime_service, sender_session_id).await?;
        let prompt = BtwQuestionText {
            sender: AttributeText(&sender),
            sender_id: AttributeText(sender_session_id),
            question: UntrustedText(question),
        }
        .render()
        .expect("btw question template must render");

        let ForkOutcome::Answer(answer) = host
            .run_fork(&snapshot, launcher, ForkTask::Answer(&prompt))
            .await?
        else {
            unreachable!("an answering fork gives an answer");
        };
        Ok(BtwAnswer {
            recipient: recipient.name.clone(),
            recipient_id: recipient.session_id.clone(),
            answer,
        })
    }

    /// The context of `session_id`'s live agent for a side request (`/btw`,
    /// `/progress`), read from a read-only fork of the request it sent last,
    /// so it holds what the agent's context holds now, working or idle,
    /// without touching the agent. `None` when the session has no live
    /// agent that has sent a request; the caller then reads the idle agent.
    /// An idle agent with a cold prompt cache, or one about to compact, is
    /// refused.
    ///
    /// # Errors
    /// See [`AgentLinkError`].
    pub async fn side_context(
        &self,
        session_id: &str,
    ) -> Result<Option<SideContext>, AgentLinkError> {
        let Some(host) = self.forks.get(session_id) else {
            return Ok(None);
        };
        let Some(snapshot) = host.source.latest() else {
            return Ok(None);
        };
        let Some(launcher) = host.launcher.0.get().cloned() else {
            return Ok(None);
        };
        let name = status_name(&self.runtime_service, session_id).await?;
        let activity = if self.runtime_service.get(session_id).await?.state.is_idle() {
            Activity::Idle
        } else {
            Activity::Running
        };
        if self.is_cold(session_id, activity).await? {
            return Err(AgentLinkError::ColdBtwRecipient(name));
        }
        if !snapshot.allows_fork() {
            return Err(AgentLinkError::NearCompaction(name));
        }
        let ForkOutcome::Context {
            messages,
            cache_epoch,
        } = host
            .run_fork(&snapshot, &launcher, ForkTask::ReadContext)
            .await?
        else {
            unreachable!("a context-reading fork gives its context");
        };
        Ok(Some(SideContext {
            messages,
            model: crate::agent::side_slot(&host.model, session_id, cache_epoch),
            languages: host.languages,
        }))
    }
}

/// Ask another main agent a side question and get its answer as this
/// command's result.
///
/// The agent named `<name>` (or addressed by its id, as `agent list` shows
/// it) answers from a read-only fork of its current context, while it keeps
/// working; the question never enters its own conversation. An idle agent
/// whose prompt cache has gone cold is not asked.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BtwArgs {
    /// Name or id of the agent to ask, as `agent list` shows it.
    pub name: String,
    /// The question.
    pub question: String,
}

/// Builds the `btw` command for the terminal that serves it.
#[derive(Debug, Clone)]
pub struct BtwCommand {
    messaging: AgentMessaging,
    session_id: String,
    languages: Option<crate::harness::language::LanguageGuard>,
    fork_launcher: Option<ForkLauncher>,
}

impl BtwCommand {
    /// The command for the session `session_id`.
    #[must_use]
    pub const fn new(messaging: AgentMessaging, session_id: String) -> Self {
        Self {
            messaging,
            session_id,
            languages: None,
            fork_launcher: None,
        }
    }

    /// Hand the launcher of the terminal serving this command to
    /// `launcher`, for the forks answering the session's side requests.
    #[must_use]
    pub fn with_fork_launcher(mut self, launcher: ForkLauncher) -> Self {
        self.fork_launcher = Some(launcher);
        self
    }

    /// Refuse a question outside the working language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: crate::harness::language::LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
    }

    /// The command as `binding`'s terminal serves it: a fork's terminal is
    /// launched from that terminal.
    #[must_use]
    pub fn for_terminal(&self, binding: &TerminalBinding) -> BtwTool {
        if let Some(launcher) = &self.fork_launcher {
            launcher.set(binding.launcher());
        }
        BtwTool {
            command: self.clone(),
            launcher: binding.launcher().clone(),
        }
    }
}

/// The `btw` terminal command; reserved for the main agent.
#[derive(Debug, Clone)]
pub struct BtwTool {
    command: BtwCommand,
    launcher: TerminalToolFactory,
}

impl Tool for BtwTool {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("btw")
    }

    type Arguments = BtwArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let question = args.question.trim();
        if question.is_empty() {
            return Err(AgentLinkError::EmptyQuestion.into());
        }
        if let Some(guard) = &self.command.languages {
            guard
                .check_working(question, crate::harness::language::WorkingText::Internal)
                .map_err(crate::agent::notices::language_refused)?;
        }
        let answer = self
            .command
            .messaging
            .btw(
                &self.command.session_id,
                &args.name,
                question,
                &self.launcher,
            )
            .await?;
        Ok(ToolResult::text(
            BtwAnswerText {
                recipient: AttributeText(&answer.recipient),
                recipient_id: AttributeText(&answer.recipient_id),
                answer: UntrustedText(&answer.answer),
            }
            .render()
            .expect("btw answer template must render"),
        ))
    }
}

#[cfg(test)]
mod tests;
