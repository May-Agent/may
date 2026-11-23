//! Agent runner for background task execution.
//!
//! This module provides a clean separation between the TUI main thread and agent processing:
//! - Main thread handles UI rendering and terminal events only
//! - Agent processing runs in a background task that OWNS the agent
//! - All communication happens through async channels
//!
//! The key insight is that the agent's `run()` method returns a stream that borrows the agent.
//! To spawn processing in a background task, the agent must live in that task.
//!
//! Architecture:
//! ```text
//! Main Thread                          Background Worker Task
//! ┌──────────────┐                     ┌───────────────────────┐
//! │ TUI / Events │ ── prompt ────────▶ │ Agent (owned)         │
//! │              │ ◀─ TuiEvent ─────── │ - run(prompt)         │
//! │              │ ── cancel ────────▶ │ - stream processing   │
//! └──────────────┘                     └───────────────────────┘
//! ```

use std::future::Future;
use std::ops::ControlFlow;

use aither_agent::AgentEvent;
use aither_core::llm::Attachment;
use async_channel::{Receiver, Sender, unbounded};
use executor_core::{Executor, Task};

use crate::MayAgent;
pub use may_client::worker::{
    AgentHandle, AgentWorker, ForeignTurn, RemoteTurn, RunLease, TuiEvent,
};

use crate::agent::CompactionReporter;
use crate::agent::provider::ProviderLlm;
use may_client::client::CompactionPhase;
use may_client::session::{CancelReason, RunOutcome, SessionRunGuard, SessionRuns};
use may_client::worker::ERR_SEND_IN_PROCESS;
pub use may_client::worker::WorkerCommand;
use may_db::{ChatDb, RunFence};
#[cfg(test)]
use may_protocol::context_report::ContextReport;
use may_protocol::model_slot::ModelSlot;

/// The refusal a `Compact` gets when the agent is mid-work: the user sees it
/// at once instead of waiting on the turn.
const ERR_WORKER_BUSY: &str = include_str!("../texts/agent_worker_busy.txt");

/// The refusal a `Send` gets on an in-process worker, idle or mid-work.
/// Spawn an agent worker that owns the agent and processes prompts.
///
/// The agent is moved into a background task. Use the returned `AgentWorker`
/// to send prompts and receive events. Work the worker starts on its own, a
/// requested or idle compaction, runs under a lease acquired from `runs`.
pub fn spawn_worker<E: Executor + 'static>(
    executor: E,
    agent: MayAgent,
    snapshot_db: ChatDb,
    runs: SessionRuns,
) -> AgentWorker<ModelSlot<ProviderLlm>, crate::MayAgent> {
    spawn_idle_worker(executor, agent, snapshot_db, runs, None, None)
}

/// Where an idle worker reports the background commands its earlier runs
/// detached.
#[derive(Debug)]
pub struct BackgroundWakes {
    /// Receives each wake that needs the model.
    pub wakes: Sender<super::BackgroundWake>,
    /// The worker starts waiting on background commands once this fires.
    pub released: crate::session::RunReleased,
}

/// Spawns a worker like [`spawn_worker`] whose idle agent also waits on the
/// background commands its earlier runs detached; each wake that needs the
/// model is sent on `background_wakes`.
pub fn spawn_idle_worker<E: Executor + 'static>(
    executor: E,
    agent: MayAgent,
    snapshot_db: ChatDb,
    runs: SessionRuns,
    background_wakes: Option<BackgroundWakes>,
    compactions: Option<Sender<CompactionPhase>>,
) -> AgentWorker<ModelSlot<ProviderLlm>, crate::MayAgent> {
    let (command_tx, command_rx) =
        unbounded::<WorkerCommand<ModelSlot<ProviderLlm>, crate::MayAgent>>();
    let jobs = agent.jobs();
    let subagent_runs = agent.subagent_runs();
    let languages = agent.language_guard();
    let warmth = agent.cache_warmth();
    let subagent_tracker = agent.subagent_tracker();

    executor
        .spawn(run_worker(
            agent,
            command_rx,
            command_tx.clone(),
            snapshot_db,
            runs,
            background_wakes,
            compactions,
        ))
        .detach();

    AgentWorker {
        command_tx,
        jobs: Some(jobs),
        subagent_runs: Some(subagent_runs),
        languages: Some(languages),
        warmth: Some(warmth),
        subagent_tracker,
        foreign_turns: None,
        background: None,
    }
}

/// The worker loop that owns and runs the agent.
async fn run_worker(
    mut agent: MayAgent,
    command_rx: Receiver<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    command_tx: Sender<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    snapshot_db: ChatDb,
    runs: SessionRuns,
    background_wakes: Option<BackgroundWakes>,
    compactions: Option<Sender<CompactionPhase>>,
) {
    let reporter = || {
        compactions
            .clone()
            .map_or_else(CompactionReporter::silent, CompactionReporter::new)
    };
    loop {
        // Wait for a command; an idle session is compacted on the way, and
        // its detached background commands are collected.
        let Some(command) = next_command(
            &mut agent,
            &command_rx,
            &snapshot_db,
            &runs,
            background_wakes.as_ref(),
            &reporter,
        )
        .await
        else {
            // Channel closed, shutdown
            break;
        };

        match command {
            WorkerCommand::Run {
                prompt,
                hidden_context,
                attachments,
                lease,
                event_tx,
            } => {
                let outcome = Box::pin(run_agent_prompt(
                    &mut agent,
                    PromptRun {
                        prompt,
                        hidden_context,
                        attachments,
                        lease,
                    },
                    &snapshot_db,
                    &command_rx,
                    &command_tx,
                    &event_tx,
                    &reporter(),
                ))
                .await;
                if matches!(outcome, WorkOutcome::Shutdown) {
                    break;
                }
            }
            WorkerCommand::Compact { focus, event_tx } => {
                let session_id = agent.session_id().as_str().to_string();
                let guard = match runs.acquire(&session_id).await {
                    Ok(guard) => guard,
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(error.to_string())).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                        continue;
                    }
                };
                let outcome = race_against_commands(
                    until_stopped(
                        agent.compact_reporting(focus.as_deref(), &reporter()),
                        guard.cancellation(),
                    ),
                    &command_rx,
                    &command_tx,
                )
                .await;
                match outcome {
                    WorkOutcome::Finished(StopRace::Done(result)) => {
                        if result.is_ok() {
                            persist_snapshot(&snapshot_db, guard.fence(), &mut agent).await;
                        }
                        finish_run(guard, RunOutcome::Completed).await;
                        let _ = event_tx.send(TuiEvent::Compacted(result)).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    WorkOutcome::Finished(StopRace::Stopped) | WorkOutcome::Cancelled => {
                        // The compact future was dropped before it could
                        // mutate the conversation; Complete settles the TUI.
                        finish_run(guard, RunOutcome::Cancelled).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    WorkOutcome::Shutdown => {
                        finish_run(guard, RunOutcome::Cancelled).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                        break;
                    }
                }
            }
            WorkerCommand::UsageReport { event_tx } => {
                let _ = event_tx
                    .send(TuiEvent::Error(
                        "usage reports are served by the daemon".to_string(),
                    ))
                    .await;
            }
            WorkerCommand::ContextReport { event_tx } => {
                let report = agent.context_report().await;
                let _ = event_tx.send(TuiEvent::ContextReport(report)).await;
                let _ = event_tx.send(TuiEvent::Complete).await;
            }
            WorkerCommand::ContextDump { event_tx } => {
                let dump = agent.context_dump().await;
                let _ = event_tx.send(TuiEvent::ContextDump(dump)).await;
                let _ = event_tx.send(TuiEvent::Complete).await;
            }
            WorkerCommand::ContextMessages { event_tx } => {
                let messages = agent.context_messages().await;
                let _ = event_tx.send(TuiEvent::ContextMessages(messages)).await;
                let _ = event_tx.send(TuiEvent::Complete).await;
            }
            WorkerCommand::SideModel { event_tx } => {
                let model = agent.side_model().await.map(Box::new);
                let _ = event_tx.send(TuiEvent::SideModel(model)).await;
                let _ = event_tx.send(TuiEvent::Complete).await;
            }
            WorkerCommand::Send { event_tx, .. } => {
                let _ = event_tx
                    .send(TuiEvent::Error(ERR_SEND_IN_PROCESS.to_string()))
                    .await;
                let _ = event_tx.send(TuiEvent::Complete).await;
            }
            WorkerCommand::Cancel => {
                // No active processing, ignore
            }
            WorkerCommand::Release { reply } => {
                let _ = reply.try_send(agent);
                return;
            }
            WorkerCommand::Shutdown => {
                break;
            }
        }
    }
}

/// Waits for the next worker command, compacting the idle session when the
/// agent says its cache is about to expire.
///
/// aither reports no pending idle compaction while a compaction model is set,
/// so this never competes with the idle reassembly that mode runs. A command
/// arriving during the compaction drops it where it stands, which leaves the
/// conversation untouched, and is returned at once. `None` means the command
/// channel closed.
async fn next_command(
    agent: &mut MayAgent,
    command_rx: &Receiver<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    snapshot_db: &ChatDb,
    runs: &SessionRuns,
    background_wakes: Option<&BackgroundWakes>,
    reporter: &(dyn Fn() -> CompactionReporter + Sync),
) -> Option<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>> {
    // A refresh that failed is not retried until the next command, so a
    // provider that refuses them is not asked again at once.
    let mut keep_warm_failed = false;
    loop {
        let compaction_due_in = agent.idle_compaction_due_in();
        let keep_warm_due_in = if keep_warm_failed {
            None
        } else {
            agent.keep_warm_due_in().await
        };
        let step = futures_lite::future::or(
            async { IdleStep::Command(command_rx.recv().await.ok()) },
            futures_lite::future::or(
                async {
                    match compaction_due_in {
                        Some(due_in) => {
                            async_io::Timer::after(due_in).await;
                            IdleStep::Compact
                        }
                        None => std::future::pending().await,
                    }
                },
                futures_lite::future::or(
                    async {
                        match keep_warm_due_in {
                            Some(due_in) => {
                                async_io::Timer::after(due_in).await;
                                IdleStep::KeepWarm
                            }
                            None => std::future::pending().await,
                        }
                    },
                    async {
                        match background_wakes {
                            Some(background) => {
                                // A job finishing before the run that parked the
                                // agent released the session would wake a host
                                // that finds the session held.
                                background.released.wait().await;
                                IdleStep::Background(agent.wait_background().await)
                            }
                            None => std::future::pending().await,
                        }
                    },
                ),
            ),
        )
        .await;
        match step {
            IdleStep::Command(command) => return command,
            IdleStep::KeepWarm => match crate::panic_boundary::round(
                "keep warm",
                keep_warm_idle(agent, command_rx, runs),
            )
            .await
            .unwrap_or(KeepWarmOutcome::Failed)
            {
                KeepWarmOutcome::Refreshed | KeepWarmOutcome::NotDue => {}
                KeepWarmOutcome::Failed => keep_warm_failed = true,
                KeepWarmOutcome::Command(command) => return command,
            },
            IdleStep::Background(wake) => {
                // The wake is recorded in the agent's context; a wake that
                // starts a run asks the host for one.
                if wake.starts_run
                    && let Some(background) = background_wakes
                {
                    let _ = background.wakes.try_send(wake);
                }
                // A wake that starts nothing drops its completions here: the
                // agent has them in its context, and they are taken.
            }
            IdleStep::Compact => {
                if let Some(command) =
                    compact_idle(agent, command_rx, snapshot_db, runs, &reporter()).await
                {
                    return command;
                }
            }
        }
    }
}

/// What ended one idle wait of the worker.
enum IdleStep {
    /// A command arrived, or `None` when the channel closed.
    Command(Option<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>),
    /// The idle compaction is due.
    Compact,
    /// The prompt-cache refresh is due.
    KeepWarm,
    /// A detached background command finished or raised an alert.
    Background(super::BackgroundWake),
}

/// How one idle refresh of the prompt cache ended.
enum KeepWarmOutcome {
    /// A refresh was sent and its usage recorded.
    Refreshed,
    /// The refresh was no longer due when its time came: the background
    /// command that called for it had ended, or a request restarted the
    /// cache entry.
    NotDue,
    /// The provider refused the refresh; none is retried until the next
    /// command.
    Failed,
    /// A command arrived meanwhile, or `None` when the channel closed; the
    /// refresh was dropped where it stood.
    Command(Option<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>),
}

/// Sends the idle agent's prompt-cache refresh and records its usage with
/// the session's. The refresh leaves the conversation untouched, so it needs
/// no run of its own; a command arriving meanwhile drops it.
async fn keep_warm_idle(
    agent: &mut MayAgent,
    command_rx: &Receiver<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    runs: &SessionRuns,
) -> KeepWarmOutcome {
    // The decision is taken again now: the command that called for the
    // refresh may have ended since it was scheduled.
    if agent.keep_warm_due_in().await != Some(std::time::Duration::ZERO) {
        return KeepWarmOutcome::NotDue;
    }
    let started = std::time::Instant::now();
    let refresh = futures_lite::future::or(async { Ok(agent.keep_cache_warm().await) }, async {
        Err(command_rx.recv().await.ok())
    })
    .await;
    let usage = match refresh {
        Err(command) => return KeepWarmOutcome::Command(command),
        Ok(Ok(Some(usage))) => usage,
        Ok(Ok(None)) => return KeepWarmOutcome::NotDue,
        Ok(Err(error)) => {
            tracing::warn!(%error, "failed to refresh the idle prompt cache");
            return KeepWarmOutcome::Failed;
        }
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let entry = may_client::daemon::types::RequestLogEntry::new(
        agent.session_id().as_str(),
        &agent.model_selector().to_string(),
        agent.provider_id(),
        &usage,
        agent.model_context_window(),
    )
    .success(elapsed_ms)
    .keep_warm();
    runs.record_keep_warm_request(entry).await;
    KeepWarmOutcome::Refreshed
}

/// Compacts the idle session as a run of its own. `Some` is a command that
/// arrived meanwhile (or `Some(None)`, the closed channel) and must be
/// returned to the worker loop.
async fn compact_idle(
    agent: &mut MayAgent,
    command_rx: &Receiver<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    snapshot_db: &ChatDb,
    runs: &SessionRuns,
    reporter: &CompactionReporter,
) -> Option<Option<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>> {
    // The compaction rewrites the session's context, so it runs as a run
    // of its own; a session another process is running is left alone.
    let session_id = agent.session_id().as_str().to_string();
    let guard = match runs.acquire(&session_id).await {
        Ok(guard) => guard,
        Err(error) => {
            tracing::info!(%error, "skipping idle compaction");
            return Some(command_rx.recv().await.ok());
        }
    };
    let compaction = futures_lite::future::or(
        async {
            Ok(until_stopped(
                agent.compact_when_idle_reporting(reporter),
                guard.cancellation(),
            )
            .await)
        },
        async { Err(command_rx.recv().await.ok()) },
    )
    .await;
    match compaction {
        Err(command) => {
            finish_run(guard, RunOutcome::Cancelled).await;
            Some(command)
        }
        Ok(StopRace::Stopped) => {
            finish_run(guard, RunOutcome::Cancelled).await;
            None
        }
        Ok(StopRace::Done(Ok(None))) => {
            finish_run(guard, RunOutcome::Completed).await;
            None
        }
        Ok(StopRace::Done(Ok(Some(_)))) => {
            persist_snapshot(snapshot_db, guard.fence(), agent).await;
            finish_run(guard, RunOutcome::Completed).await;
            None
        }
        Ok(StopRace::Done(Err(error))) => {
            tracing::error!(%error, "idle compaction failed");
            finish_run(guard, RunOutcome::Failed).await;
            Some(command_rx.recv().await.ok())
        }
    }
}

/// How in-flight work ended relative to the worker command channel.
enum WorkOutcome<T> {
    /// The work produced its value before a command interrupted it.
    Finished(T),
    /// `Cancel` interrupted the work; the worker returns to idle.
    Cancelled,
    /// `Shutdown` or a closed command channel interrupted the work; the
    /// worker loop must stop.
    Shutdown,
}

/// Race `work` against the worker command channel.
///
/// `Cancel` and `Shutdown` end the race early and drop `work` where it
/// stands. A `ContextReport`, `UsageReport`, `ContextDump`, `Release` or
/// `Run` received meanwhile is held and requeued once the race ends, so it
/// is served when the worker is idle again — the in-flight work borrows
/// the agent, and requeueing it during the race would hand it straight
/// back to this loop. A `Compact` or `Send` has no queued form: its
/// `event_tx` is answered at once with a refusal and a `Complete`.
async fn race_against_commands<F: Future>(
    work: F,
    command_rx: &Receiver<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    command_tx: &Sender<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
) -> WorkOutcome<F::Output> {
    let mut deferred = Vec::new();
    let outcome = futures_lite::future::or(async { WorkOutcome::Finished(work.await) }, async {
        loop {
            let Ok(command) = command_rx.recv().await else {
                // Channel closed — every sender is gone; stop the worker.
                return WorkOutcome::Shutdown;
            };
            match command {
                WorkerCommand::Cancel => return WorkOutcome::Cancelled,
                WorkerCommand::Shutdown => return WorkOutcome::Shutdown,
                // A run queues behind the in-flight work, exactly as an
                // enqueued message does; the agent is released once the
                // work is over; a report is measured against the idle agent.
                WorkerCommand::Run { .. }
                | WorkerCommand::Release { .. }
                | WorkerCommand::ContextReport { .. }
                | WorkerCommand::UsageReport { .. }
                | WorkerCommand::ContextDump { .. }
                | WorkerCommand::ContextMessages { .. }
                | WorkerCommand::SideModel { .. } => deferred.push(command),
                WorkerCommand::Compact { event_tx, .. } => {
                    let _ = event_tx
                        .send(TuiEvent::Error(ERR_WORKER_BUSY.trim().to_string()))
                        .await;
                    let _ = event_tx.send(TuiEvent::Complete).await;
                }
                WorkerCommand::Send { event_tx, .. } => {
                    let _ = event_tx
                        .send(TuiEvent::Error(ERR_SEND_IN_PROCESS.to_string()))
                        .await;
                    let _ = event_tx.send(TuiEvent::Complete).await;
                }
            }
        }
    })
    .await;
    for command in deferred {
        let _ = command_tx.try_send(command);
    }
    outcome
}

/// How work raced against a run's cancellation ended.
enum StopRace<T> {
    /// The work produced its value.
    Done(T),
    /// A stop request or loss of the lease dropped the work.
    Stopped,
}

/// Race `work` against `cancellation`, dropping the work where it stands when
/// the run must stop.
async fn until_stopped<F: Future>(
    work: F,
    cancellation: &Receiver<CancelReason>,
) -> StopRace<F::Output> {
    futures_lite::future::or(async { StopRace::Done(work.await) }, async {
        let _ = cancellation.recv().await;
        StopRace::Stopped
    })
    .await
}

/// Persist the agent's context snapshot as the run holding `fence`. A lost
/// lease is the only expected refusal and is already reported by the run.
async fn persist_snapshot(snapshot_db: &ChatDb, fence: &RunFence, agent: &mut MayAgent) {
    if let Err(error) =
        crate::session::persist_agent_context_snapshot(snapshot_db, fence, agent.inner_mut()).await
    {
        tracing::warn!(session_id = %fence.session_id, %error, "failed to persist the context snapshot");
    }
}

/// End a run the worker started on its own.
async fn finish_run(guard: SessionRunGuard, outcome: RunOutcome) {
    let session_id = guard.session_id().to_string();
    if let Err(error) = guard.finish_idle(outcome).await {
        tracing::error!(%session_id, %error, "failed to release the session run");
    }
}

/// A prompt to run and the lease it runs under.
struct PromptRun {
    prompt: String,
    hidden_context: Option<String>,
    attachments: Vec<Attachment>,
    lease: RunLease,
}

/// Run a single prompt and stream events.
///
/// The context snapshot is persisted before `Complete` is sent: the TUI
/// releases the run's lease on `Complete`, and a snapshot written after that
/// would be refused.
async fn run_agent_prompt(
    agent: &mut MayAgent,
    run: PromptRun,
    snapshot_db: &ChatDb,
    command_rx: &Receiver<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    command_tx: &Sender<WorkerCommand<ModelSlot<ProviderLlm>, MayAgent>>,
    event_tx: &Sender<TuiEvent<ModelSlot<ProviderLlm>>>,
    reporter: &CompactionReporter,
) -> WorkOutcome<()> {
    let PromptRun {
        prompt,
        hidden_context,
        attachments,
        lease,
    } = run;
    agent.sync_runtime_context().await;
    let run = agent.run_with_hidden_context(&prompt, hidden_context.as_deref(), attachments);
    let checkpoints = run.checkpoints();
    let fence = &lease.fence;
    // The run stops between events on the lease's cancellation; a cancel
    // command drops it where it stands. Each checkpoint the run records is
    // persisted before the event reaches the TUI, which releases the lease on
    // `Complete`: a snapshot written after that would be refused.
    let driven = run.drive(
        event_tx,
        async {
            let _ = lease.cancellation.recv().await;
            PromptStop::Stopped
        },
        |event_tx, timed| {
            let checkpoints = &checkpoints;
            async move {
                if matches!(timed.event, AgentEvent::Checkpoint { .. })
                    && let Err(error) =
                        crate::session::persist_run_checkpoint(snapshot_db, fence, checkpoints)
                            .await
                {
                    tracing::warn!(session_id = %fence.session_id, %error, "failed to persist the context snapshot");
                }
                if event_tx.send(TuiEvent::Agent(Ok(timed))).await.is_err() {
                    ControlFlow::Break(PromptStop::ReceiverGone)
                } else {
                    ControlFlow::Continue(event_tx)
                }
            }
        },
    );
    match race_against_commands(driven, command_rx, command_tx).await {
        WorkOutcome::Finished(Ok(ControlFlow::Continue(_))) => {
            // The `compact` tool ended the turn for a compaction: it runs
            // before the lease is released, so its snapshot is accepted.
            if let Err(error) = agent.compact_requested(snapshot_db, fence, reporter).await {
                tracing::error!(session_id = %fence.session_id, %error, "the requested compaction failed");
            }
        }
        WorkOutcome::Finished(Err(error)) => {
            let _ = event_tx.send(TuiEvent::Agent(Err(error))).await;
        }
        // Receiver dropped: nobody is left to tell.
        WorkOutcome::Finished(Ok(ControlFlow::Break(PromptStop::ReceiverGone))) => {
            return WorkOutcome::Cancelled;
        }
        WorkOutcome::Finished(Ok(ControlFlow::Break(PromptStop::Stopped)))
        | WorkOutcome::Cancelled => {
            // The user stopped the run: background completions wait for
            // their next input.
            agent.interrupt();
            // Completion after cancel lets the TUI know processing stopped.
            let _ = event_tx.send(TuiEvent::Complete).await;
            return WorkOutcome::Cancelled;
        }
        WorkOutcome::Shutdown => {
            let _ = event_tx.send(TuiEvent::Complete).await;
            return WorkOutcome::Shutdown;
        }
    }
    let _ = event_tx.send(TuiEvent::Complete).await;
    WorkOutcome::Finished(())
}

/// Why a prompt's run stopped before it finished.
enum PromptStop {
    /// The lease's cancellation stopped it.
    Stopped,
    /// The TUI's event receiver is gone.
    ReceiverGone,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ContextCategory;

    #[test]
    fn test_agent_handle_cancel() {
        // Basic test that cancel doesn't panic
        let (command_tx, _command_rx) = unbounded();
        let (_event_tx, event_rx) = unbounded();
        let handle: AgentHandle<ModelSlot<ProviderLlm>, MayAgent> =
            AgentHandle::new(event_rx, command_tx);
        handle.cancel();
    }

    /// Cancelling an in-flight compaction drops the compact future: the
    /// worker goes back to idle and the conversation is left untouched.
    ///
    /// The agent's model is an `OpenAI` client pointed at a stub HTTP server
    /// that answers `GET /models` (so `profile()` falls back to the
    /// aither-models registry) but accepts a completion request and never
    /// responds — from the agent's side the model's stream never yields.
    #[tokio::test]
    async fn cancel_during_compact_returns_worker_to_idle() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        crate::test_support::install_global_executor();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub server");
        let addr = listener.local_addr().expect("stub server addr");
        // Notified once the completion request arrives — proof that the
        // compaction is in flight against the model.
        let (request_tx, request_rx) = unbounded::<()>();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let request_tx = request_tx.clone();
                tokio::spawn(async move {
                    let mut head = [0u8; 2048];
                    let read = socket.read(&mut head).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&head[..read]);
                    if request
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains("/models"))
                    {
                        let body = serde_json::json!({
                            "data": [{
                                "id": crate::agent::idle_compaction_tests::SESSION_MODEL,
                                "owned_by": "stub",
                                "context_length": 128_000
                            }]
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        return;
                    }
                    let _ = request_tx.send(()).await;
                    // Hold the connection open without answering.
                    futures_lite::future::pending::<()>().await;
                });
            }
        });

        let server = crate::agent::idle_compaction_tests::StubModel {
            base_url: format!("http://{addr}"),
            requests: std::sync::Arc::default(),
        };
        let session = crate::agent::idle_compaction_tests::Session::start(
            &crate::agent::idle_compaction_tests::saved_config(&server, None),
        )
        .await;
        let temp = session.temp.path().to_path_buf();
        let mut agent = session.agent;
        agent.push_message(aither_core::llm::Message::user("first turn"));
        agent.push_message(aither_core::llm::Message::assistant("first answer"));

        let db = crate::db::ChatDb::open(&temp.join("worker.db"))
            .await
            .expect("open worker db");
        let runs =
            crate::session::SessionRuns::new(db.clone(), crate::ui_event_bus::UiEventBus::new());
        let worker = spawn_worker(executor_core::DefaultExecutor, agent, db, runs);

        // Baseline report: the worker answers while idle, and the Messages
        // row captures the conversation footprint compaction must preserve.
        let baseline = context_report(&worker, TIMEOUT).await;

        let compact_handle = worker.compact(None);
        match tokio::time::timeout(TIMEOUT, request_rx.recv()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("stub server request channel closed: {error}"),
            Err(_) => {
                let event = tokio::time::timeout(TIMEOUT, compact_handle.recv()).await;
                panic!(
                    "compact request never reached the stub server; the compaction sent {event:?}"
                );
            }
        }

        compact_handle.cancel();
        match tokio::time::timeout(TIMEOUT, compact_handle.recv()).await {
            Ok(Some(TuiEvent::Complete)) => {}
            other => panic!("expected Complete after cancel, got {other:?}"),
        }
        // Nothing further is sent on a cancelled compaction.
        assert!(
            matches!(
                tokio::time::timeout(TIMEOUT, compact_handle.recv()).await,
                Ok(None)
            ),
            "compaction channel must close after Complete"
        );

        // A following command is served right away, and the conversation is
        // unchanged.
        let after = context_report(&worker, TIMEOUT).await;
        assert_eq!(
            messages_row(&baseline),
            messages_row(&after),
            "cancelled compaction must leave the conversation untouched"
        );

        worker.shutdown();
    }

    /// The `Messages` row of a context report: estimated tokens plus the
    /// "N messages" detail.
    fn messages_row(report: &ContextReport) -> (usize, Option<String>) {
        report
            .categories
            .iter()
            .find(|row| row.kind == ContextCategory::Messages)
            .map(|row| (row.tokens, row.detail.clone()))
            .expect("context report carries a Messages row")
    }

    /// Issue a `/context` command and wait for its report — served only once
    /// the worker is idle.
    async fn context_report(
        worker: &AgentWorker<ModelSlot<ProviderLlm>, MayAgent>,
        timeout: std::time::Duration,
    ) -> ContextReport {
        let handle = worker.context_report();
        let report = async {
            match handle.recv().await {
                Some(TuiEvent::ContextReport(report)) => {
                    report.expect("context report should assemble")
                }
                Some(other) => panic!("unexpected event before context report: {other:?}"),
                None => panic!("context report channel closed"),
            }
        };
        tokio::time::timeout(timeout, report)
            .await
            .expect("context report timed out — worker never became idle")
    }

    /// A `Run` sent while work is in flight is held, then requeued once the
    /// race ends: it runs exactly once, after the in-flight work, exactly as
    /// an enqueued message does.
    #[tokio::test]
    async fn a_run_sent_mid_work_is_deferred_until_the_work_ends() {
        let (command_tx, command_rx) =
            unbounded::<WorkerCommand<ModelSlot<ProviderLlm>, crate::MayAgent>>();
        let race_rx = command_rx.clone();
        let (release_tx, release_rx) = unbounded::<()>();
        let race = tokio::spawn({
            let command_tx = command_tx.clone();
            async move {
                race_against_commands(
                    async { release_rx.recv().await.is_ok() },
                    &race_rx,
                    &command_tx,
                )
                .await
            }
        });

        let (event_tx, _event_rx) = unbounded();
        let (_cancel_tx, cancellation) = unbounded();
        command_tx
            .send(WorkerCommand::Run {
                prompt: "later".to_string(),
                hidden_context: None,
                attachments: Vec::new(),
                lease: RunLease {
                    cancellation,
                    fence: RunFence {
                        session_id: "session".to_string(),
                        run_id: "run".to_string(),
                    },
                },
                event_tx,
            })
            .await
            .expect("the command channel is open");
        // The race consumed the command but must not requeue it mid-work.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            command_rx.try_recv().is_err(),
            "the deferred run must not be requeued mid-work"
        );

        release_tx.send(()).await.expect("release the work");
        assert!(matches!(
            race.await.expect("the race joins"),
            WorkOutcome::Finished(true)
        ));
        match command_rx
            .recv()
            .await
            .expect("the deferred run is requeued")
        {
            WorkerCommand::Run { prompt, .. } => assert_eq!(prompt, "later"),
            _ => panic!("the requeued command is the run"),
        }
        assert!(
            command_rx.try_recv().is_err(),
            "the run is requeued exactly once"
        );
    }

    /// A `Compact` sent while work is in flight is refused at once: an Error
    /// then a Complete on its own event channel, and nothing is queued or run.
    #[tokio::test]
    async fn a_compact_sent_mid_work_is_refused_at_once() {
        let (command_tx, command_rx) =
            unbounded::<WorkerCommand<ModelSlot<ProviderLlm>, crate::MayAgent>>();
        let race_rx = command_rx.clone();
        let (release_tx, release_rx) = unbounded::<()>();
        let race = tokio::spawn({
            let command_tx = command_tx.clone();
            async move {
                race_against_commands(
                    async { release_rx.recv().await.is_ok() },
                    &race_rx,
                    &command_tx,
                )
                .await
            }
        });

        let (event_tx, event_rx) = unbounded();
        command_tx
            .send(WorkerCommand::Compact {
                focus: None,
                event_tx,
            })
            .await
            .expect("the command channel is open");
        match event_rx.recv().await {
            Ok(TuiEvent::Error(message)) => assert!(message.contains("busy")),
            other => panic!("expected the busy error, got {other:?}"),
        }
        match event_rx.recv().await {
            Ok(TuiEvent::Complete) => {}
            other => panic!("expected Complete after the error, got {other:?}"),
        }

        release_tx.send(()).await.expect("release the work");
        assert!(matches!(
            race.await.expect("the race joins"),
            WorkOutcome::Finished(true)
        ));
        assert!(
            command_rx.try_recv().is_err(),
            "the compact is answered, never queued"
        );
    }

    /// A `Send` sent while work is in flight gets the same in-process refusal
    /// the idle loop gives, at once.
    #[tokio::test]
    async fn a_send_sent_mid_work_is_refused_at_once() {
        let (command_tx, command_rx) =
            unbounded::<WorkerCommand<ModelSlot<ProviderLlm>, crate::MayAgent>>();
        let race_rx = command_rx.clone();
        let (release_tx, release_rx) = unbounded::<()>();
        let race = tokio::spawn({
            let command_tx = command_tx.clone();
            async move {
                race_against_commands(
                    async { release_rx.recv().await.is_ok() },
                    &race_rx,
                    &command_tx,
                )
                .await
            }
        });

        let (event_tx, event_rx) = unbounded();
        command_tx
            .send(WorkerCommand::Send {
                turn: RemoteTurn {
                    content: "turn".to_string(),
                    attachments: Vec::new(),
                    hidden_context_xml: None,
                },
                event_tx,
            })
            .await
            .expect("the command channel is open");
        match event_rx.recv().await {
            Ok(TuiEvent::Error(message)) => {
                assert!(message.contains("runs in-process"));
            }
            other => panic!("expected the in-process error, got {other:?}"),
        }
        match event_rx.recv().await {
            Ok(TuiEvent::Complete) => {}
            other => panic!("expected Complete after the error, got {other:?}"),
        }

        release_tx.send(()).await.expect("release the work");
        assert!(matches!(
            race.await.expect("the race joins"),
            WorkOutcome::Finished(true)
        ));
    }

    /// `Shutdown` sent while work is in flight ends the race — the worker
    /// loop breaks on it.
    #[tokio::test]
    async fn a_shutdown_sent_mid_work_stops_the_race() {
        let (command_tx, command_rx) =
            unbounded::<WorkerCommand<ModelSlot<ProviderLlm>, crate::MayAgent>>();
        let race_rx = command_rx.clone();
        let race = tokio::spawn({
            let command_tx = command_tx.clone();
            async move {
                race_against_commands(std::future::pending::<()>(), &race_rx, &command_tx).await
            }
        });

        command_tx
            .send(WorkerCommand::Shutdown)
            .await
            .expect("the command channel is open");
        assert!(matches!(
            race.await.expect("the race joins"),
            WorkOutcome::Shutdown
        ));
    }
}
