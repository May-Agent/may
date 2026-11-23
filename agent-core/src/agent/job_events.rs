//! What the session's jobs report to its agent.
//!
//! The shell raises a fact for every job it detaches, every one that ends and
//! every watch that fires. The agent reads each fact once: while a run is
//! under way, [`JobReports`] puts it into the next request; while the agent
//! idles, [`MayAgent::wait_background`](super::MayAgent::wait_background)
//! puts it into the context and hands the facts that need the model to the
//! host as a [`BackgroundWake`]. A subagent's shell raises its facts on the
//! same queue; they name their agent, and the subagent reads its own jobs, so
//! the main agent leaves them.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use aither_sandbox::{
    CommandKilled, JobEnded, JobProcess, MonitorEnd, PendingCompletion, ShellEvent, ShellEvents,
    ShellWording,
};
use askama::Template;
use async_channel::Sender;

use crate::shell::MayShellWording;

/// Rendered completion of a main-agent background job.
///
/// Kept by the session so rewinding over the turn that read it can restore the
/// completion if the job itself predated the rewind point.
#[derive(Debug, Clone)]
pub struct DeliveredJobCompletion {
    /// OS PID of the job.
    pub pid: u32,
    /// Monotonic instant when the job was launched.
    pub started_at: Instant,
    /// Monotonic instant when the completion was read by an agent.
    pub delivered_at: Instant,
    /// Rendered natural-language alert text.
    pub alert: String,
}

type AbandonedJobPred = Arc<dyn Fn(u32, Option<Instant>) -> bool + Send + Sync>;
type JobCompletionRecorder = Arc<dyn Fn(DeliveredJobCompletion) + Send + Sync>;

/// Session-level hooks connecting an agent's job reader to `SessionBackgrounds`.
#[derive(Clone, Default)]
pub struct SessionJobHooks {
    abandoned: Option<AbandonedJobPred>,
    recorder: Option<JobCompletionRecorder>,
}

impl std::fmt::Debug for SessionJobHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionJobHooks").finish_non_exhaustive()
    }
}

impl SessionJobHooks {
    /// Builds hooks filtering abandoned post-rewind jobs and recording
    /// delivered completions.
    #[must_use]
    pub fn new(
        abandoned: impl Fn(u32, Option<Instant>) -> bool + Send + Sync + 'static,
        recorder: impl Fn(DeliveredJobCompletion) + Send + Sync + 'static,
    ) -> Self {
        Self {
            abandoned: Some(Arc::new(abandoned)),
            recorder: Some(Arc::new(recorder)),
        }
    }

    fn is_abandoned(&self, pid: u32, started_at: Option<Instant>) -> bool {
        self.abandoned
            .as_ref()
            .is_some_and(|pred| pred(pid, started_at))
    }

    fn record(&self, completion: DeliveredJobCompletion) {
        if let Some(recorder) = &self.recorder {
            recorder(completion);
        }
    }
}

#[derive(Default)]
struct BridgeShared {
    prior: Vec<ShellEvents>,
    hooks: SessionJobHooks,
}

/// Multiplexes the current shell's [`ShellEvents`] with surviving pre-rewind
/// shells' queues and filters out jobs belonging to rewound turns.
#[derive(Clone)]
pub(super) struct SessionJobBridge {
    current: ShellEvents,
    shared: Arc<Mutex<BridgeShared>>,
}

impl SessionJobBridge {
    pub(super) fn new(current: ShellEvents) -> Self {
        Self {
            current,
            shared: Arc::new(Mutex::new(BridgeShared::default())),
        }
    }

    pub(super) fn configure(&self, prior: Vec<ShellEvents>, hooks: SessionJobHooks) {
        let mut guard = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.prior = prior;
        guard.hooks = hooks;
    }

    pub(super) fn hooks(&self) -> SessionJobHooks {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hooks
            .clone()
    }

    fn prior(&self) -> Vec<ShellEvents> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prior
            .clone()
    }

    pub(super) fn try_recv(&self) -> Option<ShellEvent> {
        for events in self.prior() {
            if let Some(event) = events.try_recv() {
                return Some(event);
            }
        }
        self.current.try_recv()
    }

    pub(super) async fn recv(&self) -> ShellEvent {
        let prior = self.prior();
        if prior.is_empty() {
            return self.current.recv().await;
        }
        let mut fut: std::pin::Pin<Box<dyn std::future::Future<Output = ShellEvent> + Send + '_>> =
            Box::pin(self.current.recv());
        for events in &prior {
            fut = Box::pin(futures_lite::future::or(events.recv(), fut));
        }
        fut.await
    }
}

/// The tag the facts' blocks carry.
const TAG: &str = "system-reminder";

/// Longest script a report quotes, in characters.
const SCRIPT_CHARS: usize = 100;

#[derive(Template)]
#[template(path = "job_alert.txt", escape = "none")]
struct JobAlert<'a> {
    alert: &'a JobAlertText<'a>,
}

/// What a fact that is not a job's end says.
enum JobAlertText<'a> {
    /// A foreground command left processes running; they were killed.
    DescendantsKilled { processes: &'a [JobProcess] },
    /// The job crossed the long-running threshold.
    LongRunning { pid: u32, elapsed: String },
    /// The job is waiting for input on its stdin.
    StdinBlocked { pid: u32 },
    /// Watching the job's stdin failed.
    StdinWatchFailed { pid: u32, error: &'a str },
    /// A monitor delivered new output lines.
    MonitorIncrement { pid: u32, lines: &'a [String] },
    /// A monitor saw no output for its stall window.
    MonitorStalled { pid: u32, stall_secs: u64 },
    /// A monitor ended.
    MonitorEnded { pid: u32, end: &'a MonitorEnd },
}

#[derive(Template)]
#[template(path = "job_ended.txt", escape = "none")]
struct JobEndedText<'a> {
    pid: u32,
    script: &'a str,
    outcome: JobOutcome<'a>,
    output: Option<&'a str>,
    stderr: Option<&'a str>,
    interruptions: &'a str,
}

/// How a background task ended.
enum JobOutcome<'a> {
    /// The script ran to its end.
    Finished { code: i32 },
    /// The job was killed, and whatever code the harness saw means nothing.
    Killed { notice: Option<String> },
    /// The task could not be run or tracked to its end.
    Failed { error: &'a str },
}

/// A fact of a main-agent job, for the clients that show the session's jobs.
#[derive(Debug, Clone)]
pub enum JobFact {
    /// A job was detached.
    Started {
        /// Its PID.
        pid: u32,
        /// First lines of its output.
        output_preview: String,
        /// The file receiving its output, relative to the session sandbox.
        output_file: String,
    },
    /// A job ended; the agent reads how in its context.
    Ended {
        /// Its PID.
        pid: u32,
    },
}

/// What the agent needs of the facts it took off the queue.
#[derive(Default)]
pub(super) struct JobReport {
    /// The text of each fact the model reads, oldest first.
    pub alerts: Vec<String>,
    /// The completions the facts owed; dropping them takes them.
    pub pending: Vec<PendingCompletion>,
    /// Completions of rewound jobs to settle silently without waking.
    pub discarded: Vec<PendingCompletion>,
    /// Whether a fact needs the model to act on it.
    pub needs_model: bool,
}

impl JobReport {
    /// Reads `event` into the report and tells `facts` what clients show of
    /// it. A subagent's fact is left to the subagent.
    pub(super) fn take(
        &mut self,
        event: ShellEvent,
        facts: &Sender<JobFact>,
        hooks: &SessionJobHooks,
    ) {
        if !event.agent().is_main() {
            return;
        }
        match event {
            ShellEvent::JobStarted(started) => {
                if hooks.is_abandoned(started.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                let _ = facts.try_send(JobFact::Started {
                    pid: started.task_id().parse().unwrap_or(0),
                    output_preview: started.output_preview().to_string(),
                    output_file: started.output_file().url.clone(),
                });
            }
            ShellEvent::JobEnded(ended) => {
                let pid = ended.info().pid;
                let started_at = ended.info().started_at;
                if hooks.is_abandoned(pid, Some(started_at)) {
                    let (_, _, _, pending) = (*ended).into_parts();
                    self.discarded.extend(pending);
                    return;
                }
                let rendered = render_ended(&ended);
                hooks.record(DeliveredJobCompletion {
                    pid,
                    started_at,
                    delivered_at: Instant::now(),
                    alert: rendered.clone(),
                });
                self.alerts.push(rendered);
                let (_, _, _, pending) = (*ended).into_parts();
                self.pending.extend(pending);
                self.needs_model = true;
                let _ = facts.try_send(JobFact::Ended { pid });
            }
            ShellEvent::LongRunning(job) => {
                if hooks.is_abandoned(job.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                self.alert(&JobAlertText::LongRunning {
                    pid: job.task_id().parse().unwrap_or(0),
                    elapsed: elapsed_text(job.after()),
                });
            }
            ShellEvent::StdinBlocked(job) => {
                if hooks.is_abandoned(job.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                self.alert(&JobAlertText::StdinBlocked {
                    pid: job.task_id().parse().unwrap_or(0),
                });
                self.needs_model = true;
            }
            ShellEvent::StdinWatchFailed(job) => {
                if hooks.is_abandoned(job.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                self.alert(&JobAlertText::StdinWatchFailed {
                    pid: job.task_id().parse().unwrap_or(0),
                    error: job.error(),
                });
            }
            ShellEvent::DescendantsKilled(killed) => {
                self.alert(&JobAlertText::DescendantsKilled {
                    processes: killed.processes(),
                });
            }
            ShellEvent::MonitorIncrement(increment) => {
                if hooks.is_abandoned(increment.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                self.alert(&JobAlertText::MonitorIncrement {
                    pid: increment.task_id().parse().unwrap_or(0),
                    lines: increment.lines(),
                });
                self.needs_model = true;
            }
            ShellEvent::MonitorStalled(stalled) => {
                if hooks.is_abandoned(stalled.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                self.alert(&JobAlertText::MonitorStalled {
                    pid: stalled.task_id().parse().unwrap_or(0),
                    stall_secs: stalled.stall_secs(),
                });
                self.needs_model = true;
            }
            ShellEvent::MonitorEnded(ended) => {
                if hooks.is_abandoned(ended.task_id().parse().unwrap_or(0), None) {
                    return;
                }
                self.alert(&JobAlertText::MonitorEnded {
                    pid: ended.task_id().parse().unwrap_or(0),
                    end: ended.end(),
                });
                self.needs_model = true;
            }
            // A fact aither raises that this version does not word has no
            // text for the model; it fails here rather than going unread.
            other => unreachable!("an unworded shell event: {other:?}"),
        }
    }

    fn alert(&mut self, alert: &JobAlertText<'_>) {
        self.alerts.push(
            JobAlert { alert }
                .render()
                .expect("a job alert renders typed input"),
        );
    }
}

/// How long a job ran, as the alert says it.
fn elapsed_text(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 60 {
        format!("{} minutes", seconds / 60)
    } else {
        format!("{seconds} seconds")
    }
}

/// The first `SCRIPT_CHARS` characters of `script`.
fn quoted(script: &str) -> &str {
    script
        .char_indices()
        .nth(SCRIPT_CHARS)
        .map_or(script, |(end, _)| script.split_at(end).0)
}

/// A stream as the block of lines the model reads, `None` when it is empty.
fn block(stream: &aither_core::llm::CommandStream) -> Option<String> {
    Some(stream.render_for_model()).filter(|text| !text.is_empty())
}

/// What the model reads of a background task that ended.
fn render_ended(ended: &JobEnded) -> String {
    let info = ended.info();
    let error_text;
    let (outcome, output, stderr, interruptions) = match ended.result() {
        Ok(command) => {
            let outcome = if info.status.kill_cause().is_some() {
                JobOutcome::Killed {
                    notice: ended
                        .timeout_kill()
                        .map(|kill| MayShellWording.command_killed(&CommandKilled::TimedOut(kill))),
                }
            } else {
                JobOutcome::Finished {
                    code: command.exit_code,
                }
            };
            (
                outcome,
                block(&command.stdout),
                command.stderr.as_ref().and_then(block),
                aither_core::llm::render_interruptions(&command.interruptions),
            )
        }
        Err(error) => {
            error_text = error.to_string();
            (
                JobOutcome::Failed { error: &error_text },
                None,
                None,
                String::new(),
            )
        }
    };
    JobEndedText {
        pid: info.pid,
        script: quoted(&info.script),
        outcome,
        output: output.as_deref(),
        stderr: stderr.as_deref(),
        interruptions: &interruptions,
    }
    .render()
    .expect("a job's end renders typed input")
}

/// Renders the natural-language alert text for an ended background job.
#[must_use]
pub fn render_job_ended(ended: &JobEnded) -> String {
    render_ended(ended)
}

/// Puts the facts the session's jobs raised into the requests of a run.
pub(super) struct JobReports {
    events: SessionJobBridge,
    facts: Sender<JobFact>,
}

impl JobReports {
    pub(super) const fn new(events: SessionJobBridge, facts: Sender<JobFact>) -> Self {
        Self { events, facts }
    }
}

impl ContextSource for JobReports {
    fn id(&self) -> SourceId {
        SourceId::new("jobs")
    }

    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        if !matches!(moment, Moment::Request { .. } | Moment::ToolTurnEnd) {
            return Vec::new();
        }
        let hooks = self.events.hooks();
        let mut report = JobReport::default();
        while let Some(event) = self.events.try_recv() {
            report.take(event, &self.facts, &hooks);
        }
        for completion in report.discarded {
            completion.take().await;
        }
        // The run reads these facts in its next request: the completions
        // they owed are taken as the report drops.
        report
            .alerts
            .into_iter()
            .map(|alert| ContextBlock::stable(TAG, alert))
            .collect()
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn peek(&self, _moment: Moment) -> Vec<ContextBlock> {
        Vec::new()
    }
}

/// What the idle agent's wait on its jobs took.
#[derive(Debug)]
pub struct BackgroundWake {
    /// The completions the wake recorded: their jobs read as owed a turn
    /// until these drop, which the turn the wake starts does once it ran.
    pub pending: Vec<PendingCompletion>,
    /// Whether the model has something to act on, which the host continues
    /// with [`MayAgent::resume`](super::MayAgent::resume).
    pub starts_run: bool,
}
