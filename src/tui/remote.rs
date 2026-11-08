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
    DaemonClient, RunEnd, SessionAgentBody, SessionAgentFrame, SessionIndexSnapshot,
    WireAgentEvent, session_path,
};
use crate::daemon::{CompactResponse, ContextDumpResponse};
use crate::tui::event::{AppEvent, EventSender};

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
    events_cursor: Option<String>,
) -> AgentWorker {
    let (command_tx, command_rx) = unbounded::<WorkerCommand>();
    let (foreign_tx, foreign_rx) = unbounded::<ForeignTurn>();
    let shared = Arc::new(Shared::default());
    let background = RemoteBackground::new(client.clone(), session_id.clone());
    tokio::spawn(follow_session_globals(
        client.clone(),
        session_id.clone(),
        shared.clone(),
        app_events.clone(),
    ));
    tokio::spawn(follow_session(
        client.clone(),
        session_id.clone(),
        shared.clone(),
        Forwarding {
            foreign_tx,
            command_tx: command_tx.clone(),
            app_events,
        },
        events_cursor,
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
    /// The agent's `Complete`, held until the daemon ends the turn: the
    /// daemon writes the turn's rows after its agent completes, and the
    /// client re-reads the history as soon as it sees the turn complete.
    held_complete: Option<TuiEvent>,
    /// This client sent the turn's message.
    own: bool,
    /// The daemon withdrew the turn: nothing that renders a marker follows.
    withdrawn: bool,
}

impl ForwardedTurn {
    const fn new(events: Sender<TuiEvent>, own: bool) -> Self {
        Self {
            events,
            own,
            withdrawn: false,
            saw_error: false,
            held_complete: None,
        }
    }

    /// The daemon withdrew the turn. The turn's own stream carries the news
    /// so it lands before the stream's end; when that stream is gone (the
    /// stop that caused the withdrawal dropped the handle) the app hears of
    /// it directly. The turn's held `Complete` is dropped, since it would
    /// render a marker for a turn that no longer exists.
    async fn withdraw(&mut self, content: String, app_events: &EventSender) {
        self.withdrawn = true;
        self.held_complete = None;
        let own = self.own;
        if let Err(closed) = self.events.send(TuiEvent::Withdrawn { content, own }).await {
            let TuiEvent::Withdrawn { content, .. } = closed.0 else {
                unreachable!("the unsent event is the one that was sent");
            };
            let _ = app_events.try_send(AppEvent::TurnWithdrawn { content, own });
        }
    }

    /// Forward one agent event; a `Complete` waits for [`Self::end`].
    async fn agent(&mut self, event: WireAgentEvent, at: jiff::Timestamp) {
        if self.withdrawn {
            return;
        }
        self.saw_error |= matches!(event, WireAgentEvent::Error { .. });
        let complete = matches!(event, WireAgentEvent::Complete { .. });
        let event = TuiEvent::Agent(event.into_timed(at));
        if complete {
            self.held_complete = Some(event);
            return;
        }
        if let Some(held) = self.held_complete.take() {
            let _ = self.events.send(held).await;
        }
        let _ = self.events.send(event).await;
    }

    /// The daemon ended the turn (or this client saw it end unseen): the
    /// held `Complete` goes out, then the end of the stream.
    async fn end(self) {
        if let Some(held) = self.held_complete {
            let _ = self.events.send(held).await;
        }
        let _ = self.events.send(TuiEvent::Complete).await;
    }
}

/// Follow worktree index coverage for `cwd` for as long as the TUI runs.
/// The session worker is not required: a new TUI has no daemon session
/// until the first message.
pub fn spawn_index_coverage(
    client: DaemonClient,
    cwd: std::path::PathBuf,
    app_events: EventSender,
) {
    tokio::spawn(follow_index(client, cwd, app_events));
}

async fn follow_index(client: DaemonClient, cwd: std::path::PathBuf, app_events: EventSender) {
    let query = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("topics", "session_index")
            .append_pair("cwd", &cwd.display().to_string());
        query.finish()
    };
    loop {
        let mut stream = match client.events(&query).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(%error, "index event stream unavailable");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        while let Ok(Some(event)) = stream.next().await {
            if event.kind != "session_index" {
                continue;
            }
            let Ok(snapshot) = serde_json::from_str::<SessionIndexSnapshot>(&event.data) else {
                continue;
            };
            let _ = app_events.send(AppEvent::IndexCoverage(snapshot)).await;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// The `profile_sessions` event payload.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSessionsFrame {
    profile_sessions: crate::profile::ProfileSessions,
}

/// Follow the session lists of `profile_id` while the task lives: the
/// daemon pushes them whenever a session is archived, opened, starts
/// working or stops.
pub async fn follow_profile_sessions(
    client: DaemonClient,
    profile_id: String,
    app_events: EventSender,
) {
    let query = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("topics", "profile_sessions")
            .append_pair("profileId", &profile_id);
        query.finish()
    };
    loop {
        match client.events(&query).await {
            Ok(mut stream) => {
                while let Ok(Some(event)) = stream.next().await {
                    if event.kind != "profile_sessions" {
                        continue;
                    }
                    let Ok(frame) = serde_json::from_str::<ProfileSessionsFrame>(&event.data)
                    else {
                        continue;
                    };
                    if app_events
                        .send(AppEvent::ProfileSessionsPushed(frame.profile_sessions))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(error) => tracing::debug!(%error, "profile sessions stream unavailable"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// The session's `/api/events` query: only the topics that did not move to
/// the session stream.
fn globals_query(session_id: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("topics", "sleep,user_notification,session_index")
        .append_pair("sessionId", session_id);
    query.finish()
}

/// The session-wide topics the session stream does not carry: sleep
/// status, notifications to the user and index coverage.
async fn follow_session_globals(
    client: DaemonClient,
    session_id: String,
    shared: Arc<Shared>,
    app_events: EventSender,
) {
    let query = globals_query(&session_id);
    while !shared.stopped.load(Ordering::Acquire) {
        let mut stream = match client.events(&query).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(%error, "session event stream unavailable");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        while let Ok(Some(event)) = stream.next().await {
            if shared.stopped.load(Ordering::Acquire) {
                return;
            }
            match event.kind.as_str() {
                "user_notification" => {
                    if let Ok(notification) =
                        serde_json::from_str::<crate::notification::UserNotification>(&event.data)
                    {
                        let _ = app_events
                            .send(AppEvent::UserNotification(Box::new(notification)))
                            .await;
                    }
                }
                "sleep" => {
                    if let Ok(status) =
                        serde_json::from_str::<crate::sleep::SleepStatus>(&event.data)
                    {
                        let _ = app_events.send(AppEvent::Sleep(status)).await;
                    }
                }
                "session_index" => {
                    if let Ok(snapshot) = serde_json::from_str::<SessionIndexSnapshot>(&event.data)
                    {
                        let _ = app_events.send(AppEvent::IndexCoverage(snapshot)).await;
                    }
                }
                _ => {}
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// What the session stream's deltas are applied to: the background
/// snapshot and linked-issue list the REST reads seeded.
#[derive(Default)]
struct StreamState {
    background: SessionBackgroundSnapshot,
    issues: Vec<crate::tui::sidebar::IssueBrief>,
}

impl StreamState {
    /// The `AppEvent` the background state now yields.
    fn background_event(&self) -> AppEvent {
        let (jobs, subagents) = self.background.clone().into_parts();
        AppEvent::BackgroundLoaded { jobs, subagents }
    }

    /// The `AppEvent` the issue list now yields.
    fn issues_event(&self, session_id: &str) -> AppEvent {
        AppEvent::SidebarIssues {
            session_id: session_id.to_string(),
            issues: self.issues.clone(),
        }
    }
}

/// Replaces the item `key` names in `items`, or appends it.
fn upsert<T, K: Eq>(items: &mut Vec<T>, item: T, key: impl Fn(&T) -> K) {
    let item_key = key(&item);
    match items.iter_mut().find(|old| key(old) == item_key) {
        Some(old) => *old = item,
        None => items.push(item),
    }
}

/// The `AppEvent` one `runtime.changed`, `warning`, `reset`, `job.*`,
/// `subagent.*` or `issue.*` frame maps to, applying the delta to `state`
/// first. Turn, suggestion and recap frames dispatch elsewhere.
fn stream_app_event(kind: &str, data: &str, state: &mut StreamState) -> Option<AppEvent> {
    match kind {
        "runtime.changed" => serde_json::from_str::<crate::client::SessionRuntimeEvent>(data)
            .ok()
            .map(|event| AppEvent::SessionRuntime(Box::new(event))),
        "warning" => serde_json::from_str::<crate::client::WarningFrame>(data)
            .ok()
            .map(|frame| AppEvent::Warning {
                session_id: frame.session_id,
                message: frame.message,
            }),
        "reset" => Some(AppEvent::HistoryStale),
        "job.upserted" => serde_json::from_str::<crate::client::JobUpsertedFrame>(data)
            .ok()
            .map(|frame| {
                upsert(&mut state.background.jobs, frame.job, |job| job.pid);
                state.background_event()
            }),
        "job.removed" => serde_json::from_str::<crate::client::JobRemovedFrame>(data)
            .ok()
            .map(|frame| {
                state.background.jobs.retain(|job| job.pid != frame.pid);
                state.background_event()
            }),
        "subagent.upserted" => serde_json::from_str::<crate::client::SubagentUpsertedFrame>(data)
            .ok()
            .map(|frame| {
                upsert(&mut state.background.subagents, frame.subagent, |run| {
                    run.run_id
                });
                state.background_event()
            }),
        "subagent.removed" => serde_json::from_str::<crate::client::SubagentRemovedFrame>(data)
            .ok()
            .map(|frame| {
                state
                    .background
                    .subagents
                    .retain(|run| run.run_id != frame.run_id);
                state.background_event()
            }),
        "issue.upserted" => serde_json::from_str::<crate::client::IssueUpsertedFrame>(data)
            .ok()
            .map(|frame| {
                let session_id = frame.session_id;
                upsert(&mut state.issues, frame.issue, |issue| issue.number);
                state.issues_event(&session_id)
            }),
        "issue.removed" => serde_json::from_str::<crate::client::IssueRemovedFrame>(data)
            .ok()
            .map(|frame| {
                state.issues.retain(|issue| issue.number != frame.key);
                state.issues_event(&frame.session_id)
            }),
        _ => None,
    }
}

/// The opening state snapshots used to carry: the runtime, background and
/// issues the session stream's deltas apply to, read once per attach.
async fn load_initial_state(
    client: &DaemonClient,
    session_id: &str,
    state: &mut StreamState,
    app_events: &EventSender,
) {
    if let Ok(runtime) = client
        .get::<crate::client::SessionRuntimeSnapshot>(&session_path(session_id, "/runtime"))
        .await
    {
        let _ = app_events
            .send(AppEvent::SessionRuntime(Box::new(
                crate::client::SessionRuntimeEvent {
                    session_id: session_id.to_string(),
                    runtime,
                },
            )))
            .await;
    }
    if let Ok(background) = client
        .get::<SessionBackgroundSnapshot>(&session_path(session_id, "/background"))
        .await
    {
        state.background = background;
        let _ = app_events.send(state.background_event()).await;
    }
    if let Ok(frame) = client
        .get::<crate::client::SessionIssuesFrame>(&session_path(session_id, "/issues"))
        .await
    {
        state.issues = frame.issues;
        let _ = app_events.send(state.issues_event(&frame.session_id)).await;
    }
}

async fn follow_session(
    client: DaemonClient,
    session_id: String,
    shared: Arc<Shared>,
    forwarding: Forwarding,
    events_cursor: Option<String>,
) {
    let mut current: Option<ForwardedTurn> = None;
    let mut state = StreamState::default();
    // Cursor the stream continues from: the resume cursor first, then the
    // last frame id received.
    let mut last_frame: Option<String> = events_cursor;
    while !shared.stopped.load(Ordering::Acquire) {
        load_initial_state(&client, &session_id, &mut state, &forwarding.app_events).await;
        let mut stream = match client
            .session_events(&session_id, last_frame.as_deref())
            .await
        {
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
            if let Some(id) = &event.id {
                last_frame = Some(id.clone());
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
                        &forwarding.app_events,
                    )
                    .await;
                }
                "session_turn" => forward_turn_error(&event.data, &mut current).await,
                "session_suggestion" => {
                    if let Ok(frame) =
                        serde_json::from_str::<crate::client::SuggestionFrame>(&event.data)
                    {
                        let _ = forwarding
                            .app_events
                            .send(AppEvent::PromptSuggestion {
                                request_id: frame.request_id,
                                session_id: frame.session_id,
                                text: frame.text,
                            })
                            .await;
                    }
                }
                "session_recap" => {
                    if let Ok(frame) =
                        serde_json::from_str::<crate::client::RecapFrame>(&event.data)
                    {
                        let _ = forwarding
                            .app_events
                            .send(AppEvent::Recap {
                                session_id: frame.session_id,
                                state: frame.state,
                                summary: frame.summary,
                                next_action: frame.next_action,
                            })
                            .await;
                    }
                }
                // Frames this client missed are gone: the turn it followed
                // ended unseen. The turn in progress, if any, follows from
                // its start.
                "reset" => {
                    if let Some(turn) = current.take() {
                        turn.end().await;
                    }
                    if let Some(app_event) = stream_app_event(&event.kind, &event.data, &mut state)
                    {
                        let _ = forwarding.app_events.send(app_event).await;
                    }
                }
                _ => {
                    if let Some(app_event) = stream_app_event(&event.kind, &event.data, &mut state)
                    {
                        let _ = forwarding.app_events.send(app_event).await;
                    }
                }
            }
        }
        // The turn in progress keeps its handle: the reopened stream
        // continues it from the frame after the last one received.
        shared.attached.store(false, Ordering::Release);
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn forward_agent_frame(
    frame: SessionAgentFrame,
    current: &mut Option<ForwardedTurn>,
    shared: &Shared,
    foreign_tx: &Sender<ForeignTurn>,
    command_tx: &Sender<WorkerCommand>,
    app_events: &EventSender,
) {
    match frame.body {
        SessionAgentBody::Start {
            content,
            message_id,
        } => {
            // A turn whose end this client never saw is over.
            if let Some(turn) = current.take() {
                turn.end().await;
            }
            // Turn boundary: the file tracker marks the pre-turn tree
            // under the prompt's row id, whoever started the turn.
            if let Some(message_id) = message_id {
                let _ = app_events.try_send(AppEvent::TurnStarted { message_id });
            }
            let pending = shared.take_pending();
            let own = pending.is_some();
            let events = pending.unwrap_or_else(|| {
                let (events, rx) = unbounded();
                let _ = foreign_tx.try_send(ForeignTurn {
                    handle: AgentHandle::new(rx, command_tx.clone()),
                    content,
                    message_id,
                    started_at: frame.at,
                });
                events
            });
            *current = Some(ForwardedTurn::new(events, own));
        }
        SessionAgentBody::Withdrawn { content, .. } => match current.as_mut() {
            Some(turn) => turn.withdraw(content, app_events).await,
            None => {
                let _ = app_events.try_send(AppEvent::TurnWithdrawn {
                    content,
                    own: false,
                });
            }
        },
        SessionAgentBody::Agent { event } => {
            if let Some(window) = event.context_window() {
                let _ = app_events
                    .send(AppEvent::ContextWindow {
                        session_id: frame.session_id,
                        window,
                    })
                    .await;
            }
            if let Some(turn) = current.as_mut() {
                turn.agent(event, frame.at).await;
            }
        }
        SessionAgentBody::ReasoningEnd { seconds } => {
            if let Some(turn) = current.as_mut() {
                let _ = turn.events.send(TuiEvent::ReasoningEnd { seconds }).await;
            }
        }
        SessionAgentBody::Hold { reason, message } => {
            if let Some(turn) = current.as_mut() {
                let _ = turn.events.send(TuiEvent::Hold { reason, message }).await;
            }
        }
        SessionAgentBody::Retract => {
            if let Some(turn) = current.as_mut() {
                let _ = turn.events.send(TuiEvent::Retract).await;
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
                turn.end().await;
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
                    .delete(&session_path(&session_id, "/turns/current"))
                    .await;
                if let Err(error) = stopped {
                    tracing::warn!(%error, "failed to stop the daemon turn");
                }
            }
            WorkerCommand::Compact { focus, event_tx } => {
                let result: Result<CompactResponse, _> = client
                    .post(
                        &session_path(&session_id, "/actions/compact"),
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
            WorkerCommand::UsageReport { event_tx } => {
                match client
                    .get::<crate::agent::SessionUsageReport>(&session_path(&session_id, "/usage"))
                    .await
                {
                    Ok(report) => {
                        let _ = event_tx.send(TuiEvent::UsageReport(report)).await;
                        let _ = event_tx.send(TuiEvent::Complete).await;
                    }
                    Err(error) => fail(&event_tx, error.to_string()).await,
                }
            }
            WorkerCommand::ContextDump { event_tx } => {
                let result: Result<ContextDumpResponse, _> = client
                    .post(
                        &session_path(&session_id, "/actions/context-dump"),
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
            WorkerCommand::ContextMessages { event_tx } => {
                fail(
                    &event_tx,
                    "the effective context is read inside the May daemon".to_string(),
                )
                .await;
            }
            WorkerCommand::SideModel { event_tx } => {
                fail(
                    &event_tx,
                    "side requests run inside the May daemon".to_string(),
                )
                .await;
            }
            WorkerCommand::Run { event_tx, .. } => {
                fail(
                    &event_tx,
                    "this agent runs in the May daemon; turns go through `send`".to_string(),
                )
                .await;
            }
            // The agent is in the daemon; there is nothing to hand back.
            WorkerCommand::Release { .. } => {}
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
        .post(&session_path(session_id, "/turns"), &body)
        .await;
    if let Err(error) = sent
        && shared.take_pending().is_some()
    {
        fail(&event_tx, error.to_string()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(turn_id: &str, body: SessionAgentBody) -> SessionAgentFrame {
        SessionAgentFrame {
            session_id: "s".to_string(),
            turn_id: turn_id.to_string(),
            at: jiff::Timestamp::now(),
            body,
        }
    }

    fn text(chunk: &str) -> SessionAgentBody {
        SessionAgentBody::Agent {
            event: WireAgentEvent::Text {
                text: chunk.to_string(),
            },
        }
    }

    fn drain_text(rx: &Receiver<TuiEvent>) -> (String, bool) {
        let mut out = String::new();
        let mut completed = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                TuiEvent::Agent(Ok(timed)) => {
                    if let aither_agent::AgentEvent::Text(chunk) = timed.event {
                        out.push_str(&chunk);
                    }
                }
                TuiEvent::Complete => completed = true,
                _ => {}
            }
        }
        (out, completed)
    }

    #[test]
    fn the_globals_query_keeps_only_the_unmoved_topics() {
        let query = globals_query("s 1");
        assert!(query.contains("sessionId=s+1"), "{query}");
        assert!(
            query.contains("sleep%2Cuser_notification%2Csession_index"),
            "{query}"
        );
        assert!(!query.contains("after="), "{query}");
        assert!(!query.contains("session_runtime"), "{query}");
    }

    #[test]
    fn a_job_upserted_then_removed_leaves_the_background_unchanged() {
        let mut state = StreamState::default();
        let job = crate::client::background::WireJob {
            pid: 42,
            task_id: "t".to_string(),
            execution_key: "k".to_string(),
            script: "sleep 1".to_string(),
            mode: aither_sandbox::TerminalMode::Sandboxed,
            language: aither_sandbox::ScriptLanguage::Shell,
            elapsed_ms: 0,
            status: crate::client::background::WireJobStatus::Running,
            output_path: None,
            place: crate::client::background::WirePlace::Host,
        };
        let upserted = stream_app_event(
            "job.upserted",
            &serde_json::to_string(&crate::client::JobUpsertedFrame {
                session_id: "s".to_string(),
                job: job.clone(),
            })
            .expect("frame serializes"),
            &mut state,
        );
        let Some(AppEvent::BackgroundLoaded { jobs, .. }) = upserted else {
            panic!("upsert yields a background event");
        };
        assert_eq!(jobs.len(), 1, "the upserted job joins the state");
        assert_eq!(state.background.jobs, vec![job]);

        let removed = stream_app_event(
            "job.removed",
            &serde_json::to_string(&crate::client::JobRemovedFrame {
                session_id: "s".to_string(),
                pid: 42,
            })
            .expect("frame serializes"),
            &mut state,
        );
        let Some(AppEvent::BackgroundLoaded { jobs, .. }) = removed else {
            panic!("remove yields a background event");
        };
        assert!(jobs.is_empty() && state.background.jobs.is_empty());
    }

    #[test]
    fn an_issue_upserted_then_removed_leaves_the_list_unchanged() {
        let mut state = StreamState::default();
        let issue = crate::tui::sidebar::IssueBrief {
            number: 7,
            title: Some("bug".to_string()),
            closed: false,
        };
        let upserted = stream_app_event(
            "issue.upserted",
            &serde_json::to_string(&crate::client::IssueUpsertedFrame {
                session_id: "s".to_string(),
                issue: issue.clone(),
            })
            .expect("frame serializes"),
            &mut state,
        );
        let Some(AppEvent::SidebarIssues { issues, .. }) = upserted else {
            panic!("upsert yields an issues event");
        };
        assert_eq!(issues, vec![issue]);

        let removed = stream_app_event(
            "issue.removed",
            &serde_json::to_string(&crate::client::IssueRemovedFrame {
                session_id: "s".to_string(),
                key: 7,
            })
            .expect("frame serializes"),
            &mut state,
        );
        let Some(AppEvent::SidebarIssues { issues, .. }) = removed else {
            panic!("remove yields an issues event");
        };
        assert!(issues.is_empty() && state.issues.is_empty());
    }

    #[test]
    fn a_reset_event_stales_the_history() {
        let mut state = StreamState::default();
        assert!(matches!(
            stream_app_event("reset", r#"{"sessionId":"s"}"#, &mut state),
            Some(AppEvent::HistoryStale)
        ));
    }

    /// A turn whose end the client missed completes when the next one
    /// starts, which arrives as a foreign turn.
    #[tokio::test]
    async fn a_turn_without_a_seen_end_completes_when_the_next_starts() {
        let shared = Shared::default();
        let (events, rx) = unbounded();
        shared.set_pending(events);
        let (foreign_tx, foreign_rx) = unbounded();
        let (commands, _command_rx) = unbounded();
        let mut current = None;
        let (app_events, _app_rx) = unbounded();

        for (turn, body) in [
            (
                "t1",
                SessionAgentBody::Start {
                    content: None,
                    message_id: None,
                },
            ),
            ("t1", text("a")),
            (
                "t2",
                SessionAgentBody::Start {
                    content: None,
                    message_id: None,
                },
            ),
        ] {
            forward_agent_frame(
                frame(turn, body),
                &mut current,
                &shared,
                &foreign_tx,
                &commands,
                &app_events,
            )
            .await;
        }

        assert_eq!(drain_text(&rx), ("a".to_string(), true));
        assert!(
            foreign_rx.try_recv().is_ok(),
            "the new turn is a foreign turn"
        );
    }

    const WITHDRAWN: &str = "你好 こんにちは 안녕하세요";

    /// Feeds a withdrawn turn's frames — start, withdrawal, then the agent's
    /// completion and the end. Nobody takes foreign turns, so another
    /// client's turn has no open stream.
    async fn forward_withdrawn_turn(shared: &Shared, app_events: &EventSender) {
        let (foreign_tx, foreign_rx) = unbounded();
        drop(foreign_rx);
        let (commands, _command_rx) = unbounded();
        let mut current = None;
        for body in [
            SessionAgentBody::Start {
                content: Some(WITHDRAWN.to_string()),
                message_id: None,
            },
            SessionAgentBody::Withdrawn {
                content: WITHDRAWN.to_string(),
                attachments: Vec::new(),
            },
            SessionAgentBody::Agent {
                event: WireAgentEvent::Complete {
                    final_text: String::new(),
                    turns: 1,
                },
            },
            SessionAgentBody::End {
                outcome: RunEnd::Cancelled,
            },
        ] {
            forward_agent_frame(
                frame("t1", body),
                &mut current,
                shared,
                &foreign_tx,
                &commands,
                app_events,
            )
            .await;
        }
    }

    /// A turn this client sent, withdrawn while its stream is still open:
    /// the withdrawal arrives on the stream, ahead of its end, and nothing
    /// that renders a marker follows — no held `Complete`, no error.
    #[tokio::test]
    async fn a_withdrawn_turn_forwards_no_marker_before_its_end() {
        let shared = Shared::default();
        let (events, rx) = unbounded();
        shared.set_pending(events);
        let (app_events, app_rx) = unbounded();
        forward_withdrawn_turn(&shared, &app_events).await;

        let seen: Vec<TuiEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            matches!(
                seen.as_slice(),
                [TuiEvent::Withdrawn { content, own: true }, TuiEvent::Complete]
                    if content == WITHDRAWN
            ),
            "{seen:?}"
        );
        assert!(app_rx.try_recv().is_err(), "the stream carried the news");
    }

    /// The stop that caused a withdrawal dropped the turn's handle, or the
    /// turn is another client's: the app hears of it directly, with whether
    /// this client sent the message.
    #[tokio::test]
    async fn a_withdrawn_turn_tells_the_app_who_sent_it() {
        for sent_here in [true, false] {
            let shared = Shared::default();
            let (events, rx) = unbounded();
            if sent_here {
                shared.set_pending(events);
            }
            drop(rx);
            let (app_events, app_rx) = unbounded();
            forward_withdrawn_turn(&shared, &app_events).await;
            let Ok(AppEvent::TurnWithdrawn { content, own }) = app_rx.try_recv() else {
                panic!("the app hears of the withdrawal");
            };
            assert_eq!(content, WITHDRAWN);
            assert_eq!(own, sent_here);
            assert!(app_rx.try_recv().is_err(), "once");
        }
    }

    /// The daemon writes a turn's rows after its agent completes and before
    /// it ends the turn; the client re-reads the history when it sees the
    /// agent complete, so that `Complete` reaches it only with the end.
    #[tokio::test]
    async fn the_agents_completion_reaches_the_turn_only_when_the_daemon_ends_it() {
        let shared = Shared::default();
        let (events, rx) = unbounded();
        shared.set_pending(events);
        let (foreign_tx, _foreign_rx) = unbounded();
        let (commands, _command_rx) = unbounded();
        let mut current = None;
        let (app_events, _app_rx) = unbounded();
        let mut forward = async |body| {
            forward_agent_frame(
                frame("t1", body),
                &mut current,
                &shared,
                &foreign_tx,
                &commands,
                &app_events,
            )
            .await;
        };
        let is_complete = |event: &TuiEvent| {
            matches!(
                event,
                TuiEvent::Agent(Ok(aither_agent::TimedEvent {
                    event: aither_agent::AgentEvent::Complete { .. },
                    ..
                }))
            )
        };

        forward(SessionAgentBody::Start {
            content: None,
            message_id: None,
        })
        .await;
        forward(text("done")).await;
        forward(SessionAgentBody::Agent {
            event: WireAgentEvent::Complete {
                final_text: "done".to_string(),
                turns: 1,
            },
        })
        .await;
        let before_end: Vec<TuiEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(before_end.len(), 1, "only the text arrives before the end");
        assert!(!before_end.iter().any(is_complete));

        forward(SessionAgentBody::End {
            outcome: RunEnd::Completed,
        })
        .await;
        let at_end: Vec<TuiEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(at_end.len(), 2, "{at_end:?}");
        assert!(is_complete(&at_end[0]), "{at_end:?}");
        assert!(matches!(at_end[1], TuiEvent::Complete), "{at_end:?}");
    }
}
