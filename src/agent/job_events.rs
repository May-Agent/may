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

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use aither_sandbox::{
    CommandKilled, JobEnded, JobProcess, MonitorEnd, PendingCompletion, ShellEvent, ShellEvents,
    ShellWording,
};
use askama::Template;
use async_channel::Sender;

use crate::shell::MayShellWording;

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
    LongRunning { task_id: &'a str, elapsed: String },
    /// The job is waiting for input on its stdin.
    StdinBlocked { task_id: &'a str },
    /// Watching the job's stdin failed.
    StdinWatchFailed { task_id: &'a str, error: &'a str },
    /// A monitor delivered new output lines.
    MonitorIncrement {
        task_id: &'a str,
        lines: &'a [String],
    },
    /// A monitor saw no output for its stall window.
    MonitorStalled { task_id: &'a str, stall_secs: u64 },
    /// A monitor ended.
    MonitorEnded {
        task_id: &'a str,
        end: &'a MonitorEnd,
    },
}

#[derive(Template)]
#[template(path = "job_ended.txt", escape = "none")]
struct JobEndedText<'a> {
    task_id: &'a str,
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
        /// Its task id.
        task_id: String,
        /// First lines of its output.
        output_preview: String,
        /// The file receiving its output, relative to the session sandbox.
        output_file: String,
    },
    /// A job ended; the agent reads how in its context.
    Ended {
        /// Its task id.
        task_id: String,
    },
}

/// What the agent needs of the facts it took off the queue.
#[derive(Default)]
pub(super) struct JobReport {
    /// The text of each fact the model reads, oldest first.
    pub alerts: Vec<String>,
    /// The completions the facts owed; dropping them takes them.
    pub pending: Vec<PendingCompletion>,
    /// Whether a fact needs the model to act on it.
    pub needs_model: bool,
}

impl JobReport {
    /// Reads `event` into the report and tells `facts` what clients show of
    /// it. A subagent's fact is left to the subagent.
    pub(super) fn take(&mut self, event: ShellEvent, facts: &Sender<JobFact>) {
        if !event.agent().is_main() {
            return;
        }
        match event {
            ShellEvent::JobStarted(started) => {
                let _ = facts.try_send(JobFact::Started {
                    task_id: started.task_id().to_string(),
                    output_preview: started.output_preview().to_string(),
                    output_file: started.output_file().url.clone(),
                });
            }
            ShellEvent::JobEnded(ended) => {
                let task_id = ended.info().task_id.clone();
                self.alerts.push(render_ended(&ended));
                let (_, _, _, pending) = (*ended).into_parts();
                self.pending.extend(pending);
                self.needs_model = true;
                let _ = facts.try_send(JobFact::Ended { task_id });
            }
            ShellEvent::LongRunning(job) => self.alert(&JobAlertText::LongRunning {
                task_id: job.task_id(),
                elapsed: elapsed_text(job.after()),
            }),
            ShellEvent::StdinBlocked(job) => {
                self.alert(&JobAlertText::StdinBlocked {
                    task_id: job.task_id(),
                });
                self.needs_model = true;
            }
            ShellEvent::StdinWatchFailed(job) => self.alert(&JobAlertText::StdinWatchFailed {
                task_id: job.task_id(),
                error: job.error(),
            }),
            ShellEvent::DescendantsKilled(killed) => {
                self.alert(&JobAlertText::DescendantsKilled {
                    processes: killed.processes(),
                });
            }
            ShellEvent::MonitorIncrement(increment) => {
                self.alert(&JobAlertText::MonitorIncrement {
                    task_id: increment.task_id(),
                    lines: increment.lines(),
                });
                self.needs_model = true;
            }
            ShellEvent::MonitorStalled(stalled) => {
                self.alert(&JobAlertText::MonitorStalled {
                    task_id: stalled.task_id(),
                    stall_secs: stalled.stall_secs(),
                });
                self.needs_model = true;
            }
            ShellEvent::MonitorEnded(ended) => {
                self.alert(&JobAlertText::MonitorEnded {
                    task_id: ended.task_id(),
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
        task_id: &info.task_id,
        script: quoted(&info.script),
        outcome,
        output: output.as_deref(),
        stderr: stderr.as_deref(),
        interruptions: &interruptions,
    }
    .render()
    .expect("a job's end renders typed input")
}

/// Puts the facts the session's jobs raised into the requests of a run.
pub(super) struct JobReports {
    events: ShellEvents,
    facts: Sender<JobFact>,
}

impl JobReports {
    pub(super) const fn new(events: ShellEvents, facts: Sender<JobFact>) -> Self {
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
        let mut report = JobReport::default();
        while let Some(event) = self.events.try_recv() {
            report.take(event, &self.facts);
        }
        // The run reads these facts in its next request: the completions
        // they owed are taken as the report drops.
        report
            .alerts
            .into_iter()
            .map(|alert| ContextBlock::stable(TAG, alert))
            .collect()
    }

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
