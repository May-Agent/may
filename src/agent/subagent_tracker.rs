//! Live tracking of `subagent` terminal-command invocations.
//!
//! The `subagent` command executes inside the session's IPC command registry,
//! so the inner agent's LLM traffic never reaches the chat event stream. To
//! make a running subagent observable (elapsed time and token counters in the
//! jobs panel and status bar), [`register_shared_terminal_commands`] wraps the
//! registered tool and the model it hands to inner agents:
//!
//! - [`TrackedSubagentTool`] records every invocation in a shared
//!   [`SubagentTracker`] and scopes it to its run entry.
//! - [`UsageObservedModel`] credits every streamed `Usage` event to the
//!   in-scope run.
//!
//! A task-local links the two halves: several subagents may run concurrently
//! on the same shared model, so the observer cannot tell which run a `Usage`
//! event belongs to on its own.
//!
//! [`register_shared_terminal_commands`]: crate::terminal_commands::register_shared_terminal_commands

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aither_agent::specialized::SubagentArgs;
use aither_core::LanguageModel;
use aither_core::llm::model::Profile;
use aither_core::llm::tool::ToolContext;
use aither_core::llm::{Event, LLMRequest, LLMRequestWithTools, Tool, ToolResult};
use aither_sandbox::CommandUsage;
use dashmap::DashMap;

use crate::command_reading::{Card, SubagentState};
use futures_lite::{Stream, StreamExt};

tokio::task_local! {
    /// The run currently executing inside [`TrackedSubagentTool::call`].
    static ACTIVE_RUN: Arc<SubagentRun>;
}

/// Point-in-time view of one tracked subagent run, for rendering.
#[derive(Debug, Clone)]
pub struct SubagentRunSnapshot {
    /// Stable identifier of this run within its tracker.
    pub run_id: u64,
    /// Subagent type name (or file path) the call invoked.
    pub agent: String,
    /// Full prompt handed to the subagent.
    pub prompt: String,
    /// When the run started.
    pub started_at: Instant,
    /// Prompt tokens consumed so far.
    pub prompt_tokens: u64,
    /// Completion tokens produced so far.
    pub completion_tokens: u64,
    /// LLM responses the run has observed so far.
    pub requests: u64,
}

impl SubagentRunSnapshot {
    /// Seconds elapsed since the run started.
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Total tokens observed so far.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// One in-flight subagent invocation.
#[derive(Debug)]
struct SubagentRun {
    /// Run identifier within the tracker.
    id: u64,
    /// Requested subagent name (or path).
    agent: String,
    /// Prompt passed to the subagent.
    prompt: String,
    /// When the call started.
    started_at: Instant,
    /// Accumulated prompt tokens.
    prompt_tokens: AtomicU64,
    /// Accumulated completion tokens.
    completion_tokens: AtomicU64,
    /// Number of `Usage` events (one per LLM response) seen so far.
    requests: AtomicU64,
}

impl SubagentRun {
    /// Credit one `Usage` event to this run.
    fn record(&self, usage: &aither_core::llm::Usage) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if let Some(tokens) = usage.prompt_tokens {
            self.prompt_tokens
                .fetch_add(u64::from(tokens), Ordering::Relaxed);
        }
        if let Some(tokens) = usage.completion_tokens {
            self.completion_tokens
                .fetch_add(u64::from(tokens), Ordering::Relaxed);
        }
    }

    /// Read the run's current counters.
    fn snapshot(&self) -> SubagentRunSnapshot {
        SubagentRunSnapshot {
            run_id: self.id,
            agent: self.agent.clone(),
            prompt: self.prompt.clone(),
            started_at: self.started_at,
            prompt_tokens: self.prompt_tokens.load(Ordering::Relaxed),
            completion_tokens: self.completion_tokens.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
        }
    }
}

/// Removes its run entry when dropped — including on future cancellation.
#[derive(Debug)]
struct RunGuard {
    /// The tracker's run map.
    runs: Arc<DashMap<u64, Arc<SubagentRun>>>,
    /// Entry to remove.
    id: u64,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs.remove(&self.id);
    }
}

/// The subagent handles one session keeps across agent rebuilds, so runs a
/// turn started stay listed and cancellable after the next turn rebuilt the
/// agent.
#[derive(Debug, Default, Clone)]
pub struct SubagentHandles {
    /// Cancellation registry of the `subagent` command's runs.
    pub runs: aither_agent::specialized::SubagentRuns,
    /// Live snapshots of the same runs.
    pub tracker: SubagentTracker,
}

/// Shared registry of in-flight `subagent` runs for one session.
///
/// The session's worker holds one handle and the `subagent` IPC command holds
/// another; the TUI reads snapshots off it for the jobs panel and status bar.
/// A default-constructed tracker still records — it simply has no reader.
#[derive(Debug, Default, Clone)]
pub struct SubagentTracker {
    /// Live run entries.
    runs: Arc<DashMap<u64, Arc<SubagentRun>>>,
    /// Run id sequence.
    next_id: Arc<AtomicU64>,
    /// Where the commands of the session that render as cards report them.
    cards: super::command_cards::CommandCards,
}

impl SubagentTracker {
    /// The session's command cards: `subagent` reports its runs there, and
    /// so do the other commands that render as cards.
    #[must_use]
    pub const fn cards(&self) -> &super::command_cards::CommandCards {
        &self.cards
    }

    /// Snapshots of all in-flight runs, oldest start first.
    #[must_use]
    pub fn running(&self) -> Vec<SubagentRunSnapshot> {
        let mut runs: Vec<SubagentRunSnapshot> = self
            .runs
            .iter()
            .map(|entry| entry.value().snapshot())
            .collect();
        runs.sort_by_key(|run| run.started_at);
        runs
    }

    /// Snapshot of one run, if it is still in flight.
    #[must_use]
    pub fn get(&self, run_id: u64) -> Option<SubagentRunSnapshot> {
        self.runs.get(&run_id).map(|run| run.snapshot())
    }

    /// Register a new run and return its scope value plus the guard that
    /// removes the entry on drop.
    fn begin(&self, agent: &str, prompt: &str) -> (Arc<SubagentRun>, RunGuard) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let run = Arc::new(SubagentRun {
            id,
            agent: agent.to_string(),
            prompt: prompt.to_string(),
            started_at: Instant::now(),
            prompt_tokens: AtomicU64::new(0),
            completion_tokens: AtomicU64::new(0),
            requests: AtomicU64::new(0),
        });
        self.runs.insert(id, run.clone());
        (
            run,
            RunGuard {
                runs: self.runs.clone(),
                id,
            },
        )
    }
}

/// Credit `usage` to the in-scope run, if a tracked call is executing.
fn record_usage(usage: &aither_core::llm::Usage) {
    let _ = ACTIVE_RUN.try_with(|run| run.record(usage));
}

/// A [`LanguageModel`] that credits streamed `Usage` events to the in-scope
/// tracked run.
///
/// Only the `subagent` command's inner agents receive this model, so every
/// observed event belongs to some subagent run; the task-local scope set by
/// [`TrackedSubagentTool::call`] identifies which one.
#[derive(Clone)]
pub struct UsageObservedModel<M> {
    inner: M,
}

impl<M> fmt::Debug for UsageObservedModel<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageObservedModel").finish_non_exhaustive()
    }
}

impl<M> UsageObservedModel<M> {
    /// Wraps `inner` so its response streams feed the in-scope tracked run.
    pub const fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<M> LanguageModel for UsageObservedModel<M>
where
    M: LanguageModel + Clone,
{
    type Error = M::Error;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
        self.inner.respond(request).inspect(|item| {
            if let Ok(Event::Usage(usage)) = item {
                record_usage(usage);
            }
        })
    }

    fn respond_with_tools(
        &self,
        request: LLMRequestWithTools<'_>,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
        self.inner.respond_with_tools(request).inspect(|item| {
            if let Ok(Event::Usage(usage)) = item {
                record_usage(usage);
            }
        })
    }

    fn profile(&self) -> impl Future<Output = Result<Profile, M::Error>> + Send {
        self.inner.profile()
    }
}

/// A `subagent` IPC command that reports each invocation to a
/// [`SubagentTracker`].
///
/// Registered in place of a bare `SubagentTool`; combine it with
/// [`UsageObservedModel`] as the inner tool's model so per-run token counters
/// fill in.
pub struct TrackedSubagentTool<T> {
    inner: T,
    tracker: SubagentTracker,
    /// Whether each run is reported as the card of its terminal call.
    cards: bool,
}

impl<T> fmt::Debug for TrackedSubagentTool<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrackedSubagentTool")
            .finish_non_exhaustive()
    }
}

impl<T> TrackedSubagentTool<T> {
    /// Wraps `inner`, reporting every call to `tracker`.
    pub const fn new(inner: T, tracker: SubagentTracker) -> Self {
        Self {
            inner,
            tracker,
            cards: false,
        }
    }

    /// Reports every run as the card of the terminal call that started it.
    #[must_use]
    pub const fn with_cards(mut self) -> Self {
        self.cards = true;
        self
    }
}

impl<T> Tool for TrackedSubagentTool<T>
where
    T: Tool<Arguments = SubagentArgs, Res = ToolResult> + Send + Sync + 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("subagent")
    }

    type Arguments = SubagentArgs;
    type Res = ToolResult;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> aither_core::Result<Self::Res> {
        // `prompt_file` carries the task text now; read it for the run's
        // display label, falling back to the path itself if it is gone.
        let subagent = args.subagent().map_or_else(
            || {
                args.id
                    .as_deref()
                    .map_or_else(|| "resume".to_string(), |id| format!("resume {id}"))
            },
            ToString::to_string,
        );
        let prompt_file = args.prompt_file().to_string();
        let prompt = async_fs::read_to_string(&prompt_file)
            .await
            .unwrap_or(prompt_file);
        let (run, _guard) = self.tracker.begin(&subagent, &prompt);
        let card = self
            .cards
            .then(|| cx.identity().map(|identity| identity.call().to_string()))
            .flatten();
        let started_at = jiff::Timestamp::now();
        let card_of = |state, finished_at, result| Card::Subagent {
            agent: subagent.clone(),
            title: Card::brief_title(&prompt),
            state,
            started_at,
            finished_at,
            result,
        };
        if let Some(call_id) = &card {
            self.tracker
                .cards()
                .report(call_id, Some(card_of(SubagentState::Running, None, None)));
        }
        let outcome = ACTIVE_RUN.scope(run, self.inner.call(args, cx)).await;
        if let Some(call_id) = &card {
            // A run that failed has no card: the ordinary row shows why.
            let finished = outcome.as_ref().ok().and_then(|result| match result {
                ToolResult::Text { text } | ToolResult::Tsv { text } => Some(text.clone()),
                ToolResult::Done => Some(String::new()),
                _ => None,
            });
            let card = finished.map(|text| {
                card_of(
                    SubagentState::Done,
                    Some(jiff::Timestamp::now()),
                    Some(text),
                )
            });
            self.tracker.cards().report(call_id, card);
        }
        outcome
    }
}

impl<T> CommandUsage for TrackedSubagentTool<T>
where
    T: CommandUsage<Arguments = SubagentArgs, Res = ToolResult> + Send + Sync + 'static,
{
    fn usage(&self) -> impl Future<Output = String> + Send {
        self.inner.usage()
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::{AgentIdentity, CallIdentity, ToolCallId};

    use super::*;

    /// An inner `subagent` that returns `outcome`.
    struct Stub(Result<&'static str, &'static str>);

    impl Tool for Stub {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("subagent")
        }

        type Arguments = SubagentArgs;
        type Res = ToolResult;

        #[allow(clippy::unused_async_trait_impl)]
        async fn call(
            &self,
            _: Self::Arguments,
            _: ToolContext,
        ) -> aither_core::Result<ToolResult> {
            match self.0 {
                Ok(text) => Ok(ToolResult::text(text)),
                Err(message) => Err(anyhow::anyhow!(message)),
            }
        }
    }

    fn args(brief: &std::path::Path) -> SubagentArgs {
        serde_json::from_value(serde_json::json!({
            "name": "explorer",
            "prompt_file": brief.to_str().expect("utf-8 path"),
        }))
        .expect("subagent arguments")
    }

    fn call_context() -> ToolContext {
        ToolContext::new().with_identity(CallIdentity::new(
            ToolCallId::new("c1"),
            AgentIdentity::Main,
        ))
    }

    #[tokio::test]
    async fn a_run_is_reported_as_the_card_of_its_call_until_it_returns() {
        let dir = tempfile::tempdir().expect("dir");
        let brief = dir.path().join("brief.md");
        std::fs::write(&brief, "\n## Map the parser\n\nbody").expect("brief");
        let tracker = SubagentTracker::default();
        let updates = tracker.cards().claim_updates().expect("reader");
        let tool = TrackedSubagentTool::new(Stub(Ok("Found two parsers.")), tracker).with_cards();
        tool.call(args(&brief), call_context()).await.expect("runs");

        let running = updates.try_recv().expect("start");
        assert_eq!(running.call_id, "c1");
        let Some(Card::Subagent {
            agent,
            title,
            state,
            result,
            ..
        }) = running.card
        else {
            panic!("a subagent card");
        };
        assert_eq!(
            (agent.as_str(), title.as_str()),
            ("explorer", "Map the parser")
        );
        assert_eq!((state, result), (SubagentState::Running, None));

        let done = updates.try_recv().expect("completion");
        let Some(Card::Subagent {
            state,
            finished_at,
            result,
            ..
        }) = done.card
        else {
            panic!("a subagent card");
        };
        assert_eq!(state, SubagentState::Done);
        assert!(finished_at.is_some());
        assert_eq!(result.as_deref(), Some("Found two parsers."));
    }

    #[tokio::test]
    async fn a_failed_run_withdraws_its_card() {
        let dir = tempfile::tempdir().expect("dir");
        let brief = dir.path().join("brief.md");
        std::fs::write(&brief, "brief").expect("brief");
        let tracker = SubagentTracker::default();
        let updates = tracker.cards().claim_updates().expect("reader");
        let tool = TrackedSubagentTool::new(Stub(Err("no such subagent")), tracker).with_cards();
        assert!(tool.call(args(&brief), call_context()).await.is_err());
        assert!(updates.try_recv().expect("start").card.is_some());
        assert_eq!(updates.try_recv().expect("withdrawn").card, None);
    }

    #[tokio::test]
    async fn explore_and_calls_outside_a_terminal_report_no_card() {
        let dir = tempfile::tempdir().expect("dir");
        let brief = dir.path().join("brief.md");
        std::fs::write(&brief, "brief").expect("brief");
        let tracker = SubagentTracker::default();
        let updates = tracker.cards().claim_updates().expect("reader");
        let plain = TrackedSubagentTool::new(Stub(Ok("x")), tracker.clone());
        plain
            .call(args(&brief), call_context())
            .await
            .expect("runs");
        let carded = TrackedSubagentTool::new(Stub(Ok("x")), tracker).with_cards();
        carded
            .call(args(&brief), ToolContext::new())
            .await
            .expect("runs");
        assert!(updates.try_recv().is_err());
    }
}
