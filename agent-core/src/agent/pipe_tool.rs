//! The `pipe` IPC command: inspects or reads retained upstream stage output
//! from a running or recently finished multi-stage shell pipeline.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aither_core::llm::{Tool, ToolContext, ToolResult};
use aither_sandbox::{
    JobStatus, Jobs, PipelineReadError, PipelineSnapshot, StageStatus, StageStreamSelector,
};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ipc::{Role, SessionShell};

/// Earlier shells' [`Jobs`] handles preserved across main-agent rebuilds while
/// their retained pipeline stages remain within the retention window.
#[derive(Debug, Clone, Default)]
pub struct PriorJobs(Arc<Mutex<Vec<Jobs>>>);

impl PriorJobs {
    /// Replaces the retained prior-shell handles with `jobs` (oldest first).
    pub fn set(&self, jobs: Vec<Jobs>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = jobs;
    }

    fn snapshot(&self) -> Vec<Jobs> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Read or list retained stages of a multi-stage pipeline by its `PID`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PipeArgs {
    /// Process ID returned by the pipeline command.
    #[schemars(extend("x-positional" = true))]
    pub pid: u32,
    /// Zero-based stage index within the pipeline (`0` is the first stage).
    /// Omit (or pass `--list`) to list stages and which ones were retained.
    #[serde(default)]
    #[schemars(extend("x-positional" = true))]
    pub step: Option<usize>,
    /// List the pipeline's stages, status, sizes, and retention instead of
    /// printing stage output.
    #[serde(default)]
    pub list: bool,
    /// Read only the stage's pipeline `stdout` stream.
    #[serde(default)]
    pub stdout: bool,
    /// Read only the stage's unredirected `stderr` stream.
    #[serde(default)]
    pub stderr: bool,
    /// Output the stage's verbatim native bytes (preserving command-authored
    /// ANSI and blank lines while stripping harness-injected color).
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug)]
struct StepRow {
    step: usize,
    command: String,
    state: String,
    lines: usize,
    bytes: usize,
    retained: bool,
}

#[derive(Debug, Template)]
#[template(path = "pipe_steps.txt", escape = "none")]
struct PipeStepsText {
    pid: u32,
    status: String,
    remaining_secs: Option<u64>,
    stages: Vec<StepRow>,
}

#[derive(Debug, Template)]
#[template(path = "shell/pipeline_error.txt", escape = "none")]
struct PipelineErrorText<'a> {
    error: &'a PipelineReadError,
}

/// Errors raised by the `pipe` command.
#[derive(Debug, thiserror::Error)]
pub enum PipeToolError {
    /// Runtime has no shell attached.
    #[error("pipe needs a session shell, which this runtime has none")]
    NoShell,
    /// Caller passed both `--stdout` and `--stderr`.
    #[error(
        "--stdout and --stderr cannot be combined; omit both to read interleaved stdout and stderr"
    )]
    ConflictingStreams,
    /// Stage lookup or read failed.
    #[error("{0}")]
    Pipeline(String),
}

fn render_pipeline_error(error: &PipelineReadError) -> String {
    PipelineErrorText { error }
        .render()
        .expect("pipeline error template renders")
        .trim_end_matches('\n')
        .to_owned()
}

fn format_job_status(status: &JobStatus) -> String {
    match status {
        JobStatus::Running => "running".to_owned(),
        JobStatus::Completed { exit_code, .. } => format!("exit {exit_code}"),
        JobStatus::Failed { error, .. } => format!("failed: {error}"),
        JobStatus::Killed { .. } => "killed".to_owned(),
    }
}

fn format_stage_state(status: StageStatus, own_duration: Option<std::time::Duration>) -> String {
    match status {
        StageStatus::Pending => "pending".to_owned(),
        StageStatus::Running => "running".to_owned(),
        StageStatus::Exited { code } => match own_duration {
            Some(dur) if dur.as_secs() >= 1 => format!("exit {code}, {}s", dur.as_secs()),
            Some(dur) => format!("exit {code}, {}ms", dur.as_millis()),
            None => format!("exit {code}"),
        },
    }
}

fn render_snapshot(snapshot: PipelineSnapshot) -> String {
    let now = Instant::now();
    let remaining_secs = snapshot
        .expires_at
        .map(|exp| exp.saturating_duration_since(now).as_secs());
    let stages = snapshot
        .stages
        .into_iter()
        .map(|stage| StepRow {
            step: stage.step,
            command: stage.command,
            state: format_stage_state(stage.status, stage.own_duration),
            lines: stage.total_lines,
            bytes: stage.stdout_bytes + stage.stderr_bytes,
            retained: stage.retained,
        })
        .collect();
    PipeStepsText {
        pid: snapshot.pid,
        status: format_job_status(&snapshot.job_status),
        remaining_secs,
        stages,
    }
    .render()
    .expect("pipe steps template renders")
}

/// The `pipe` command handler bound to the calling launch's shell.
#[derive(Clone)]
pub struct PipeTool {
    shell: Option<SessionShell>,
    prior_jobs: Option<PriorJobs>,
}

impl std::fmt::Debug for PipeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeTool").finish_non_exhaustive()
    }
}

impl PipeTool {
    /// Creates a `pipe` handler for `role`, searching `shell` first and, for
    /// the main agent, any `prior_jobs` from earlier turns' shells.
    #[must_use]
    pub fn new(shell: Option<SessionShell>, role: Role, prior_jobs: PriorJobs) -> Self {
        Self {
            shell,
            prior_jobs: (role == Role::Main).then_some(prior_jobs),
        }
    }

    fn candidate_registries(&self) -> Result<Vec<Jobs>, PipeToolError> {
        let mut out = Vec::new();
        if let Some(shell) = &self.shell {
            out.push(shell.jobs());
        }
        if let Some(prior) = &self.prior_jobs {
            let mut older = prior.snapshot();
            older.reverse();
            out.extend(older);
        }
        if out.is_empty() {
            return Err(PipeToolError::NoShell);
        }
        Ok(out)
    }

    async fn lookup_snapshot(&self, pid: u32) -> Result<PipelineSnapshot, PipeToolError> {
        let registries = self.candidate_registries()?;
        let mut best_error: Option<PipelineReadError> = None;
        for jobs in registries {
            match jobs.pipeline(pid).await {
                Ok(snapshot) => return Ok(snapshot),
                Err(err) => {
                    if best_error
                        .as_ref()
                        .is_none_or(|prev| matches!(prev, PipelineReadError::UnknownJob { .. }))
                    {
                        best_error = Some(err);
                    }
                }
            }
        }
        let err = best_error.unwrap_or(PipelineReadError::UnknownJob { pid });
        Err(PipeToolError::Pipeline(render_pipeline_error(&err)))
    }

    async fn read_stage(
        &self,
        pid: u32,
        step: usize,
        selector: StageStreamSelector,
        raw: bool,
    ) -> Result<Vec<u8>, PipeToolError> {
        let registries = self.candidate_registries()?;
        let mut best_error: Option<PipelineReadError> = None;
        for jobs in registries {
            match jobs.read_pipeline_stage(pid, step, selector, raw).await {
                Ok(bytes) => return Ok(bytes),
                Err(err) => {
                    if best_error
                        .as_ref()
                        .is_none_or(|prev| matches!(prev, PipelineReadError::UnknownJob { .. }))
                    {
                        best_error = Some(err);
                    }
                }
            }
        }
        let err = best_error.unwrap_or(PipelineReadError::UnknownJob { pid });
        Err(PipeToolError::Pipeline(render_pipeline_error(&err)))
    }
}

impl Tool for PipeTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("pipe")
    }

    type Arguments = PipeArgs;
    type Res = ToolResult;
    type Error = PipeToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        if args.stdout && args.stderr {
            return Err(PipeToolError::ConflictingStreams);
        }
        if args.list || args.step.is_none() {
            let snapshot = self.lookup_snapshot(args.pid).await?;
            return Ok(ToolResult::text(render_snapshot(snapshot)));
        }
        let step = args.step.expect("checked Some above");
        let selector = if args.stdout {
            StageStreamSelector::Stdout
        } else if args.stderr {
            StageStreamSelector::Stderr
        } else {
            StageStreamSelector::Combined
        };
        let bytes = self.read_stage(args.pid, step, selector, args.raw).await?;
        Ok(ToolResult::text(
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    }
}
