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

use aither_agent::AgentEvent;
use async_channel::{Receiver, Sender, unbounded};
use executor_core::{Executor, Task};
use futures_lite::StreamExt;

use crate::MayAgent;

/// Events sent from the agent background task to the TUI.
#[derive(Debug)]
pub enum TuiEvent {
    /// Agent produced a streaming event.
    Agent(Result<AgentEvent, aither_agent::AgentError>),
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
        attachments: Vec<url::Url>,
        event_tx: Sender<TuiEvent>,
    },
    /// Cancel current processing.
    Cancel,
    /// Shutdown the worker.
    Shutdown,
}

/// Handle to control a running agent worker.
pub struct AgentWorker {
    /// Sender for commands to the worker.
    command_tx: Sender<WorkerCommand>,
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
    pub fn run(&self, prompt: String, attachments: Vec<url::Url>) -> AgentHandle {
        const ERR_WORKER_CHANNEL_CLOSED: &str =
            include_str!("../texts/agent_worker_channel_closed.txt");
        let (event_tx, event_rx) = unbounded();
        self.command_tx
            .try_send(WorkerCommand::Run {
                prompt,
                attachments,
                event_tx,
            })
            .expect(ERR_WORKER_CHANNEL_CLOSED.trim());
        AgentHandle {
            event_rx,
            command_tx: self.command_tx.clone(),
        }
    }

    /// Shutdown the worker task.
    pub fn shutdown(&self) {
        let _ = self.command_tx.try_send(WorkerCommand::Shutdown);
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
pub fn spawn_worker<E: Executor + 'static>(executor: E, agent: MayAgent) -> AgentWorker {
    let (command_tx, command_rx) = unbounded::<WorkerCommand>();

    executor.spawn(run_worker(agent, command_rx)).detach();

    AgentWorker { command_tx }
}

/// The worker loop that owns and runs the agent.
async fn run_worker(mut agent: MayAgent, command_rx: Receiver<WorkerCommand>) {
    loop {
        // Wait for a command
        let Ok(command) = command_rx.recv().await else {
            // Channel closed, shutdown
            break;
        };

        match command {
            WorkerCommand::Run {
                prompt,
                attachments,
                event_tx,
            } => {
                run_agent_prompt(&mut agent, &prompt, attachments, &command_rx, &event_tx).await;
            }
            WorkerCommand::Cancel => {
                // No active processing, ignore
            }
            WorkerCommand::Shutdown => {
                agent.close_shell_sessions().await;
                break;
            }
        }
    }
}

/// Run a single prompt and stream events.
async fn run_agent_prompt(
    agent: &mut MayAgent,
    prompt: &str,
    attachments: Vec<url::Url>,
    command_rx: &Receiver<WorkerCommand>,
    event_tx: &Sender<TuiEvent>,
) {
    let mut stream = std::pin::pin!(agent.run(prompt, attachments));
    let mut cancelled = false;

    loop {
        // Race between stream event and cancel command
        let event = futures_lite::future::or(
            async {
                match stream.next().await {
                    Some(result) => Some(TuiEvent::Agent(result)),
                    None => Some(TuiEvent::Complete),
                }
            },
            async {
                // Check for cancel command (non-blocking peek then async recv)
                if let Ok(cmd) = command_rx.try_recv() {
                    if matches!(cmd, WorkerCommand::Cancel | WorkerCommand::Shutdown) {
                        return None;
                    }
                }
                // Wait for cancel (this will also catch shutdown)
                loop {
                    if let Ok(cmd) = command_rx.recv().await {
                        match cmd {
                            WorkerCommand::Cancel | WorkerCommand::Shutdown => return None,
                            WorkerCommand::Run { .. } => {
                                // Ignore new run during processing
                                continue;
                            }
                        }
                    } else {
                        // Channel closed
                        return None;
                    }
                }
            },
        )
        .await;

        match event {
            Some(TuiEvent::Complete) => {
                let _ = event_tx.send(TuiEvent::Complete).await;
                break;
            }
            Some(tui_event) => {
                if event_tx.send(tui_event).await.is_err() {
                    // Receiver dropped, stop processing
                    break;
                }
            }
            None => {
                // Cancelled
                cancelled = true;
                break;
            }
        }
    }

    if cancelled {
        // Send completion after cancel so TUI knows processing stopped
        let _ = event_tx.send(TuiEvent::Complete).await;
    }
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
