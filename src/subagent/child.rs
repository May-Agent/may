//! A subagent's in-process agent as a [`ChildAgent`]: the run loop around
//! aither's, which May owns now that aither carries no terminal concepts.
//!
//! What the loop adds to aither's run:
//! - the `compact` tool's valve: a request ends the turn at its boundary, the
//!   agent compacts and the run resumes;
//! - the wait for background commands: while any runs, or finished and not yet
//!   taken, the run waits for the next to end, tells the model and resumes,
//!   sending a cache keep-alive when the wait would outlive the cache;
//! - the per-run token usage the jobs panel reads.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aither_agent::context::{CacheLifetime, HandoffDocument};
use aither_agent::{
    AfterTool, Agent, AgentError, AgentEvent, AgentPath, ChildAgent, ChildEvents, Hooks, Run,
    RunOutcome, ToolDecision, ToolInvocation, ToolOutcome, TurnBoundaryAction, TurnBoundaryContext,
};
use aither_core::LanguageModel;
use aither_core::llm::Message;
use aither_core::llm::tool::CancelSignal;
use aither_sandbox::{Job, JobWaitError};
use futures_util::future::select_all;

use super::finished::Expires;
use super::report::{HistoryEntry, IdleAlert, JobEnd, JobEnded, render};
use crate::agent::subagent_tracker::RunUsage;
use crate::harness::compact_tool::{COMPACT_TOOL_NAME, CompactSignal};
use crate::ipc::SessionShell;

/// How long before its cache goes cold a keep-alive is sent.
const KEEP_ALIVE_LEAD: Duration = Duration::from_secs(30);

/// The decisions a child's loop makes at its control points.
#[derive(Debug)]
pub struct ChildHooks {
    valve: Option<Arc<CompactSignal>>,
    requested: Arc<AtomicBool>,
    /// Refuses every tool call with this reason when set.
    deny: Option<String>,
}

impl ChildHooks {
    /// Hooks that run the `compact` valve `valve` and refuse nothing. The
    /// flag is set when the valve ended a turn for a compaction.
    #[must_use]
    pub const fn compacting(valve: Option<Arc<CompactSignal>>, requested: Arc<AtomicBool>) -> Self {
        Self {
            valve,
            requested,
            deny: None,
        }
    }

    /// Hooks that refuse every tool call with `reason`.
    #[must_use]
    pub fn denying(reason: String) -> Self {
        Self {
            valve: None,
            requested: Arc::default(),
            deny: Some(reason),
        }
    }
}

impl Hooks for ChildHooks {
    async fn before_tool(&self, _call: &ToolInvocation<'_>) -> ToolDecision {
        self.deny
            .clone()
            .map_or(ToolDecision::Allow, ToolDecision::Deny)
    }

    async fn after_tool(&self, outcome: &ToolOutcome) -> AfterTool {
        if let Some(valve) = &self.valve
            && outcome.name != COMPACT_TOOL_NAME
        {
            valve.acted();
        }
        AfterTool::Keep
    }

    async fn on_turn_boundary(&self, _cx: &TurnBoundaryContext<'_>) -> TurnBoundaryAction {
        match &self.valve {
            Some(valve) if valve.take_request() => {
                self.requested.store(true, Ordering::Release);
                TurnBoundaryAction::EndTurn
            }
            _ => TurnBoundaryAction::Continue,
        }
    }
}

/// Keep-alive settings: set only for a provider whose cache is short enough
/// to go cold during a wait.
#[derive(Debug, Clone, Copy)]
pub struct KeepAlive {
    /// How many keep-alives in one run count as idling frequently.
    pub frequent_idle_threshold: usize,
}

/// What a session's loop reads besides the agent.
#[derive(Debug)]
pub struct ChildParts {
    /// The subagent's name, for the alert.
    pub name: String,
    /// The `compact` valve, when the agent carries the tool.
    pub valve: Option<Arc<CompactSignal>>,
    /// The compaction floor of the agent's policy.
    pub floor: usize,
    /// Keep-alives, when the provider's cache calls for them.
    pub keep_alive: Option<KeepAlive>,
    /// The name of the tool that reads a job's output.
    pub read_tool: &'static str,
    /// Where the run's token usage is credited.
    pub usage: Option<RunUsage>,
}

/// What the driver of one run credits its events to.
#[derive(Clone)]
struct Credit {
    valve: Option<Arc<CompactSignal>>,
    floor: usize,
    usage: Option<RunUsage>,
}

/// What a wait for background commands ended on.
enum Woke {
    Ended(String, JobEnd),
    KeepAlive,
    Cancelled,
}

/// An in-process subagent: its agent, its shell and what its loop holds.
pub struct ChildSession<LLM: LanguageModel + 'static> {
    agent: Agent<LLM, ChildHooks, LLM, HandoffDocument>,
    shell: SessionShell,
    requested: Arc<AtomicBool>,
    /// Handles on the jobs the run waited for, so their output stays
    /// readable.
    held: Vec<Job>,
    lifetime: CacheLifetime,
    parts: ChildParts,
    sent: usize,
    last_idle: Option<String>,
}

impl<LLM: LanguageModel + 'static> std::fmt::Debug for ChildSession<LLM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSession")
            .field("name", &self.parts.name)
            .finish_non_exhaustive()
    }
}

impl<LLM: LanguageModel + 'static> ChildSession<LLM> {
    /// A session over `agent`, built with hooks sharing `requested`, running
    /// in `shell`.
    pub fn new(
        agent: Agent<LLM, ChildHooks, LLM, HandoffDocument>,
        requested: Arc<AtomicBool>,
        shell: SessionShell,
        parts: ChildParts,
    ) -> Self {
        let lifetime = agent.cache_lifetime();
        Self {
            agent,
            shell,
            requested,
            held: Vec::new(),
            lifetime,
            parts,
            sent: 0,
            last_idle: None,
        }
    }

    /// The cache lifetime the agent runs with.
    #[must_use]
    pub const fn lifetime(&self) -> CacheLifetime {
        self.lifetime
    }

    /// The shell the agent runs in.
    #[must_use]
    pub const fn shell(&self) -> &SessionShell {
        &self.shell
    }

    /// Points the run's token usage at `usage`.
    pub fn credit_to(&mut self, usage: Option<RunUsage>) {
        self.parts.usage = usage;
    }

    /// The alert for a run that kept needing keep-alives.
    #[must_use]
    pub fn idle_alert(&self) -> Option<String> {
        let threshold = self.parts.keep_alive?.frequent_idle_threshold;
        (self.sent >= threshold).then(|| {
            render(&IdleAlert {
                subagent: &self.parts.name,
                sent: self.sent,
                reason: self
                    .last_idle
                    .as_deref()
                    .unwrap_or("idle without a running background job"),
            })
        })
    }

    /// The tail of the conversation, for a failed run's error text.
    #[must_use]
    pub fn recent_history(&self) -> Vec<HistoryEntry> {
        let history = self.agent.history();
        let skipped = history.len().saturating_sub(12);
        history
            .iter()
            .skip(skipped)
            .map(|message| HistoryEntry {
                role: message.role(),
                content: message.content().to_string(),
                tool_names: message
                    .tool_calls()
                    .iter()
                    .map(|call| call.name.clone())
                    .collect(),
            })
            .collect()
    }

    /// Stops the jobs the agent left running.
    pub async fn stop_jobs(&self) -> usize {
        self.shell.jobs().kill_all().await
    }

    fn credit(&self) -> Credit {
        Credit {
            valve: self.parts.valve.clone(),
            floor: self.parts.floor,
            usage: self.parts.usage.clone(),
        }
    }

    /// Drives `run` to its end, forwarding its events and crediting its
    /// requests; `None` when it emitted no end.
    async fn drive<F>(
        run: Run<F>,
        own: &AgentPath,
        credit: &Credit,
        events: &ChildEvents,
        cancel: &CancelSignal,
    ) -> Result<Option<RunOutcome>, AgentError>
    where
        F: std::future::Future<Output = Result<(), AgentError>>,
    {
        let flow = run
            .drive(None, cancel.cancelled(), |ended, timed| {
                let mut ended = ended;
                if timed.agent == *own {
                    match &timed.event {
                        AgentEvent::RunEnded { outcome, .. } => ended = Some(outcome.clone()),
                        AgentEvent::RequestEnded {
                            usage: Some(reported),
                            ..
                        } => {
                            if let Some(usage) = &credit.usage {
                                usage.record(reported);
                            }
                            if let Some(valve) = &credit.valve {
                                let tokens = reported.prompt_tokens.unwrap_or(0);
                                valve.sync(
                                    false,
                                    usize::try_from(tokens).unwrap_or(usize::MAX),
                                    credit.floor,
                                );
                            }
                        }
                        _ => {}
                    }
                }
                events.forward(timed);
                std::future::ready(ControlFlow::<(), _>::Continue(ended))
            })
            .await?;
        Ok(match flow {
            ControlFlow::Continue(ended) => ended,
            ControlFlow::Break(()) => Some(RunOutcome::Cancelled),
        })
    }

    fn outcome(driven: Result<Option<RunOutcome>, AgentError>) -> RunOutcome {
        match driven {
            Ok(Some(outcome)) => outcome,
            Ok(None) => RunOutcome::Cancelled,
            Err(error) => RunOutcome::Failed { error },
        }
    }

    async fn turn(
        &mut self,
        prompt: Option<&str>,
        events: &ChildEvents,
        cancel: &CancelSignal,
    ) -> RunOutcome {
        let own = AgentPath::of(self.agent.identity());
        let credit = self.credit();
        let driven = match prompt {
            Some(prompt) => {
                Self::drive(
                    self.agent.run(prompt, std::iter::empty()),
                    &own,
                    &credit,
                    events,
                    cancel,
                )
                .await
            }
            None => Self::drive(self.agent.resume(), &own, &credit, events, cancel).await,
        };
        Self::outcome(driven)
    }

    /// Compacts the agent's context, then lets the valve know it landed.
    async fn compact(&mut self, events: &ChildEvents, cancel: &CancelSignal) -> Option<RunOutcome> {
        let own = AgentPath::of(self.agent.identity());
        let credit = self.credit();
        let driven = Self::drive(self.agent.compact(None), &own, &credit, events, cancel).await;
        if let Some(valve) = &self.parts.valve {
            valve.landed();
        }
        match driven {
            Ok(Some(RunOutcome::Cancelled)) => Some(RunOutcome::Cancelled),
            Ok(_) => None,
            Err(error) => Some(RunOutcome::Failed { error }),
        }
    }

    /// Waits until a background command ends, the cache needs a keep-alive
    /// or the run is cancelled; `None` when no command runs or waits for
    /// its completion to be taken.
    async fn await_jobs(&mut self, cancel: &CancelSignal) -> Option<Woke> {
        let jobs = self.shell.jobs();
        let mut waiting = Vec::new();
        for info in jobs.list().await {
            if (matches!(info.status, aither_sandbox::JobStatus::Running)
                || info.status.completion_pending())
                && let Some(job) = jobs.get(&info.task_id).await
            {
                waiting.push(job);
            }
        }
        if waiting.is_empty() {
            return None;
        }
        self.last_idle = Some(format!(
            "waiting on background task {}",
            waiting[0].task_id()
        ));
        let keep_alive = self
            .parts
            .keep_alive
            .and_then(|_| self.agent.cache_warmth().cold_in())
            .map(|cold_in| {
                cold_in
                    .saturating_sub(KEEP_ALIVE_LEAD)
                    .max(Duration::from_secs(1))
            });
        let ended = async {
            let (finished, _, _) =
                select_all(waiting.iter().map(|job| {
                    Box::pin(async move { (job.task_id().to_owned(), job.wait().await) })
                }))
                .await;
            let (task_id, result) = finished;
            let end = match result {
                Ok(script) => match script.outcome {
                    aither_sandbox::RunOutcome::Exited { code } => JobEnd::Exited { code },
                    aither_sandbox::RunOutcome::Killed(_) => JobEnd::Killed,
                },
                Err(JobWaitError::Failed { error, .. }) => JobEnd::Failed { error },
                Err(other) => JobEnd::Failed {
                    error: other.to_string(),
                },
            };
            Woke::Ended(task_id, end)
        };
        let timer = async {
            match keep_alive {
                Some(after) => {
                    async_io::Timer::after(after).await;
                    Woke::KeepAlive
                }
                None => std::future::pending().await,
            }
        };
        let cancelled = async {
            cancel.cancelled().await;
            Woke::Cancelled
        };
        let woke =
            futures_lite::future::or(ended, futures_lite::future::or(timer, cancelled)).await;
        self.held.extend(waiting);
        Some(woke)
    }
}

impl<LLM: LanguageModel + 'static> Expires for ChildSession<LLM> {
    fn expired(&self) -> Option<CacheLifetime> {
        (!self.agent.cache_warmth().is_warm()).then_some(self.lifetime)
    }
}

impl<LLM: LanguageModel + 'static> ChildAgent for ChildSession<LLM> {
    async fn run(
        &mut self,
        prompt: &str,
        events: &ChildEvents,
        cancel: CancelSignal,
    ) -> RunOutcome {
        let mut outcome = self.turn(Some(prompt), events, &cancel).await;
        loop {
            if !matches!(outcome, RunOutcome::Completed { .. }) {
                return outcome;
            }
            if self.requested.swap(false, Ordering::AcqRel) {
                if let Some(ended) = self.compact(events, &cancel).await {
                    return ended;
                }
                outcome = self.turn(None, events, &cancel).await;
                continue;
            }
            match self.await_jobs(&cancel).await {
                None => return outcome,
                Some(Woke::Cancelled) => return RunOutcome::Cancelled,
                Some(Woke::KeepAlive) => {
                    if let Err(error) = self.agent.keep_cache_warm().await {
                        return RunOutcome::Failed { error };
                    }
                    self.sent += 1;
                }
                Some(Woke::Ended(task_id, end)) => {
                    let text = render(&JobEnded {
                        task_id: &task_id,
                        end,
                        read_tool: self.parts.read_tool,
                    });
                    self.agent.push_message(Message::user(text));
                    outcome = self.turn(None, events, &cancel).await;
                }
            }
        }
    }
}
