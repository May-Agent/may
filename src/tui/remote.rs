//! The TUI's agent worker when the agent runs in the daemon.
//!
//! [`spawn_remote_worker`] returns an [`AgentWorker`] like the in-process
//! one: turns, `/compact` and `/context` produce the same [`TuiEvent`]s, so
//! the TUI renders a daemon turn exactly as it rendered its own. The worker
//! follows the session's `session_agent` frames for as long as it lives;
//! turns another client or the daemon's queue starts arrive as foreign
//! turns ([`AgentWorker::next_foreign_turn`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_agent::CompactResult;
use async_channel::{Receiver, Sender, unbounded};

use crate::agent::runner::WorkerCommand;
use crate::agent::{AgentHandle, AgentWorker, ForeignTurn, RemoteTurn, TuiEvent};
use crate::client::background::{RemoteBackground, SessionBackgroundSnapshot};
use crate::client::{
    DaemonClient, RunEnd, SessionAgentBody, SessionAgentFrame, WireAgentEvent, session_path,
};
use crate::tui::event::{AppEvent, EventSender};
use crate::ui_server::{CompactResponse, ContextDumpResponse};

/// Wait before reopening a dropped event stream.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How long a send waits for the event stream to be attached.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct Shared {
    /// Event sender of the turn this client sent, until the turn starts.
    pending: Mutex<Option<Sender<TuiEvent>>>,
    /// Whether the event stream is attached.
    attached: AtomicBool,
    /// Set when the worker shuts down.
    stopped: AtomicBool,
}

impl Shared {
    fn take_pending(&self) -> Option<Sender<TuiEvent>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn set_pending(&self, sender: Sender<TuiEvent>) {
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
    }
}

/// An agent worker for `session_id` whose agent runs in the daemon behind
/// `client`.
#[must_use]
pub fn spawn_remote_worker(
    client: DaemonClient,
    session_id: String,
    app_events: EventSender,
) -> AgentWorker {
    let (command_tx, command_rx) = unbounded::<WorkerCommand>();
    let (foreign_tx, foreign_rx) = unbounded::<ForeignTurn>();
    let shared = Arc::new(Shared::default());
    let background = RemoteBackground::new(client.clone(), session_id.clone());
    tokio::spawn(follow_session(
        client.clone(),
        session_id.clone(),
        shared.clone(),
        Forwarding {
            foreign_tx,
            command_tx: command_tx.clone(),
            app_events,
        },
    ));
    tokio::spawn(run_commands(client, session_id, shared, command_rx));
    AgentWorker::remote(command_tx, foreign_rx, background)
}

/// Where the session's events go besides the turn being forwarded.
struct Forwarding {
    foreign_tx: Sender<ForeignTurn>,
    command_tx: Sender<WorkerCommand>,
    app_events: EventSender,
}

/// One turn being forwarded to its handle.
struct ForwardedTurn {
    events: Sender<TuiEvent>,
    saw_error: bool,
}

async fn follow_session(
    client: DaemonClient,
    session_id: String,
    shared: Arc<Shared>,
    forwarding: Forwarding,
) {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("topics", "session_agent,session_turn,session_background")
        .append_pair("sessionId", &session_id)
        .finish();
    let mut current: Option<ForwardedTurn> = None;
    while !shared.stopped.load(Ordering::Acquire) {
        let mut stream = match client.events(&query).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(%error, "session event stream unavailable");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        shared.attached.store(true, Ordering::Release);
        while let Ok(Some(event)) = stream.next().await {
            if shared.stopped.load(Ordering::Acquire) {
                return;
            }
            match event.kind.as_str() {
                "session_agent" => {
                    let Ok(frame) = serde_json::from_str::<SessionAgentFrame>(&event.data) else {
                        continue;
                    };
                    forward_agent_frame(
                        frame,
                        &mut current,
                        &shared,
                        &forwarding.foreign_tx,
                        &forwarding.command_tx,
                    )
                    .await;
                }
                "session_turn" => forward_turn_error(&event.data, &mut current).await,
                "session_background" => {
                    if let Ok(snapshot) =
                        serde_json::from_str::<SessionBackgroundSnapshot>(&event.data)
                    {
                        let (jobs, subagents) = snapshot.into_parts();
                        let _ = forwarding
                            .app_events
                            .send(AppEvent::BackgroundLoaded { jobs, subagents })
                            .await;
                    }
                }
                _ => {}
            }
        }
        shared.attached.store(false, Ordering::Release);
        // A turn cut off by the dropped stream ends here; its rows are in
        // the session history.
        if let Some(turn) = current.take() {
            let _ = turn.events.send(TuiEvent::Complete).await;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn forward_agent_frame(
    frame: SessionAgentFrame,
    current: &mut Option<ForwardedTurn>,
    shared: &Shared,
    foreign_tx: &Sender<ForeignTurn>,
    command_tx: &Sender<WorkerCommand>,
) {
    match frame.body {
        SessionAgentBody::Start { content } => {
            let events = shared.take_pending().unwrap_or_else(|| {
                let (events, rx) = unbounded();
                let _ = foreign_tx.try_send(ForeignTurn {
                    handle: AgentHandle::new(rx, command_tx.clone()),
                    content,
                });
                events
            });
            *current = Some(ForwardedTurn {
                events,
                saw_error: false,
            });
        }
        SessionAgentBody::Agent { event } => {
            if let Some(turn) = current.as_mut() {
                turn.saw_error |= matches!(event, WireAgentEvent::Error { .. });
                let _ = turn
                    .events
                    .send(TuiEvent::Agent(event.into_timed(frame.at)))
                    .await;
            }
        }
        SessionAgentBody::End { outcome } => {
            if let Some(turn) = current.take() {
                if outcome == RunEnd::Failed && !turn.saw_error {
                    let _ = turn
                        .events
                        .send(TuiEvent::Error(
                            "the turn failed in the May daemon".to_string(),
                        ))
                        .await;
                }
                let _ = turn.events.send(TuiEvent::Complete).await;
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct TurnFrameHead {
    event: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// A turn that failed before the agent ran (no model, provider down) only
/// reports through the web UI's `error` event; the TUI shows it too.
async fn forward_turn_error(data: &str, current: &mut Option<ForwardedTurn>) {
    let Ok(frame) = serde_json::from_str::<TurnFrameHead>(data) else {
        return;
    };
    if frame.event != "error" {
        return;
    }
    let Some(turn) = current.as_mut() else {
        return;
    };
    if turn.saw_error {
        return;
    }
    turn.saw_error = true;
    let message = frame
        .data
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("the turn failed in the May daemon")
        .to_string();
    let _ = turn.events.send(TuiEvent::Error(message)).await;
}

async fn wait_attached(shared: &Shared) -> bool {
    let deadline = tokio::time::Instant::now() + ATTACH_TIMEOUT;
    while !shared.attached.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    true
}

async fn fail(events: &Sender<TuiEvent>, message: String) {
    let _ = events.send(TuiEvent::Error(message)).await;
    let _ = events.send(TuiEvent::Complete).await;
}

async fn run_commands(
    client: DaemonClient,
    session_id: String,
    shared: Arc<Shared>,
    command_rx: Receiver<WorkerCommand>,
) {
    while let Ok(command) = command_rx.recv().await {
        match command {
            WorkerCommand::Send { turn, event_tx } => {
                send_turn(&client, &session_id, &shared, turn, event_tx).await;
            }
            WorkerCommand::Cancel => {
                let stopped: Result<serde_json::Value, _> = client
                    .post(&session_path(&session_id, "/stop"), &serde_json::json!({}))
                    .await;
                if let Err(error) = stopped {
                    tracing::warn!(%error, "failed to stop the daemon turn");
                }
            }
            WorkerCommand::Compact { focus, event_tx } => {
                let result: Result<CompactResponse, _> = client
                    .post(
                        &session_path(&session_id, "/compact"),
                        &serde_json::json!({ "focus": focus }),
                    )
                    .await;
                match result {
                    Ok(response) => {
                        let compacted = response.compacted.then(|| CompactResult {
                            messages_compacted: response.messages_compacted,
                            messages_remaining: response.messages_remaining,
                            summary: response.summary.unwrap_or_default(),
                        });
                        let _ = event_tx.send(TuiEvent::Compacted(Ok(compacted))).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    Err(error) => fail(&event_tx, error.to_string()).await,
                }
            }
            WorkerCommand::ContextReport { event_tx } => {
                match client
                    .get::<crate::agent::ContextReport>(&session_path(
                        &session_id,
                        "/context-report",
                    ))
                    .await
                {
                    Ok(report) => {
                        let _ = event_tx.send(TuiEvent::ContextReport(Ok(report))).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    Err(error) => fail(&event_tx, error.to_string()).await,
                }
            }
            WorkerCommand::ContextDump { event_tx } => {
                let result: Result<ContextDumpResponse, _> = client
                    .post(
                        &session_path(&session_id, "/context-dump"),
                        &serde_json::json!({}),
                    )
                    .await;
                match result {
                    Ok(response) => {
                        let _ = event_tx
                            .send(TuiEvent::ContextDumpSaved {
                                path: response.path,
                                message_count: response.message_count,
                            })
                            .await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    Err(error) => fail(&event_tx, error.to_string()).await,
                }
            }
            WorkerCommand::Run { event_tx, .. } => {
                fail(
                    &event_tx,
                    "this agent runs in the May daemon; turns go through `send`".to_string(),
                )
                .await;
            }
            WorkerCommand::Shutdown => break,
        }
    }
    shared.stopped.store(true, Ordering::Release);
}

async fn send_turn(
    client: &DaemonClient,
    session_id: &str,
    shared: &Shared,
    turn: RemoteTurn,
    event_tx: Sender<TuiEvent>,
) {
    if !wait_attached(shared).await {
        fail(
            &event_tx,
            "the May daemon's event stream is unavailable".to_string(),
        )
        .await;
        return;
    }
    shared.set_pending(event_tx.clone());
    let body = serde_json::json!({
        "sessionId": session_id,
        "content": turn.content,
        "attachments": turn.attachments,
        "hiddenContextXml": turn.hidden_context_xml,
    });
    let sent: Result<serde_json::Value, _> = client
        .post(&session_path(session_id, "/messages"), &body)
        .await;
    if let Err(error) = sent
        && shared.take_pending().is_some()
    {
        fail(&event_tx, error.to_string()).await;
    }
}
