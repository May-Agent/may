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
use std::sync::{Arc, Mutex};

use aither_agent::AgentOptions as _;
use aither_core::llm::{AgentIdentity, SubagentIdentity, Tool, ToolContext, ToolResult};
use aither_sandbox::{
    AnyCommandGate, CommandGate, GateDecision, SubagentGrant, TerminalBinding, TerminalToolFactory,
};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{AgentLinkError, AgentMessaging, AttributeText, UntrustedText, status_name};
use crate::agent::ProviderLlm;

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
    llm: ProviderLlm,
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
        llm: ProviderLlm,
    ) -> ForkRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.hosts()
            .insert(session_id.to_string(), ForkHost { id, source, llm });
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
        let mut fork = aither_agent::AgentBuilder::new(host.llm)
            .forked_from(&snapshot, terminal)
            .identity(AgentIdentity::Subagent(SubagentIdentity::new(
                BTW_FORK, None,
            )))
            .build();
        let answer = fork.query(&prompt).await;
        drop(fork);
        // The fork's space held nothing anyone reads again.
        if let Err(error) = async_fs::remove_dir_all(&space).await {
            tracing::warn!(space = %space.display(), %error, "failed to remove a btw fork's space");
        }
        Ok(BtwAnswer {
            recipient: recipient.name.clone(),
            recipient_id: recipient.session_id.clone(),
            answer: answer.map_err(|error| AgentLinkError::Fork(error.to_string()))?,
        })
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
    languages: Option<aither_agent::LanguageGuard>,
}

impl BtwCommand {
    /// The command for the session `session_id`.
    #[must_use]
    pub const fn new(messaging: AgentMessaging, session_id: String) -> Self {
        Self {
            messaging,
            session_id,
            languages: None,
        }
    }

    /// Refuse a question outside the working language `guard` enforces.
    #[must_use]
    pub fn with_languages(mut self, guard: aither_agent::LanguageGuard) -> Self {
        self.languages = Some(guard);
        self
    }

    /// The command as `binding`'s terminal serves it: a fork's terminal is
    /// launched from that terminal.
    #[must_use]
    pub fn for_terminal(&self, binding: &TerminalBinding) -> BtwTool {
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
            guard.check_working(question, aither_agent::WorkingText::Internal)?;
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
