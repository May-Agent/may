//! The `sidekick` IPC command registered as `main_exclusive` for Fusion sessions.

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aither_core::llm::tool::{CallIdentity, ToolDefinition};
use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::fusion::{FusionPairState, FusionService};
use crate::ipc::CommandSet;
use crate::model_selector::ModelSelector;

#[derive(Template)]
#[template(path = "sidekick_result.txt", escape = "none")]
struct SidekickResultTemplate<'a> {
    report: &'a str,
    diff: &'a str,
}

#[derive(Template)]
#[template(path = "sidekick_started.txt", escape = "none")]
struct SidekickStartedTemplate;

#[derive(Template)]
#[template(path = "sidekick_steered.txt", escape = "none")]
struct SidekickSteeredTemplate;

#[derive(Template)]
#[template(path = "sidekick_completed.txt", escape = "none")]
struct SidekickCompletedTemplate<'a> {
    report: &'a str,
    diff: &'a str,
}

/// Renders the hidden completion notice delivered to the lead when an async sidekick turn ends.
///
/// # Panics
/// Panics if the embedded Askama template fails to render.
#[must_use]
pub fn render_sidekick_completed(report: &str, diff: &str) -> String {
    SidekickCompletedTemplate { report, diff }
        .render()
        .expect("sidekick completed template renders")
}

/// Request handed by [`SidekickTool`] to the daemon's [`SidekickExecutor`].
#[derive(Debug, Clone)]
pub struct SidekickRequest {
    /// Parent lead session id.
    pub parent_session_id: String,
    /// Model selector (`provider::model`) configured for the companion sidekick.
    pub sidekick_model: ModelSelector,
    /// Brief or steer message sent to the sidekick.
    pub prompt: String,
    /// When `true`, launch asynchronously and return immediately; when `false`, wait
    /// for completion and return report + incremental `git diff`.
    pub async_run: bool,
}

/// Outcome returned by [`SidekickExecutor::execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidekickOutcome {
    /// Blocking turn completed with its final report and incremental `git diff`.
    Completed {
        /// Sidekick's text report.
        report: String,
        /// Incremental `git diff` against the pre-handoff working-tree baseline.
        diff: String,
    },
    /// Async turn started and acquired the exclusive workspace write lease.
    StartedAsync,
    /// A turn was already running; the message was queued as a steer for the next turn boundary.
    Steered,
}

/// Daemon-side executor backing the `sidekick` IPC command.
pub trait SidekickExecutor: Send + Sync {
    /// Runs, launches asynchronously, or steers the companion sidekick session.
    fn execute<'a>(
        &'a self,
        request: SidekickRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SidekickOutcome, String>> + Send + 'a>>;
}

/// Delegate routine coding, builds, lints, and verification to the persistent Fusion sidekick.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SidekickArgs {
    /// Task brief or mid-run steer instructions for the sidekick.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Path to a UTF-8 file containing the brief or steer instructions (alternative to `--prompt`).
    #[serde(default)]
    pub prompt_file: Option<String>,
    /// Run the sidekick turn asynchronously while granting it an exclusive workspace write lease.
    #[serde(default, rename = "async", alias = "background")]
    pub async_run: bool,
}

/// Tool implementation behind the `sidekick` IPC command.
#[derive(Clone)]
pub struct SidekickTool {
    service: FusionService,
    pair: Arc<FusionPairState>,
    session_id: String,
    sandbox_dir: PathBuf,
}

impl std::fmt::Debug for SidekickTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidekickTool")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SidekickTool {
    /// Creates a `sidekick` tool bound to `session_id`'s Fusion pair.
    #[must_use]
    pub fn new(service: FusionService, session_id: String, sandbox_dir: PathBuf) -> Self {
        let pair = service.pairs().for_parent(&session_id);
        Self {
            service,
            pair,
            session_id,
            sandbox_dir,
        }
    }

    async fn resolve_prompt(&self, args: &SidekickArgs) -> Result<String, String> {
        let inline = args
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let file = args
            .prompt_file
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        match (inline, file) {
            (Some(_), Some(_)) => {
                Err("pass either --prompt or --prompt-file, not both".to_string())
            }
            (None, None) => {
                Err("missing brief: pass --prompt <text> or --prompt-file <path>".to_string())
            }
            (Some(text), None) => Ok(text.to_string()),
            (None, Some(path)) => {
                let resolved = {
                    let candidate = PathBuf::from(path);
                    if candidate.is_absolute() {
                        candidate
                    } else {
                        self.sandbox_dir.join(candidate)
                    }
                };
                let text = async_fs::read_to_string(&resolved).await.map_err(|error| {
                    format!("failed to read prompt file {}: {error}", resolved.display())
                })?;
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Err(format!("prompt file {} is empty", resolved.display()));
                }
                Ok(trimmed.to_string())
            }
        }
    }

    /// Executes `sidekick` for `args`.
    ///
    /// # Errors
    /// Returns an error message when the session is not in Fusion mode or execution fails.
    ///
    /// # Panics
    /// Panics if an embedded Askama response template fails to render.
    pub async fn execute(&self, args: SidekickArgs) -> Result<ToolResult, String> {
        let mode = self.pair.execution_mode();
        let Some(sidekick_model) = mode.sidekick_model().cloned() else {
            return Err(
                "`sidekick` is only available when the session execution mode is `fusion`"
                    .to_string(),
            );
        };
        let prompt = self.resolve_prompt(&args).await?;
        if let Some(tracker) = self.pair.existing_edit_tracker() {
            tracker.mark_sidekick_call();
        }
        let outcome = self
            .service
            .execute(SidekickRequest {
                parent_session_id: self.session_id.clone(),
                sidekick_model,
                prompt,
                async_run: args.async_run,
            })
            .await?;
        let text = match outcome {
            SidekickOutcome::Completed { report, diff } => SidekickResultTemplate {
                report: report.trim(),
                diff: diff.trim(),
            }
            .render()
            .expect("sidekick result template renders"),
            SidekickOutcome::StartedAsync => SidekickStartedTemplate
                .render()
                .expect("sidekick started template renders"),
            SidekickOutcome::Steered => SidekickSteeredTemplate
                .render()
                .expect("sidekick steered template renders"),
        };
        Ok(ToolResult::text(text))
    }
}

/// Error returned by the `sidekick` IPC tool.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SidekickError(pub String);

impl Tool for SidekickTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("sidekick")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Delegate implementation, builds, lints, and verification to the persistent Fusion sidekick, or steer an in-flight sidekick turn.",
        )
    }

    type Arguments = SidekickArgs;
    type Res = ToolResult;
    type Error = SidekickError;

    fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> impl Future<Output = Result<Self::Res, Self::Error>> + Send {
        let this = self.clone();
        async move { this.execute(args).await.map_err(SidekickError) }
    }
}

/// Dynamic [`CommandSet`] exposing `sidekick` while the session is in Fusion mode.
#[derive(Clone)]
pub struct SidekickCommandSet {
    tool: SidekickTool,
    definition: ToolDefinition,
}

impl std::fmt::Debug for SidekickCommandSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidekickCommandSet")
            .field("session_id", &self.tool.session_id)
            .finish_non_exhaustive()
    }
}

impl SidekickCommandSet {
    /// Creates a dynamic `sidekick` command set for `tool`.
    #[must_use]
    pub fn new(tool: SidekickTool) -> Self {
        let definition = ToolDefinition::new(&tool);
        Self { tool, definition }
    }
}

impl CommandSet for SidekickCommandSet {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self.tool.pair.execution_mode().is_fusion() {
            vec![self.definition.clone()]
        } else {
            Vec::new()
        }
    }

    async fn call(
        &self,
        name: &str,
        _call: Option<CallIdentity>,
        arguments: Value,
    ) -> Result<ToolResult, String> {
        if name != "sidekick" {
            return Err(format!("unknown fusion command '{name}'"));
        }
        let args: SidekickArgs = serde_json::from_value(arguments)
            .map_err(|error| format!("invalid `sidekick` arguments: {error}"))?;
        self.tool.execute(args).await
    }
}
