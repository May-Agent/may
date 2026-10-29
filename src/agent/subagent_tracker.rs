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
use dashmap::DashMap;
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
}

impl SubagentTracker {
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
        Self { inner, tracker }
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
        // `prompt_file` carries the task text; read it for the run's
        // display label, falling back to the path itself if it is gone. A
        // resumed run has no subagent name of its own, so it is labelled by
        // the id it continues.
        let (agent, prompt_file) = match &args {
            SubagentArgs::Start {
                subagent,
                prompt_file,
                ..
            } => (subagent.clone(), prompt_file),
            SubagentArgs::Resume { id, prompt_file } => (format!("resume #{id}"), prompt_file),
        };
        let prompt = async_fs::read_to_string(prompt_file)
            .await
            .unwrap_or_else(|_| prompt_file.clone());
        let (run, _guard) = self.tracker.begin(&agent, &prompt);
        ACTIVE_RUN.scope(run, self.inner.call(args, cx)).await
    }
}
