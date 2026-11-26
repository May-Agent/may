//! Asynchronous bash-first delegated workers for May Bots.
//!
//! Each delegated worker runs under `~/.may/bots/<bot_id>/delegates/<agent_id>/`
//! (plus any granted worktrees) with a dedicated **delegated-worker authority
//! level** distinct from ephemeral nested subagents:
//! - Top-level delegated workers can request workspace access, open SSH
//!   connections, mutate issues, and push/open PRs, routing all four gates
//!   (`unsafe`, `ssh`, `publish`, `workspace`) through the owning bot's
//!   [`BotApprovalRouter`] (`decide_approval`).
//! - Nested ephemeral subagents launched inside a delegated worker retain the
//!   restricted subagent role.
//! - Stopping, completing, or failing checkpoints `aither_agent::Context` into
//!   SQLite (`bot_delegates.checkpoint_blob`) independent of provider cache
//!   expiry, and `resume_agent` restores from that checkpoint.

use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aither_agent::{
    Agent, AgentCheckpoint, AgentEvent, Hooks, RunOutcome, TurnBoundaryAction, TurnBoundaryContext,
};
use aither_core::LanguageModel;
use aither_core::llm::Message;
use aither_core::llm::model::Parameters;
use aither_sandbox::{Grants, Interpreter};
use may_db::{ChatDb, NewBotDelegate, NewBotInboxEntry};
use may_harness::harness::approval_pause::ApprovalPause;
use may_protocol::bot::{BotApprovalGate, BotDelegateStatus, BotDelegateSummary, BotInboxKind};
use may_protocol::session_layout::MayHomePaths;

use super::approvals::{ApprovalOutcome, BotApprovalRouter, BotWakeFn};

/// Specification for launching a new delegated worker.
#[derive(Debug, Clone)]
pub struct DelegateSpawnSpec {
    /// Owning bot id.
    pub bot_id: String,
    /// Human-readable label for the worker.
    pub name: String,
    /// Subagent definition name (e.g. `"general"`).
    pub definition: String,
    /// Initial task prompt.
    pub prompt: String,
    /// Additional host worktree directories granted read/write.
    pub worktrees: Vec<String>,
}

/// Errors returned by delegated worker lifecycle operations.
#[derive(Debug, thiserror::Error)]
pub enum DelegateError {
    /// No worker exists with that `agent_id` for this bot.
    #[error("delegated worker `{0}` not found")]
    NotFound(String),
    /// Worker belongs to a different bot.
    #[error("delegated worker `{0}` does not belong to this bot")]
    Unauthorized(String),
    /// Worker cannot be resumed because no checkpoint exists.
    #[error("delegated worker `{0}` has no persisted checkpoint to resume from")]
    NoCheckpoint(String),
    /// Worker is already running.
    #[error("delegated worker `{0}` is already running")]
    AlreadyRunning(String),
    /// Sandbox or runtime launch error.
    #[error("failed to launch delegated worker: {0}")]
    Launch(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] may_db::ChatDbError),
    /// Filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Live handle to an active delegated worker run.
struct RunningDelegate {
    cancel: aither_agent::CancelHandle,
    steer_tx: async_channel::Sender<String>,
    active_command: Arc<Mutex<Option<String>>>,
    started_at: Instant,
    base_elapsed_ms: i64,
}

/// Factory that builds a delegated worker's `aither_agent::Agent` (or runs it),
/// allowing the daemon to supply real `ProviderLlm` + `heel` sandboxed terminal
/// and tests to inject deterministic scripted models.
pub trait DelegateRunner: Send + Sync + 'static {
    /// Drives a delegated worker turn (fresh or restored from `checkpoint`),
    /// recording messages into `db`, honoring `cancel` & `steer_rx`, and
    /// returning the final [`DelegateRunResult`] including the latest checkpoint.
    #[allow(clippy::too_many_arguments)]
    fn run_worker(
        &self,
        bot_id: String,
        agent_id: String,
        name: String,
        definition: String,
        work_dir: PathBuf,
        grants: Grants,
        prompt: Option<String>,
        checkpoint: Option<AgentCheckpoint>,
        approvals: BotApprovalRouter,
        pause: ApprovalPause,
        steer_rx: async_channel::Receiver<String>,
        cancel_out: Arc<Mutex<Option<aither_agent::CancelHandle>>>,
        active_command: Arc<Mutex<Option<String>>>,
    ) -> Pin<Box<dyn Future<Output = DelegateRunResult> + Send + 'static>>;
}

/// Outcome returned when a delegated worker task finishes, fails, or stops.
#[derive(Debug)]
pub struct DelegateRunResult {
    /// Final lifecycle status.
    pub status: BotDelegateStatus,
    /// Summary / output text posted back to `bot_inbox`.
    pub summary: String,
    /// Accumulated prompt tokens.
    pub prompt_tokens: i64,
    /// Accumulated completion tokens.
    pub completion_tokens: i64,
    /// Persisted context checkpoint (independent of provider cache expiry).
    pub checkpoint: Option<AgentCheckpoint>,
}

/// Hooks for an in-process delegated worker: interrupts at turn boundary when a
/// steer message arrives from the owning bot via `send_message`.
pub struct DelegateHooks {
    steer_rx: async_channel::Receiver<String>,
    pending_steers: Arc<Mutex<Vec<String>>>,
    active_command: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for DelegateHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateHooks").finish_non_exhaustive()
    }
}

impl Hooks for DelegateHooks {
    fn before_tool(
        &self,
        call: &aither_agent::ToolInvocation<'_>,
    ) -> impl Future<Output = aither_agent::ToolDecision> + Send {
        let label = if call.name == "terminal" {
            call.arguments
                .get("script")
                .and_then(|v| v.as_str())
                .map_or_else(
                    || "terminal".to_string(),
                    |s| {
                        let line = s.lines().next().unwrap_or(s).trim();
                        format!("terminal: {line}")
                    },
                )
        } else {
            call.name.to_string()
        };
        if let Ok(mut guard) = self.active_command.lock() {
            *guard = Some(label);
        }
        std::future::ready(aither_agent::ToolDecision::Allow)
    }

    fn after_tool(
        &self,
        _outcome: &aither_agent::ToolOutcome,
    ) -> impl Future<Output = aither_agent::AfterTool> + Send {
        if let Ok(mut guard) = self.active_command.lock() {
            *guard = None;
        }
        std::future::ready(aither_agent::AfterTool::Keep)
    }

    fn on_turn_boundary(
        &self,
        _cx: &TurnBoundaryContext<'_>,
    ) -> impl Future<Output = TurnBoundaryAction> + Send {
        let mut drained = Vec::new();
        while let Ok(msg) = self.steer_rx.try_recv() {
            drained.push(msg);
        }
        if !drained.is_empty() {
            if let Ok(mut guard) = self.pending_steers.lock() {
                guard.extend(drained);
            }
            return std::future::ready(TurnBoundaryAction::EndTurn);
        }
        std::future::ready(TurnBoundaryAction::Continue)
    }
}

/// Drives any `aither_agent::Agent` as a delegated worker with checkpointing,
/// timestamped transcript persistence, steering, and cancellation.
#[allow(clippy::too_many_arguments)]
pub async fn drive_delegate_agent<LLM: LanguageModel + 'static>(
    db: &ChatDb,
    bot_id: &str,
    agent_id: &str,
    mut agent: Agent<LLM, DelegateHooks, LLM>,
    initial_prompt: Option<String>,
    pending_steers: Arc<Mutex<Vec<String>>>,
    cancel_out: Arc<Mutex<Option<aither_agent::CancelHandle>>>,
) -> DelegateRunResult {
    let mut prompt_tokens: i64 = 0;
    let mut completion_tokens: i64 = 0;
    let mut final_text = String::new();
    let mut current_prompt = initial_prompt;

    if let Some(ref text) = current_prompt {
        let _ = db
            .append_bot_delegate_message(agent_id, bot_id, "user", text)
            .await;
    }

    loop {
        if let Some(prompt) = current_prompt.take() {
            agent.push_message(Message::user(prompt));
        }
        let run = agent.resume();
        let handle = run.cancel_handle();
        let checkpoints = run.checkpoints();
        if let Ok(mut guard) = cancel_out.lock() {
            *guard = Some(handle);
        }

        let mut text_buf = String::new();
        let mut run_outcome: Option<RunOutcome> = None;

        let driven = run
            .drive((), std::future::pending::<()>(), |(), timed| {
                match timed.event {
                    AgentEvent::BlockDelta {
                        kind: aither_agent::OutputKind::Text,
                        text,
                        ..
                    } => text_buf.push_str(&text),
                    AgentEvent::RequestEnded {
                        usage: Some(usage), ..
                    } => {
                        prompt_tokens = prompt_tokens
                            .saturating_add(i64::from(usage.prompt_tokens.unwrap_or(0)));
                        completion_tokens = completion_tokens
                            .saturating_add(i64::from(usage.completion_tokens.unwrap_or(0)));
                    }
                    AgentEvent::RunEnded { outcome, .. } => {
                        run_outcome = Some(outcome);
                    }
                    _ => {}
                }
                std::future::ready(ControlFlow::<(), _>::Continue(()))
            })
            .await;

        if !text_buf.trim().is_empty() {
            let _ = db
                .append_bot_delegate_message(agent_id, bot_id, "assistant", text_buf.trim())
                .await;
            final_text = text_buf.trim().to_string();
        }

        // Check if steers arrived at turn boundary while active.
        let steers: Vec<String> = pending_steers
            .lock()
            .map_or_default(|mut guard| std::mem::take(&mut *guard));
        if !steers.is_empty() && matches!(run_outcome, Some(RunOutcome::Completed { .. })) {
            let combined = steers.join("\n\n");
            let _ = db
                .append_bot_delegate_message(agent_id, bot_id, "user", &combined)
                .await;
            agent.push_message(Message::user(combined));
            continue;
        }

        let latest_cp = agent
            .export_checkpoint()
            .await
            .map_or_else(|_| checkpoints.latest(), Some);

        return match (driven, run_outcome) {
            (Err(error), _) | (Ok(_), Some(RunOutcome::Failed { error })) => DelegateRunResult {
                status: BotDelegateStatus::Failed,
                summary: error.to_string(),
                prompt_tokens,
                completion_tokens,
                checkpoint: latest_cp,
            },
            (Ok(_), Some(RunOutcome::Cancelled)) => DelegateRunResult {
                status: BotDelegateStatus::Stopped,
                summary: if final_text.is_empty() {
                    "stopped by bot".to_string()
                } else {
                    final_text
                },
                prompt_tokens,
                completion_tokens,
                checkpoint: latest_cp,
            },
            _ => DelegateRunResult {
                status: BotDelegateStatus::Completed,
                summary: if final_text.is_empty() {
                    "completed".to_string()
                } else {
                    final_text
                },
                prompt_tokens,
                completion_tokens,
                checkpoint: latest_cp,
            },
        };
    }
}

/// Builds a [`DelegateHooks`] pair for a delegated worker `Agent`.
#[must_use]
pub fn make_delegate_hooks(
    steer_rx: async_channel::Receiver<String>,
    active_command: Arc<Mutex<Option<String>>>,
) -> (DelegateHooks, Arc<Mutex<Vec<String>>>) {
    let pending_steers = Arc::new(Mutex::new(Vec::new()));
    let hooks = DelegateHooks {
        steer_rx,
        pending_steers: pending_steers.clone(),
        active_command,
    };
    (hooks, pending_steers)
}

/// Production bash-first [`DelegateRunner`] that builds a sandboxed `Shell` +
/// top-level delegated-worker IPC toolkit with all four permission gates routed
/// through [`BotApprovalRouter`].
pub struct BashDelegateRunner<F> {
    db: ChatDb,
    may_dir: PathBuf,
    tool_distribution: Arc<crate::tools::ToolDistribution>,
    resolve_model: Arc<F>,
}

impl<F> std::fmt::Debug for BashDelegateRunner<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashDelegateRunner").finish_non_exhaustive()
    }
}

impl<F, LLM> BashDelegateRunner<F>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    F: Fn(&str) -> Pin<Box<dyn Future<Output = anyhow::Result<(LLM, Parameters)>> + Send>>
        + Send
        + Sync
        + 'static,
{
    /// Creates a bash-first delegated worker runner.
    pub fn new(
        db: ChatDb,
        may_dir: PathBuf,
        tool_distribution: Arc<crate::tools::ToolDistribution>,
        resolve_model: F,
    ) -> Self {
        Self {
            db,
            may_dir,
            tool_distribution,
            resolve_model: Arc::new(resolve_model),
        }
    }
}

impl<F, LLM> DelegateRunner for BashDelegateRunner<F>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    F: Fn(&str) -> Pin<Box<dyn Future<Output = anyhow::Result<(LLM, Parameters)>> + Send>>
        + Send
        + Sync
        + 'static,
{
    fn run_worker(
        &self,
        bot_id: String,
        agent_id: String,
        name: String,
        definition: String,
        work_dir: PathBuf,
        grants: Grants,
        prompt: Option<String>,
        checkpoint: Option<AgentCheckpoint>,
        approvals: BotApprovalRouter,
        pause: ApprovalPause,
        steer_rx: async_channel::Receiver<String>,
        cancel_out: Arc<Mutex<Option<aither_agent::CancelHandle>>>,
        active_command: Arc<Mutex<Option<String>>>,
    ) -> Pin<Box<dyn Future<Output = DelegateRunResult> + Send + 'static>> {
        let db = self.db.clone();
        let may_dir = self.may_dir.clone();
        let tool_distribution = self.tool_distribution.clone();
        let resolve_model = self.resolve_model.clone();
        Box::pin(async move {
            let (llm, params) = match (resolve_model)(&bot_id).await {
                Ok(pair) => pair,
                Err(error) => {
                    return DelegateRunResult {
                        status: BotDelegateStatus::Failed,
                        summary: format!("failed to resolve model for delegated worker: {error}"),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        checkpoint,
                    };
                }
            };
            let tools_dir = MayHomePaths::new(may_dir.clone()).tools_dir();
            let tools = match crate::tools::ToolEnvironments::prepare_host(
                &tool_distribution,
                &tools_dir,
                false,
            )
            .await
            {
                Ok(t) => t,
                Err(error) => {
                    return DelegateRunResult {
                        status: BotDelegateStatus::Failed,
                        summary: format!("failed to prepare sandbox tools: {error}"),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        checkpoint,
                    };
                }
            };

            let worker_approver =
                approvals.approver_for_delegate(bot_id.clone(), agent_id.clone(), pause.clone());
            // Route workspace requests from the delegated worker straight to BotApprovalRouter!
            let (ws_broker, ws_queue) = crate::workspace_request::channel();
            {
                let approvals = approvals.clone();
                let bot_id = bot_id.clone();
                let agent_id = agent_id.clone();
                let pause = pause.clone();
                let grants_live = grants.clone();
                tokio::spawn(async move {
                    while let Some(req) = ws_queue.next().await {
                        let path = req.args.request.path.clone();
                        let reason = req.args.request.reason.clone();
                        let approved = approvals
                            .request(
                                &bot_id,
                                "delegate",
                                &agent_id,
                                BotApprovalGate::Workspace,
                                &path,
                                Some(&reason),
                                &pause,
                                None,
                            )
                            .await
                            .is_ok_and(|outcome| outcome == ApprovalOutcome::Approved);
                        if approved {
                            grants_live.add_writable(PathBuf::from(&path));
                        }
                        let _ = req.respond(approved);
                    }
                });
            }

            let mut registry_builder = crate::ipc::CommandRegistryBuilder::new();
            registry_builder.with_approval_pause(pause.clone());
            registry_builder.configure_tool(
                crate::workspace_request::RequestWorkspaceTool::new(agent_id.clone(), ws_broker)
                    .with_approval_pause(pause.clone()),
            );

            let audit_log = match heel::NetworkAuditLog::file(work_dir.join(".network-audit.log")) {
                Ok(log) => log,
                Err(error) => {
                    return DelegateRunResult {
                        status: BotDelegateStatus::Failed,
                        summary: format!("delegated worker audit log failed: {error}"),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        checkpoint,
                    };
                }
            };
            let budgets = crate::shell::ShellBudgets::of(&crate::config::ShellSettings::default());
            let toolkit = crate::shell::ShellToolkit::of(&budgets);
            let assembly = crate::shell::ShellAssembly {
                tools,
                container: None,
                working_dir: work_dir.clone(),
                grants: grants.clone(),
                priority: heel::ProcessPriority::default(),
                audit_log,
                permissions: worker_approver.clone(),
                env_source: None,
                budgets,
                interpreters: vec![Interpreter::Shell, Interpreter::Python],
                policies: crate::command_policy::PolicySet {
                    managed_bash: false,
                    refused: crate::command_policy::RefusedReads::default(),
                    read_epochs: crate::command_policy::FileReadEpochs::default(),
                    checkout_reads: None,
                    write_lease: None,
                },
                slot: crate::ipc::ShellSlot::new(),
                ssh_servers: Vec::new(),
                ids: None,
                spaces: crate::agent::subagent_spaces::SessionSubagentSpaces::without_clones(),
                pool: None,
                connectivity: None,
                jobs: None,
            };
            let shell = match assembly.build(registry_builder.build()).await {
                Ok(sh) => sh,
                Err(error) => {
                    return DelegateRunResult {
                        status: BotDelegateStatus::Failed,
                        summary: format!("delegated worker shell failed: {error}"),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        checkpoint,
                    };
                }
            };

            let (hooks, pending_steers) = make_delegate_hooks(steer_rx, active_command);
            let mut builder = Agent::builder(llm)
                .request_parameters(params)
                .approver(worker_approver)
                .instruction(format!(
                    "You are delegated worker `{agent_id}` ({name}, definition: {definition}) working for May Bot `{bot_id}` in `{}`. You have full delegated-worker authority; escalations (`unsafe`, `ssh`, `publish`, `workspace`) route to your owning bot.",
                    work_dir.display()
                ))
                .hooks(hooks);
            builder = toolkit.register(builder, &shell, None);

            // Load definition prompt if a bundled subagent definition matches.
            if let Some(def) = crate::bundle::Bundle::subagent_definitions()
                .into_iter()
                .find(|d| d.name == definition)
                && !def.prompt.trim().is_empty()
            {
                builder = builder.instruction(def.prompt);
            }

            let mut agent = builder.build();
            if let Some(cp) = checkpoint
                && let Err(error) = agent.restore_checkpoint(cp)
            {
                tracing::warn!(%agent_id, %error, "failed to restore delegate checkpoint; continuing fresh");
            }
            drive_delegate_agent(
                &db,
                &bot_id,
                &agent_id,
                agent,
                prompt,
                pending_steers,
                cancel_out,
            )
            .await
        })
    }
}

/// Manages all delegated workers across bots.
#[derive(Clone)]
pub struct BotDelegateManager {
    db: ChatDb,
    may_dir: PathBuf,
    approvals: BotApprovalRouter,
    runner: Arc<dyn DelegateRunner>,
    wake_bot: BotWakeFn,
    running: Arc<Mutex<HashMap<String, RunningDelegate>>>,
}

impl std::fmt::Debug for BotDelegateManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotDelegateManager").finish_non_exhaustive()
    }
}

impl BotDelegateManager {
    /// Creates a delegated worker manager.
    #[must_use]
    pub fn new(
        db: ChatDb,
        may_dir: PathBuf,
        approvals: BotApprovalRouter,
        runner: Arc<dyn DelegateRunner>,
        wake_bot: BotWakeFn,
    ) -> Self {
        Self {
            db,
            may_dir,
            approvals,
            runner,
            wake_bot,
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawns a new bash-first delegated worker asynchronously, returning its
    /// stable `word-word-word` `agent_id` immediately.
    ///
    /// # Errors
    /// Returns [`DelegateError`] if directory setup or DB persistence fails.
    pub async fn spawn(
        &self,
        spec: DelegateSpawnSpec,
    ) -> Result<BotDelegateSummary, DelegateError> {
        let agent_id = crate::id::new();
        let paths = MayHomePaths::new(self.may_dir.clone()).bot_paths(&spec.bot_id);
        let work_dir = paths.delegate_dir(&agent_id);
        async_fs::create_dir_all(&work_dir).await?;

        let summary = self
            .db
            .create_bot_delegate(NewBotDelegate {
                agent_id: &agent_id,
                bot_id: &spec.bot_id,
                name: &spec.name,
                definition: &spec.definition,
                work_dir: &work_dir.display().to_string(),
                granted_worktrees: &spec.worktrees,
            })
            .await?;

        self.launch_task(summary.clone(), Some(spec.prompt), None);
        Ok(summary)
    }

    /// Cancels a running delegated worker; its task checkpoints `Context` to SQLite.
    ///
    /// # Errors
    /// Returns [`DelegateError`] if the worker does not exist or belongs to another bot.
    ///
    /// # Panics
    /// Panics if the running-delegates mutex is poisoned.
    pub async fn stop(
        &self,
        bot_id: &str,
        agent_id: &str,
    ) -> Result<BotDelegateSummary, DelegateError> {
        let existing = self
            .db
            .get_bot_delegate(agent_id)
            .await?
            .ok_or_else(|| DelegateError::NotFound(agent_id.to_string()))?;
        if existing.bot_id != bot_id {
            return Err(DelegateError::Unauthorized(agent_id.to_string()));
        }
        let cancel = self
            .running
            .lock()
            .expect("running delegates lock")
            .get(agent_id)
            .map(|r| r.cancel.clone());
        if let Some(handle) = cancel {
            handle.cancel();
        } else if existing.status == BotDelegateStatus::Running {
            self.db
                .update_bot_delegate_state(
                    agent_id,
                    BotDelegateStatus::Stopped,
                    None,
                    existing.elapsed_ms,
                    existing.prompt_tokens,
                    existing.completion_tokens,
                    None,
                    None,
                )
                .await?;
        }
        Ok(self
            .db
            .get_bot_delegate(agent_id)
            .await?
            .unwrap_or(existing))
    }

    /// Resumes a stopped, completed, or failed delegated worker from its
    /// persisted `AgentCheckpoint` in SQLite (independent of cache expiry).
    ///
    /// # Errors
    /// Returns [`DelegateError`] if missing, unauthorized, already running, or lacking checkpoint.
    ///
    /// # Panics
    /// Panics if the running-delegates mutex is poisoned.
    pub async fn resume(
        &self,
        bot_id: &str,
        agent_id: &str,
        prompt: Option<String>,
    ) -> Result<BotDelegateSummary, DelegateError> {
        let existing = self
            .db
            .get_bot_delegate(agent_id)
            .await?
            .ok_or_else(|| DelegateError::NotFound(agent_id.to_string()))?;
        if existing.bot_id != bot_id {
            return Err(DelegateError::Unauthorized(agent_id.to_string()));
        }
        if self
            .running
            .lock()
            .expect("running delegates lock")
            .contains_key(agent_id)
        {
            return Err(DelegateError::AlreadyRunning(agent_id.to_string()));
        }
        let checkpoint = self
            .db
            .get_bot_delegate_checkpoint(agent_id)
            .await?
            .ok_or_else(|| DelegateError::NoCheckpoint(agent_id.to_string()))?;

        self.db
            .update_bot_delegate_state(
                agent_id,
                BotDelegateStatus::Running,
                None,
                existing.elapsed_ms,
                existing.prompt_tokens,
                existing.completion_tokens,
                None,
                None,
            )
            .await?;
        let refreshed = self
            .db
            .get_bot_delegate(agent_id)
            .await?
            .unwrap_or(existing);

        self.launch_task(refreshed.clone(), prompt, Some(checkpoint));
        Ok(refreshed)
    }

    /// Steers a running delegated worker with `message`, or resumes a stopped/finished
    /// worker with `message` if it has a persisted checkpoint.
    ///
    /// # Errors
    /// Returns [`DelegateError`] if the worker is missing or cannot be resumed.
    ///
    /// # Panics
    /// Panics if the running-delegates mutex is poisoned.
    pub async fn message(
        &self,
        bot_id: &str,
        agent_id: &str,
        message: String,
    ) -> Result<&'static str, DelegateError> {
        let existing = self
            .db
            .get_bot_delegate(agent_id)
            .await?
            .ok_or_else(|| DelegateError::NotFound(agent_id.to_string()))?;
        if existing.bot_id != bot_id {
            return Err(DelegateError::Unauthorized(agent_id.to_string()));
        }
        let steer_tx = self
            .running
            .lock()
            .expect("running delegates lock")
            .get(agent_id)
            .map(|r| r.steer_tx.clone());
        if let Some(tx) = steer_tx {
            let _ = tx.try_send(message);
            return Ok("steered running worker");
        }
        self.resume(bot_id, agent_id, Some(message)).await?;
        Ok("resumed worker with message")
    }

    /// Returns a live snapshot of a delegated worker's metrics (elapsed time,
    /// active command, status, tokens).
    ///
    /// # Errors
    /// Returns [`DelegateError`] if not found or unauthorized.
    ///
    /// # Panics
    /// Panics if the running-delegates mutex is poisoned.
    pub async fn inspect(
        &self,
        bot_id: &str,
        agent_id: &str,
    ) -> Result<BotDelegateSummary, DelegateError> {
        let mut summary = self
            .db
            .get_bot_delegate(agent_id)
            .await?
            .ok_or_else(|| DelegateError::NotFound(agent_id.to_string()))?;
        if summary.bot_id != bot_id {
            return Err(DelegateError::Unauthorized(agent_id.to_string()));
        }
        if let Some(running) = self
            .running
            .lock()
            .expect("running delegates lock")
            .get(agent_id)
        {
            summary.status = BotDelegateStatus::Running;
            let live_ms = i64::try_from(running.started_at.elapsed().as_millis()).unwrap_or(0);
            summary.elapsed_ms = running.base_elapsed_ms.saturating_add(live_ms);
            summary.active_command = running
                .active_command
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
        }
        Ok(summary)
    }

    /// Number of currently running delegated workers for `bot_id`.
    ///
    /// # Panics
    /// Panics if the running-delegates mutex is poisoned.
    #[must_use]
    pub fn active_count(&self, bot_id: &str) -> usize {
        // Count live workers; fall back cleanly.
        let running = self.running.lock().expect("running delegates lock");
        // Since keys are agent_ids, callers that need exact per-bot counts also check DB.
        let _ = bot_id;
        running.len()
    }

    fn launch_task(
        &self,
        summary: BotDelegateSummary,
        prompt: Option<String>,
        checkpoint: Option<AgentCheckpoint>,
    ) {
        let (steer_tx, steer_rx) = async_channel::unbounded();
        let active_command: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cancel_slot: Arc<Mutex<Option<aither_agent::CancelHandle>>> =
            Arc::new(Mutex::new(None));
        let work_dir = PathBuf::from(&summary.work_dir);
        let grants = Grants::new();
        grants.add_writable(&work_dir);
        for wt in &summary.granted_worktrees {
            grants.add_writable(PathBuf::from(wt));
        }

        let placeholder_cancel = aither_agent::CancelHandle::default();
        let started_at = Instant::now();
        {
            self.running.lock().expect("running delegates lock").insert(
                summary.agent_id.clone(),
                RunningDelegate {
                    cancel: placeholder_cancel,
                    steer_tx,
                    active_command: active_command.clone(),
                    started_at,
                    base_elapsed_ms: summary.elapsed_ms,
                },
            );
        }

        let db = self.db.clone();
        let approvals = self.approvals.clone();
        let runner = self.runner.clone();
        let wake_bot = self.wake_bot.clone();
        let running_map = self.running.clone();

        tokio::spawn(async move {
            let pause = ApprovalPause::new();
            let agent_id = summary.agent_id.clone();
            let bot_id = summary.bot_id.clone();
            let base_elapsed = summary.elapsed_ms;
            let base_prompt_tokens = summary.prompt_tokens;
            let base_completion_tokens = summary.completion_tokens;

            // Watch for the real cancel handle once the runner builds the Run.
            let sync_map = running_map.clone();
            let sync_slot = cancel_slot.clone();
            let sync_id = agent_id.clone();
            let handle_sync = tokio::spawn(async move {
                loop {
                    if let Some(real_cancel) = sync_slot.lock().ok().and_then(|g| g.clone()) {
                        if let Some(entry) = sync_map.lock().ok().and_then(|mut g| {
                            g.get_mut(&sync_id).map(|e| {
                                e.cancel = real_cancel.clone();
                            })
                        }) {
                            let () = entry;
                        }
                        break;
                    }
                    async_io::Timer::after(std::time::Duration::from_millis(5)).await;
                }
            });

            let result = runner
                .run_worker(
                    bot_id.clone(),
                    agent_id.clone(),
                    summary.name.clone(),
                    summary.definition.clone(),
                    work_dir,
                    grants,
                    prompt,
                    checkpoint,
                    approvals,
                    pause,
                    steer_rx,
                    cancel_slot,
                    active_command,
                )
                .await;

            handle_sync.abort();
            let _ = running_map
                .lock()
                .expect("running delegates lock")
                .remove(&agent_id);

            let elapsed_ms = base_elapsed
                .saturating_add(i64::try_from(started_at.elapsed().as_millis()).unwrap_or(0));
            let total_prompt = base_prompt_tokens.saturating_add(result.prompt_tokens);
            let total_completion = base_completion_tokens.saturating_add(result.completion_tokens);

            let _ = db
                .update_bot_delegate_state(
                    &agent_id,
                    result.status,
                    None,
                    elapsed_ms,
                    total_prompt,
                    total_completion,
                    Some(&result.summary),
                    result.checkpoint.as_ref(),
                )
                .await;

            let inbox_kind = match result.status {
                BotDelegateStatus::Completed => BotInboxKind::DelegateCompleted,
                BotDelegateStatus::Failed
                | BotDelegateStatus::Stopped
                | BotDelegateStatus::Running => BotInboxKind::DelegateFailed,
            };
            let body = format!(
                "Delegated worker `{agent_id}` ({}) ended with status `{}`:\n{}",
                summary.name, result.status, result.summary
            );
            let _ = db
                .enqueue_bot_inbox(NewBotInboxEntry {
                    bot_id: &bot_id,
                    kind: inbox_kind,
                    sender_id: Some(&agent_id),
                    sender_name: Some(&summary.name),
                    body: &body,
                    meta_json: None,
                })
                .await;
            (wake_bot)(&bot_id);
        });
    }
}
