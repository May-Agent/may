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

use aither_agent::{CompactResult, TimedEvent};
use aither_core::llm::Attachment;
use async_channel::{Receiver, Sender, unbounded};
use executor_core::{Executor, Task};
use futures_lite::StreamExt;

use crate::MayAgent;
use crate::agent::{ContextReport, RequestParameterizedModel};
use crate::db::ChatDb;

/// Events sent from the agent background task to the TUI.
#[derive(Debug)]
pub enum TuiEvent {
    /// Agent produced a streaming event.
    Agent(Result<TimedEvent, aither_agent::AgentError>),
    /// Compaction request finished.
    Compacted(Result<Option<CompactResult>, aither_agent::AgentError>),
    /// Context-window measurement for `/context` is ready.
    ContextReport(Result<ContextReport, aither_agent::AgentError>),
    /// Agent stream completed (ready for next prompt).
    Complete,
    /// Agent encountered a fatal error.
    Error(String),
}

/// Command sent from main thread to agent worker.
enum WorkerCommand {
    /// Run a prompt.
    Run {
        prompt: String,
        hidden_context: Option<String>,
        attachments: Vec<Attachment>,
        event_tx: Sender<TuiEvent>,
    },
    /// Compact conversation history.
    Compact {
        focus: Option<String>,
        event_tx: Sender<TuiEvent>,
    },
    /// Measure the context window for `/context`.
    ContextReport { event_tx: Sender<TuiEvent> },
    /// Cancel current processing.
    Cancel,
    /// Shutdown the worker.
    Shutdown,
}

/// Handle to control a running agent worker.
pub struct AgentWorker {
    /// Sender for commands to the worker.
    command_tx: Sender<WorkerCommand>,
    /// Background-job registry shared with the session's terminal tool.
    job_registry: aither_sandbox::JobRegistry,
    /// Registry of in-flight `subagent` command runs.
    subagent_runs: aither_agent::specialized::SubagentRuns,
    /// Model clone for `/btw` side questions.
    btw_llm: RequestParameterizedModel<aither_cloud::CloudProvider>,
    /// Tracker shared with the session's `subagent` IPC command.
    subagent_tracker: crate::agent::SubagentTracker,
}

impl std::fmt::Debug for AgentWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentWorker").finish_non_exhaustive()
    }
}

impl AgentWorker {
    /// Start processing a prompt.
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
    ) -> AgentHandle {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded();
        self.command_tx
            .try_send(WorkerCommand::Run {
                prompt,
                hidden_context,
                attachments,
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
    pub fn compact(&self, focus: Option<String>) -> AgentHandle {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded();
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
    pub fn context_report(&self) -> AgentHandle {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded();
        self.command_tx
            .try_send(WorkerCommand::ContextReport { event_tx })
            .unwrap_or_else(|_| panic!("{}", ERR_WORKER_CHANNEL_CLOSED.trim()));
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Signal the worker task to shut down.
    pub fn shutdown(&self) {
        let _ = self.command_tx.try_send(WorkerCommand::Shutdown);
    }

    /// Registry of background terminal jobs for this worker's session.
    #[must_use]
    pub const fn job_registry(&self) -> &aither_sandbox::JobRegistry {
        &self.job_registry
    }

    /// Registry of in-flight subagent runs — list or cancel them.
    #[must_use]
    pub const fn subagent_runs(&self) -> &aither_agent::specialized::SubagentRuns {
        &self.subagent_runs
    }

    /// Model clone for `/btw` side questions.
    #[must_use]
    pub const fn btw_llm(&self) -> &RequestParameterizedModel<aither_cloud::CloudProvider> {
        &self.btw_llm
    }

    /// Live tracker of this worker's `subagent` command invocations.
    #[must_use]
    pub const fn subagent_tracker(&self) -> &crate::agent::SubagentTracker {
        &self.subagent_tracker
    }
}

impl Drop for AgentWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Handle to receive events from a running agent task.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    /// Receiver for events from the agent.
    event_rx: Receiver<TuiEvent>,
    /// Sender for commands (for cancel).
    command_tx: Sender<WorkerCommand>,
}

impl AgentHandle {
    /// Cancel the current agent processing.
    pub fn cancel(&self) {
        let _ = self.command_tx.try_send(WorkerCommand::Cancel);
    }

    /// Check if there are pending events without blocking.
    #[must_use]
    pub fn try_recv(&self) -> Option<TuiEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Receive the next event asynchronously.
    pub async fn recv(&self) -> Option<TuiEvent> {
        self.event_rx.recv().await.ok()
    }
}

/// Spawn an agent worker that owns the agent and processes prompts.
///
/// The agent is moved into a background task. Use the returned `AgentWorker`
/// to send prompts and receive events.
pub fn spawn_worker<E: Executor + 'static>(
    executor: E,
    agent: MayAgent,
    snapshot_db: ChatDb,
) -> AgentWorker {
    let (command_tx, command_rx) = unbounded::<WorkerCommand>();
    let job_registry = agent.job_registry();
    let subagent_runs = agent.subagent_runs();
    let btw_llm = agent.btw_llm();
    let subagent_tracker = agent.subagent_tracker();

    executor
        .spawn(run_worker(
            agent,
            command_rx,
            command_tx.clone(),
            snapshot_db,
        ))
        .detach();

    AgentWorker {
        command_tx,
        job_registry,
        subagent_runs,
        btw_llm,
        subagent_tracker,
    }
}

/// The worker loop that owns and runs the agent.
async fn run_worker(
    mut agent: MayAgent,
    command_rx: Receiver<WorkerCommand>,
    command_tx: Sender<WorkerCommand>,
    snapshot_db: ChatDb,
) {
    loop {
        // Wait for a command; an idle session is compacted on the way.
        let Some(command) = next_command(&mut agent, &command_rx, &snapshot_db).await else {
            // Channel closed, shutdown
            break;
        };

        match command {
            WorkerCommand::Run {
                prompt,
                hidden_context,
                attachments,
                event_tx,
            } => {
                let outcome = Box::pin(run_agent_prompt(
                    &mut agent,
                    &prompt,
                    hidden_context.as_deref(),
                    attachments,
                    &command_rx,
                    &command_tx,
                    &event_tx,
                ))
                .await;
                match outcome {
                    WorkOutcome::Finished(()) => {
                        let session_id = agent.session_id().as_str().to_string();
                        let _ = crate::session::persist_agent_context_snapshot(
                            &snapshot_db,
                            &session_id,
                            &mut agent,
                        )
                        .await;
                    }
                    WorkOutcome::Cancelled => {}
                    WorkOutcome::Shutdown => break,
                }
            }
            WorkerCommand::Compact { focus, event_tx } => {
                match race_against_commands(
                    agent.compact(focus.as_deref()),
                    &command_rx,
                    &command_tx,
                )
                .await
                {
                    WorkOutcome::Finished(result) => {
                        if result.is_ok() {
                            let session_id = agent.session_id().as_str().to_string();
                            let _ = crate::session::persist_agent_context_snapshot(
                                &snapshot_db,
                                &session_id,
                                &mut agent,
                            )
                            .await;
                        }
                        let _ = event_tx.send(TuiEvent::Compacted(result)).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    WorkOutcome::Cancelled => {
                        // The compact future was dropped before it could
                        // mutate the conversation; Complete settles the TUI.
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    WorkOutcome::Shutdown => {
                        let _ = event_tx.send(TuiEvent::Complete).await;
                        break;
                    }
                }
            }
            WorkerCommand::ContextReport { event_tx } => {
                let report = agent.context_report().await;
                let _ = event_tx.send(TuiEvent::ContextReport(report)).await;
                let _ = event_tx.send(TuiEvent::Complete).await;
            }
            WorkerCommand::Cancel => {
                // No active processing, ignore
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
    command_rx: &Receiver<WorkerCommand>,
    snapshot_db: &ChatDb,
) -> Option<WorkerCommand> {
    loop {
        let Some(due_in) = agent.idle_compaction_due_in() else {
            return command_rx.recv().await.ok();
        };
        // `Some` is a command (or the closed channel), `None` the elapsed timer.
        let command =
            futures_lite::future::or(async { Some(command_rx.recv().await.ok()) }, async {
                async_io::Timer::after(due_in).await;
                None
            })
            .await;
        if let Some(command) = command {
            return command;
        }
        let compaction =
            futures_lite::future::or(async { Ok(agent.compact_when_idle().await) }, async {
                Err(command_rx.recv().await.ok())
            })
            .await;
        match compaction {
            Err(command) => return command,
            Ok(Ok(None)) => {}
            Ok(Ok(Some(_))) => {
                let session_id = agent.session_id().as_str().to_string();
                let _ =
                    crate::session::persist_agent_context_snapshot(snapshot_db, &session_id, agent)
                        .await;
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "idle compaction failed");
                return command_rx.recv().await.ok();
            }
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
/// stands. A `ContextReport` received meanwhile is held and requeued once
/// the race ends, so it is answered when the worker is idle again — the
/// in-flight work borrows the agent, so a report cannot be measured
/// mid-turn, and requeueing it during the race would hand it straight back
/// to this loop. `Run` and `Compact` commands arriving mid-work are ignored.
async fn race_against_commands<F: Future>(
    work: F,
    command_rx: &Receiver<WorkerCommand>,
    command_tx: &Sender<WorkerCommand>,
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
                WorkerCommand::ContextReport { event_tx } => deferred.push(event_tx),
                WorkerCommand::Run { .. } | WorkerCommand::Compact { .. } => {}
            }
        }
    })
    .await;
    for event_tx in deferred {
        let _ = command_tx.try_send(WorkerCommand::ContextReport { event_tx });
    }
    outcome
}

/// Run a single prompt and stream events.
async fn run_agent_prompt(
    agent: &mut MayAgent,
    prompt: &str,
    hidden_context: Option<&str>,
    attachments: Vec<Attachment>,
    command_rx: &Receiver<WorkerCommand>,
    command_tx: &Sender<WorkerCommand>,
    event_tx: &Sender<TuiEvent>,
) -> WorkOutcome<()> {
    agent.sync_runtime_context().await;
    let mut stream = agent.run_with_hidden_context(prompt, hidden_context, attachments);

    loop {
        // Race between the next stream event and a cancel command
        match race_against_commands(stream.next(), command_rx, command_tx).await {
            WorkOutcome::Finished(Some(result)) => {
                if event_tx.send(TuiEvent::Agent(result)).await.is_err() {
                    // Receiver dropped, stop processing
                    return WorkOutcome::Cancelled;
                }
            }
            WorkOutcome::Finished(None) => {
                let _ = event_tx.send(TuiEvent::Complete).await;
                return WorkOutcome::Finished(());
            }
            WorkOutcome::Cancelled => break,
            WorkOutcome::Shutdown => {
                let _ = event_tx.send(TuiEvent::Complete).await;
                return WorkOutcome::Shutdown;
            }
        }
    }

    // Send completion after cancel so TUI knows processing stopped
    let _ = event_tx.send(TuiEvent::Complete).await;
    WorkOutcome::Cancelled
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
        let handle = AgentHandle {
            event_rx,
            command_tx,
        };
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
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);

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
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        return;
                    }
                    let _ = request_tx.send(()).await;
                    // Hold the connection open without answering.
                    futures_lite::future::pending::<()>().await;
                });
            }
        });

        let llm = crate::agent::RequestParameterizedModel::new(
            aither_cloud::CloudProvider::OpenAI(
                aither_cloud::openai::OpenAI::builder("test-key")
                    .base_url(format!("http://{addr}"))
                    .model("gpt-5")
                    .no_retry()
                    .build(),
            ),
            aither_core::llm::model::Parameters::default(),
        );
        let (mut agent, temp) = crate::agent::may::test_agent(llm, "openai", "gpt-5").await;
        agent.push_message(aither_core::llm::Message::user("first turn"));
        agent.push_message(aither_core::llm::Message::assistant("first answer"));

        let db = crate::db::ChatDb::open(&temp.path().join("worker.db"))
            .await
            .expect("open worker db");
        let worker = spawn_worker(executor_core::DefaultExecutor, agent, db);

        // Baseline report: the worker answers while idle, and the Messages
        // row captures the conversation footprint compaction must preserve.
        let baseline = context_report(&worker, TIMEOUT).await;

        let compact_handle = worker.compact(None);
        tokio::time::timeout(TIMEOUT, request_rx.recv())
            .await
            .expect("compact request never reached the stub server")
            .expect("stub server request channel closed");

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
    async fn context_report(worker: &AgentWorker, timeout: std::time::Duration) -> ContextReport {
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
}
