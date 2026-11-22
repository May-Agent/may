//! The worker protocol the TUI and daemon share: events, commands and the
//! handles that exchange them.
//!
//! `S` is the side-model type (`ModelSlot<ProviderLlm>` in-process, `()` for
//! a remote worker), `R` the released agent (`MayAgent` in-process, `()` for
//! a remote worker).

use async_channel::{Receiver, Sender, unbounded};

use crate::session::{CancelReason, SessionRunGuard};
use aither_agent::{CompactResult, TimedEvent};
use aither_core::llm::Attachment;
use may_db::RunFence;

/// Why an in-process agent cannot take a daemon `Send` command.
pub const ERR_SEND_IN_PROCESS: &str = "this agent runs in-process; turns go through `run`";

/// Events sent from the agent background task to the TUI.
#[derive(Debug)]
pub enum TuiEvent<S = ()> {
    /// Agent produced a streaming event.
    Agent(Result<TimedEvent, aither_agent::AgentError>),
    /// Compaction request finished.
    Compacted(Result<Option<CompactResult>, aither_agent::AgentError>),
    /// Context-window measurement for `/context` is ready.
    ContextReport(Result<may_protocol::context_report::ContextReport, aither_agent::AgentError>),
    /// Provider-usage aggregation for `/usage` is ready.
    UsageReport(may_protocol::session_usage::SessionUsageReport),
    /// Assembled-context dump for `/context dump` is ready.
    ContextDump(Result<may_protocol::context_dump::ContextDump, aither_agent::AgentError>),
    /// The agent's effective context, read for a side request.
    ContextMessages(Result<Vec<aither_core::llm::Message>, aither_agent::AgentError>),
    /// The agent's model for a side request, under its prompt-cache key.
    SideModel(Result<Box<S>, aither_agent::AgentError>),
    /// The daemon wrote the `/context dump` to `path`.
    ContextDumpSaved {
        /// Where the dump was written.
        path: String,
        /// Messages in the dump.
        message_count: usize,
    },
    /// A reasoning phase closed after `seconds`, whether or not it produced
    /// readable text. Emitted just before the output that follows it.
    ReasoningEnd {
        /// Seconds the phase lasted. Sub-second phases keep the fraction.
        seconds: f64,
    },
    /// The daemon read a terminal call's script; sent right after the
    /// call's `ToolCallStart`.
    ToolReading {
        /// The call the reading belongs to.
        call_id: String,
        /// How the daemon read the call's script.
        reading: may_protocol::command_reading::CommandReading,
    },
    /// The card of a terminal call that ran one of May's commands changed.
    ToolCard {
        /// The call the card belongs to.
        call_id: String,
        /// The card; absent when the call has none and shows as a command row.
        card: Option<may_protocol::command_reading::Card>,
    },
    /// The call `call_id` ended in a refusal, not a failure — an unchanged
    /// re-read the policy rejected; the task it read stays running. Sent
    /// right after the call's `ToolCallEnded`.
    ToolEnd {
        /// The call the refusal belongs to.
        call_id: String,
        /// Whether the call's result is a policy refusal.
        refused: bool,
    },
    /// The daemon parked the run.
    Hold {
        /// What the run waits for.
        reason: crate::client::HoldReason,
        /// The upstream message behind the wait.
        message: String,
    },
    /// The text streamed so far is discarded: the attempt failed and the
    /// prompt is resubmitted.
    Retract,
    /// The reply just streamed was not in the communication language: show
    /// this translation of it, marked as translated.
    ReplyTranslated {
        /// The reply translated into the communication language.
        text: String,
        /// The communication language's ISO 639-3 code.
        language: String,
    },
    /// The daemon withdrew the turn: it was stopped before any tool call or
    /// response text, so its message left the conversation.
    Withdrawn {
        /// The withdrawn message text.
        content: String,
        /// Whether this client sent it and gets it back in its input.
        own: bool,
    },
    /// The user declined, at the session's repository question, to work
    /// where the session starts: the turn did not run, and nothing failed.
    Declined(String),
    /// The daemon ended the turn because its agent kept looping after the
    /// alert; the message says which loop. The session takes the next
    /// message, but what the turn was asked for was not carried out.
    LoopStopped(String),
    /// Agent stream completed (ready for next prompt).
    Complete,
    /// Agent encountered a fatal error.
    Error(String),
}

/// What a run needs from the lease the TUI acquired for it.
///
/// The TUI holds the run's [`SessionRunGuard`] until it has persisted the
/// run's last message; the worker only carries the pieces the run reacts to.
#[derive(Debug, Clone)]
pub struct RunLease {
    /// Receives a message when the run must stop: a stop request or loss of
    /// the lease.
    pub cancellation: Receiver<CancelReason>,
    /// Fence the run's database commits verify.
    pub fence: RunFence,
}

impl RunLease {
    /// The parts of `guard` a run reacts to.
    #[must_use]
    pub fn of(guard: &SessionRunGuard) -> Self {
        Self {
            cancellation: guard.cancellation().clone(),
            fence: guard.fence().clone(),
        }
    }
}

/// A turn handed to the daemon: the user's message as the TUI composed it.
#[derive(Debug, Clone)]
pub struct RemoteTurn {
    /// Message text.
    pub content: String,
    /// Attachments, as the daemon's API records them.
    pub attachments: Vec<may_protocol::session_runtime::PersistedAttachment>,
    /// Hidden context for the turn.
    pub hidden_context_xml: Option<String>,
}

/// A turn of the session another client or the daemon's queue started.
#[derive(Debug)]
pub struct ForeignTurn<S = (), R = ()> {
    /// Events of the turn.
    pub handle: AgentHandle<S, R>,
    /// The user message the turn answers, when it has one.
    pub content: Option<String>,
    /// Row id of that message in the session history, when the daemon
    /// stored it.
    pub message_id: Option<i32>,
    /// When the daemon started the turn.
    pub started_at: jiff::Timestamp,
}

/// Command sent from main thread to agent worker.
pub enum WorkerCommand<S = (), R = ()> {
    /// Run a prompt.
    Run {
        /// The prompt text.
        prompt: String,
        /// Context prepended the user never sees.
        hidden_context: Option<String>,
        /// Files attached to the prompt.
        attachments: Vec<Attachment>,
        /// The run's lease.
        lease: RunLease,
        /// Where the turn's events go.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Compact conversation history.
    Compact {
        /// What the compaction keeps in focus.
        focus: Option<String>,
        /// Where the turn's events go.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Measure the context window for `/context`.
    ContextReport {
        /// Where the report goes.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Aggregate the session's provider usage for `/usage`.
    UsageReport {
        /// Where the report goes.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Dump the assembled context for `/context dump`.
    ContextDump {
        /// Where the dump goes.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Read the effective context as request messages.
    ContextMessages {
        /// Where the messages go.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Read the model for a side request.
    SideModel {
        /// Where the model goes.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Run a turn in the daemon (remote workers only).
    Send {
        /// The turn to run.
        turn: RemoteTurn,
        /// Where the turn's events go.
        event_tx: Sender<TuiEvent<S>>,
    },
    /// Cancel current processing.
    Cancel,
    /// Hand the idle agent back and stop: the next turn of the session
    /// continues with it.
    Release {
        /// Where the released agent goes.
        reply: Sender<R>,
    },
    /// Shutdown the worker.
    Shutdown,
}

impl<S, R> std::fmt::Debug for WorkerCommand<S, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerCommand").finish_non_exhaustive()
    }
}

/// Handle to control a running agent worker.
pub struct AgentWorker<S = (), R = ()> {
    /// Sender for commands to the worker.
    #[doc(hidden)]
    pub command_tx: Sender<WorkerCommand<S, R>>,
    /// The session's background jobs, shared with its shell; `None` when the
    /// agent runs in the daemon.
    #[doc(hidden)]
    pub jobs: Option<aither_sandbox::Jobs>,
    /// Registry of in-flight `subagent` command runs; `None` when the agent
    /// runs in the daemon.
    #[doc(hidden)]
    pub subagent_runs: Option<aither_agent::SubagentRuns>,
    /// The agent's own language guard; `None` when the agent runs in the
    /// daemon.
    #[doc(hidden)]
    pub languages: Option<may_harness::harness::language::LanguageGuard>,
    /// The agent's prompt-cache entry; `None` when the agent runs in the
    /// daemon.
    #[doc(hidden)]
    pub warmth: Option<aither_agent::CacheWarmth>,
    /// Tracker shared with the session's `subagent` IPC command.
    #[doc(hidden)]
    pub subagent_tracker: crate::subagent_tracker::SubagentTracker,
    /// Turns of the session another client (or the daemon's queue) started,
    /// for a worker whose agent runs in the daemon.
    #[doc(hidden)]
    pub foreign_turns: Option<Receiver<ForeignTurn<S, R>>>,
    /// The session's background work in the daemon, for a remote worker.
    #[doc(hidden)]
    pub background: Option<crate::client::background::RemoteBackground>,
}

impl<S, R> std::fmt::Debug for AgentWorker<S, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentWorker").finish_non_exhaustive()
    }
}

impl<S, R> AgentWorker<S, R> {
    /// Start processing a prompt under `lease`.
    ///
    /// Returns a handle to receive events from this run.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn run(
        &self,
        prompt: String,
        hidden_context: Option<String>,
        attachments: Vec<Attachment>,
        lease: RunLease,
    ) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::Run {
                prompt,
                hidden_context,
                attachments,
                lease,
                event_tx,
            })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Shutdown the worker task.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn compact(&self, focus: Option<String>) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::Compact { focus, event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Request a context-window report for `/context`.
    ///
    /// Issued while a turn is in flight, the measurement is deferred by the
    /// worker and answers with the post-turn context once the turn ends.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn context_report(&self) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::ContextReport { event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Ask for the session's provider-usage aggregation for `/usage`.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn usage_report(&self) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::UsageReport { event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Request a verbatim context dump for `/context dump`.
    ///
    /// Issued while a turn is in flight, the dump is deferred by the worker
    /// and captures the post-turn context once the turn ends.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn context_dump(&self) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::ContextDump { event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Request the agent's effective context (system prefix, handoff, kept
    /// turns) as the messages its next request would carry. Reading changes
    /// nothing: no runtime block is reconciled and no source is consumed.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn context_messages(&self) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::ContextMessages { event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Run `turn` in the daemon; only a worker from
    /// `spawn_remote_worker` accepts it.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    #[must_use]
    pub fn send(&self, turn: RemoteTurn) -> AgentHandle<S, R> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::Send { turn, event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Whether the agent runs in the daemon.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        self.foreign_turns.is_some()
    }

    /// The next turn of the session this client did not start. Never
    /// resolves for an in-process agent.
    pub async fn next_foreign_turn(&self) -> Option<ForeignTurn<S, R>> {
        match &self.foreign_turns {
            Some(turns) => turns.recv().await.ok(),
            None => std::future::pending().await,
        }
    }

    /// Asks the worker to hand its idle agent back on `reply` and stop; a
    /// worker busy with a run hands it back once the run is over.
    pub fn release(&self, reply: Sender<R>) {
        let _ = self.command_tx.try_send(WorkerCommand::Release { reply });
    }

    /// Signal the worker task to shut down.
    pub fn shutdown(&self) {
        let _ = self.command_tx.try_send(WorkerCommand::Shutdown);
    }

    /// The background terminal jobs of this worker's session.
    #[must_use]
    pub const fn jobs(&self) -> Option<&aither_sandbox::Jobs> {
        self.jobs.as_ref()
    }

    /// Registry of in-flight subagent runs — list or cancel them.
    #[must_use]
    pub const fn subagent_runs(&self) -> Option<&aither_agent::SubagentRuns> {
        self.subagent_runs.as_ref()
    }

    /// The model for side requests, under the prompt-cache key the agent's
    /// next request carries. Read from the idle agent, so a cache epoch the
    /// agent has moved to is the one the side request uses.
    ///
    /// # Errors
    ///
    /// Returns the agent's message when it cannot give its model, or when it
    /// stops before answering.
    ///
    /// # Panics
    ///
    /// Panics when the worker command channel is already closed — the worker
    /// task has stopped unexpectedly.
    pub async fn side_model(&self) -> Result<S, String> {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded::<TuiEvent<S>>();
        self.command_tx
            .try_send(WorkerCommand::SideModel { event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        while let Ok(event) = event_rx.recv().await {
            match event {
                TuiEvent::SideModel(result) => {
                    return result.map(|slot| *slot).map_err(|error| error.to_string());
                }
                TuiEvent::Error(message) => return Err(message),
                _ => {}
            }
        }
        Err("the agent stopped before it gave its model".to_string())
    }

    /// The communication and working languages the agent was configured
    /// with.
    #[must_use]
    pub fn languages(&self) -> Option<may_harness::harness::language::Languages> {
        self.languages.as_ref().map(|guard| *guard.languages())
    }

    /// The agent's prompt-cache entry, which keeps tracking the agent's
    /// later requests.
    #[must_use]
    pub const fn cache_warmth(&self) -> Option<&aither_agent::CacheWarmth> {
        self.warmth.as_ref()
    }

    /// A worker over `command_tx` whose agent runs in the daemon.
    #[must_use]
    pub fn remote(
        command_tx: Sender<WorkerCommand<S, R>>,
        foreign_turns: Receiver<ForeignTurn<S, R>>,
        background: crate::client::background::RemoteBackground,
    ) -> Self {
        Self {
            command_tx,
            jobs: None,
            subagent_runs: None,
            languages: None,
            warmth: None,
            subagent_tracker: crate::subagent_tracker::SubagentTracker::default(),
            foreign_turns: Some(foreign_turns),
            background: Some(background),
        }
    }

    /// The session's jobs and subagents in the daemon, for a worker whose
    /// agent runs there.
    #[must_use]
    pub const fn background(&self) -> Option<&crate::client::background::RemoteBackground> {
        self.background.as_ref()
    }

    /// Live tracker of this worker's `subagent` command invocations.
    #[must_use]
    pub const fn subagent_tracker(&self) -> &crate::subagent_tracker::SubagentTracker {
        &self.subagent_tracker
    }
}

impl<S, R> Drop for AgentWorker<S, R> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Handle to receive events from a running agent task.
#[derive(Debug, Clone)]
pub struct AgentHandle<S = (), R = ()> {
    /// Receiver for events from the agent.
    event_rx: Receiver<TuiEvent<S>>,
    /// Sender for commands (for cancel).
    command_tx: Sender<WorkerCommand<S, R>>,
}

impl<S, R> AgentHandle<S, R> {
    /// A handle reading `event_rx`, cancelled through `command_tx`.
    #[must_use]
    pub const fn new(
        event_rx: Receiver<TuiEvent<S>>,
        command_tx: Sender<WorkerCommand<S, R>>,
    ) -> Self {
        Self {
            event_rx,
            command_tx,
        }
    }
}

impl<S, R> AgentHandle<S, R> {
    /// Cancel the current agent processing.
    pub fn cancel(&self) {
        let _ = self.command_tx.try_send(WorkerCommand::Cancel);
    }

    /// Check if there are pending events without blocking.
    #[must_use]
    pub fn try_recv(&self) -> Option<TuiEvent<S>> {
        self.event_rx.try_recv().ok()
    }

    /// Receive the next event asynchronously.
    pub async fn recv(&self) -> Option<TuiEvent<S>> {
        self.event_rx.recv().await.ok()
    }
}
