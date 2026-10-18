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

use aither_agent::{AgentEvent, CompactResult};
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
    Agent(Result<AgentEvent, aither_agent::AgentError>),
    /// Compaction request finished.
    Compacted(Result<Option<CompactResult>, aither_agent::AgentError>),
    /// Context-window measurement for `/context` is ready.
    ContextReport(ContextReport),
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
        // Wait for a command
        let Ok(command) = command_rx.recv().await else {
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
                let completed = Box::pin(run_agent_prompt(
                    &mut agent,
                    &prompt,
                    hidden_context.as_deref(),
                    attachments,
                    &command_rx,
                    &command_tx,
                    &event_tx,
                ))
                .await;
                if completed {
                    let session_id = agent.session_id().as_str().to_string();
                    let _ = crate::session::persist_agent_context_snapshot(
                        &snapshot_db,
                        &session_id,
                        &mut agent,
                    )
                    .await;
                }
            }
            WorkerCommand::Compact { focus, event_tx } => {
                let result = agent.compact(focus.as_deref()).await;
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

/// Run a single prompt and stream events.
async fn run_agent_prompt(
    agent: &mut MayAgent,
    prompt: &str,
    hidden_context: Option<&str>,
    attachments: Vec<Attachment>,
    command_rx: &Receiver<WorkerCommand>,
    command_tx: &Sender<WorkerCommand>,
    event_tx: &Sender<TuiEvent>,
) -> bool {
    agent.sync_runtime_context().await;
    let mut stream = agent.run_with_hidden_context(prompt, hidden_context, attachments);

    loop {
        // Race between stream event and cancel command
        let event = futures_lite::future::or(
            async {
                stream
                    .next()
                    .await
                    .map_or(Some(TuiEvent::Complete), |result| {
                        Some(TuiEvent::Agent(result))
                    })
            },
            async {
                // Check for cancel command (non-blocking peek then async recv)
                if let Ok(cmd) = command_rx.try_recv() {
                    match cmd {
                        WorkerCommand::Cancel | WorkerCommand::Shutdown => return None,
                        // The run stream borrows the agent, so a context
                        // report cannot be measured mid-turn — requeue it to
                        // answer with the post-turn context.
                        WorkerCommand::ContextReport { event_tx } => {
                            let _ = command_tx.try_send(WorkerCommand::ContextReport { event_tx });
                        }
                        // Ignore new run/compact commands during processing
                        WorkerCommand::Run { .. } | WorkerCommand::Compact { .. } => {}
                    }
                }
                // Wait for cancel (this will also catch shutdown)
                loop {
                    if let Ok(cmd) = command_rx.recv().await {
                        match cmd {
                            WorkerCommand::Cancel | WorkerCommand::Shutdown => return None,
                            WorkerCommand::ContextReport { event_tx } => {
                                let _ =
                                    command_tx.try_send(WorkerCommand::ContextReport { event_tx });
                            }
                            WorkerCommand::Run { .. } | WorkerCommand::Compact { .. } => {
                                // Ignore new run/compact commands during processing
                                continue;
                            }
                        }
                    }
                    // Channel closed
                    return None;
                }
            },
        )
        .await;

        match event {
            Some(TuiEvent::Complete) => {
                let _ = event_tx.send(TuiEvent::Complete).await;
                return true;
            }
            Some(tui_event) => {
                if event_tx.send(tui_event).await.is_err() {
                    // Receiver dropped, stop processing
                    return false;
                }
            }
            None => {
                // Cancelled
                break;
            }
        }
    }

    // Send completion after cancel so TUI knows processing stopped
    let _ = event_tx.send(TuiEvent::Complete).await;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
