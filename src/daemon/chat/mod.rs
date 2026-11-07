mod aux;
mod background;
mod hosts;
mod hub;
mod recap;
mod request_ledger;
mod retry;
mod suggest;
pub use aux::AuxTasks;
pub use background::SessionBackgrounds;
pub use background::spawn_watcher as spawn_background_watcher;
pub use background::{
    cancel_subagent, cancel_subagents, get_background, get_job, kill_job, read_job,
};
pub use hosts::{BtwMessage, BtwRequest, BtwResponse, CompactResponse, ContextDumpResponse};
pub use hosts::{
    ParkedAgents, btw, compact_session, context_dump, context_report, release_agent, usage_report,
};
pub use hub::{TurnFrame, TurnHub, TurnSubscription};
pub use recap::{RecapRecord, RecapSchedules, recap_session};
mod tool_dispatch;
mod turn_events;
mod turn_persister;
#[cfg(test)]
mod warm_aux_tests;

use self::tool_dispatch::{
    handle_browser_request, handle_notify_request, handle_register_skills_request,
    handle_task_request,
};

use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template;
use serde::Serialize;
use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use aither_core::llm::{
    Attachment as LlmAttachment, Message as LlmMessage, Role as LlmRole, ToolResult, Usage,
};
use aither_core::llm::{Failure, FailureKind};
use aither_models::lookup as lookup_model_info;
use aither_sandbox::{AnyContainerExec, BollardContainerExec, ContainerLaunchSpec};

use crate::agent::MayAgent;
use crate::ask_user;
use crate::db::RunFence;
use crate::prompt_mentions;
use crate::quota;
use crate::session::{
    AcquireError, CancelReason, ChatHistory, ChatMessage, ChatMessageStats, MayHomePaths,
    MessageRole, PendingMessageState, PendingUserMessage, PersistedAttachment, RunOutcome,
    SessionId, SessionPaths, SessionRunGuard, SessionRuntimeState, SessionStore,
    TerminalRuntimeState,
};
use crate::task_trigger::EVENT_CHAT_TURN_COMPLETED;
use crate::ui_event_bus::UiEventTopics;

use super::attachments::build_prompt_and_attachments;
use super::browser::ensure_browser_runtime_started;
use super::error::internal_error;
use super::error_chain::error_chain_text;
use super::helpers::{RETRY_BACKOFF_MS, elapsed_ms_saturating, truncate_preview};
use super::message_mapping::api_message_id;
use super::model_resolution::resolve_default_model;
use super::state::{DaemonState, UiToolQueues};
use super::terminal_parsing::update_terminal_snapshot_from_tool_event;
use super::title_generation;
use super::types::{
    ApiErrorCode, ApiMessage, ChatRequest, PendingMessageKind, RemovePendingMessageRequest,
    RequestLogEntry, RequestStats, SessionRuntimeResponse, SseChunkEvent, SseDoneEvent,
    SseErrorEvent, SseHoldEvent, SseSpeedEvent, SseStartEvent, SseTodoUpdateEvent,
    SseToolDeltaEvent, SseToolEndEvent, SseToolStartEvent, TerminalStateSnapshot, ToolEventRecord,
    ToolVariant,
};
use crate::decode_meter::DecodeMeter;

const fn segment_emitted_at(segment_at: Option<jiff::Timestamp>) -> jiff::Timestamp {
    segment_at.expect("a non-empty text segment has a text event time")
}

fn persist_stopped_terminal_calls(
    terminal_snapshot: &mut TerminalStateSnapshot,
    turn_persister: &mut turn_persister::TurnPersister,
) -> Result<(), crate::db::ChatDbError> {
    let running_terminal_ids = terminal_snapshot
        .running_by_call_id
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for call_id in running_terminal_ids {
        let record = ToolEventRecord::Stopped {
            id: call_id.clone(),
            name: "terminal".to_string(),
        };
        let serialized = serde_json::to_string(&record)
            .map_err(|error| crate::db::ChatDbError::Storage(error.to_string()))?;
        // The stop is decided here, not by the agent: there is no emission
        // time to carry, so the stop is stamped when it happens.
        turn_persister.push_tool_event(call_id, serialized, jiff::Timestamp::now());
        update_terminal_snapshot_from_tool_event(terminal_snapshot, &record);
    }
    Ok(())
}

// ── ChatSink trait ──────────────────────────────────────────

/// Abstraction over how chat events are delivered to the caller.
///
/// The stream protocol carries ordering metadata — `seq` (monotonic per
/// turn, shared with pings) and `turn_id` (fresh per retry) — so the
/// frontend can detect gaps and reconcile with history.
///
/// [`HubSink`] publishes them to every client of the session.
trait ChatSink {
    /// The run is prepared; `user_message_id` is the row of the user
    /// message it answers, when it stored one.
    fn on_prepared(&mut self, _user_message_id: Option<i32>) {}
    /// Every agent event of the run, as the agent emitted it.
    fn on_agent_event(
        &mut self,
        _event: Result<&aither_agent::AgentEvent, &aither_agent::AgentError>,
    ) {
    }
    fn on_start(
        &mut self,
        session_id: &str,
        assistant_id: &str,
    ) -> impl Future<Output = bool> + Send;
    fn on_text(&mut self, chunk: &str) -> impl Future<Output = bool> + Send;
    fn on_reasoning(&mut self, chunk: &str) -> impl Future<Output = bool> + Send;
    /// The reply just streamed was not in the communication language:
    /// `text` is its translation, shown instead and marked as translated.
    fn on_reply_translated(
        &mut self,
        _text: &str,
        _language: &str,
    ) -> impl Future<Output = bool> + Send {
        async { true }
    }
    /// A reasoning phase closed after `seconds`, whether or not it produced
    /// readable text. Emitted just before the first visible output that
    /// follows the phase.
    fn on_reasoning_end(&mut self, _seconds: f64) -> impl Future<Output = bool> + Send {
        async { true }
    }
    fn on_tool_delta(
        &mut self,
        id: &str,
        name: &str,
        arguments_fragment: &str,
        summary: crate::daemon::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send;
    fn on_tool_start(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        summary: crate::daemon::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send;
    fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &ToolResult,
        summary: crate::daemon::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send;
    fn on_todo_update(
        &mut self,
        checklist: &crate::todo_parser::TodoChecklist,
    ) -> impl Future<Output = bool> + Send;
    fn on_speed(&mut self, tokens_per_second: f64) -> impl Future<Output = bool> + Send;
    /// The run waits for `reason` before it continues.
    fn on_hold(
        &mut self,
        _reason: crate::client::HoldReason,
        _message: &str,
    ) -> impl Future<Output = bool> + Send {
        async { true }
    }
    /// The text streamed since the attempt began is discarded: the attempt
    /// failed and the prompt is resubmitted.
    fn on_retract(&mut self) -> impl Future<Output = bool> + Send {
        async { true }
    }
    fn on_error(
        &mut self,
        code: ApiErrorCode,
        message: &str,
        retryable: bool,
    ) -> impl Future<Output = bool> + Send;
}

/// Publishes a run's events to every client of its session through the
/// [`hub::TurnHub`]. A run never stops because nobody watches it, so every
/// `on_*` call reports the sink as alive.
struct HubSink {
    hub: Arc<hub::TurnHub>,
    session_id: String,
    /// The user message of the run, shown by clients that did not send it.
    content: Option<String>,
    seq: u64,
    turn_id: String,
    /// The `session_agent` start frame of the run went out.
    run_announced: bool,
}

impl HubSink {
    fn new(state: &DaemonState, req: &ChatRequest) -> Self {
        state.turn_hub.begin_turn(&req.session_id);
        Self {
            hub: state.turn_hub.clone(),
            session_id: req.session_id.clone(),
            content: Some(req.content.clone()).filter(|content| !content.trim().is_empty()),
            seq: 1,
            turn_id: String::new(),
            run_announced: false,
        }
    }

    const fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    /// Publishes a web UI chat event: `event` is its name, `data` the
    /// payload the per-request stream used to carry.
    fn publish_turn<T: Serialize>(&self, event: &str, data: &T) {
        let frame = SessionTurnFrame {
            session_id: &self.session_id,
            event,
            data,
        };
        match serde_json::to_string(&frame) {
            Ok(json) => self
                .hub
                .publish(&self.session_id, "session_turn", json.into()),
            Err(error) => tracing::error!(%error, event, "failed to encode a turn frame"),
        }
    }

    fn publish_agent(&self, body: crate::client::SessionAgentBody) {
        let frame = crate::client::SessionAgentFrame {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            at: jiff::Timestamp::now(),
            body,
        };
        match serde_json::to_string(&frame) {
            Ok(json) => self
                .hub
                .publish(&self.session_id, "session_agent", json.into()),
            Err(error) => tracing::error!(%error, "failed to encode an agent frame"),
        }
    }

    /// Ends the turn: final frames go out, then late subscribers read the
    /// turn from history.
    fn finish(&mut self, event: Option<(&str, &SseDoneEvent)>, outcome: crate::client::RunEnd) {
        if let Some((event, done)) = event {
            self.publish_turn(event, done);
        }
        let seq = self.next_seq();
        self.publish_turn(
            "end",
            &TurnEndEvent {
                seq,
                turn_id: &self.turn_id,
                outcome,
            },
        );
        self.publish_agent(crate::client::SessionAgentBody::End { outcome });
        self.hub.end_turn(&self.session_id);
    }
}

/// `session_turn` frame carrying the display translation of the reply the
/// turn just streamed; the streamed text stays its original.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SseTranslatedEvent<'a> {
    seq: u64,
    turn_id: &'a str,
    text: &'a str,
    language: &'a str,
    language_name: &'a str,
}

/// `session_turn` frame discarding the text the turn streamed so far.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SseRetractEvent<'a> {
    seq: u64,
    turn_id: &'a str,
}

/// `session_turn` frame closing a reasoning phase: `seconds` is its length
/// whether or not the provider returned readable reasoning text.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningEndEvent<'a> {
    seq: u64,
    turn_id: &'a str,
    seconds: f64,
}

/// The last `session_turn` event of every turn, however it ended.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnEndEvent<'a> {
    seq: u64,
    turn_id: &'a str,
    outcome: crate::client::RunEnd,
}

/// A `session_turn` frame on `/api/events`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionTurnFrame<'a, T> {
    session_id: &'a str,
    event: &'a str,
    data: &'a T,
}

// The hub publishes synchronously; the trait's methods are async for sinks
// that write to a socket.
#[allow(clippy::unused_async_trait_impl)]
impl ChatSink for HubSink {
    async fn on_start(&mut self, session_id: &str, assistant_id: &str) -> bool {
        self.turn_id.clear();
        self.turn_id.push_str(assistant_id);
        let seq = self.next_seq();
        self.publish_turn(
            "start",
            &SseStartEvent {
                seq,
                turn_id: &self.turn_id,
                session_id,
                assistant_id,
            },
        );
        true
    }

    // The TUI's start frame waits for the stored message, so a client that
    // reloaded its history can tell the turn's message is already there.
    fn on_prepared(&mut self, user_message_id: Option<i32>) {
        // A retry prepares again, but the run is the one clients already
        // follow.
        if std::mem::replace(&mut self.run_announced, true) {
            return;
        }
        self.publish_agent(crate::client::SessionAgentBody::Start {
            content: self.content.clone(),
            message_id: user_message_id,
        });
    }

    fn on_agent_event(
        &mut self,
        event: Result<&aither_agent::AgentEvent, &aither_agent::AgentError>,
    ) {
        if let Some(event) = crate::client::WireAgentEvent::from_agent(event) {
            self.publish_agent(crate::client::SessionAgentBody::Agent { event });
        }
    }

    async fn on_text(&mut self, chunk: &str) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "text",
            &SseChunkEvent {
                seq,
                turn_id: &self.turn_id,
                chunk,
            },
        );
        true
    }

    async fn on_reply_translated(&mut self, text: &str, language: &str) -> bool {
        let seq = self.next_seq();
        let language_name = crate::session::language_display_name(language);
        self.publish_turn(
            "translated",
            &SseTranslatedEvent {
                seq,
                turn_id: &self.turn_id,
                text,
                language,
                language_name: &language_name,
            },
        );
        true
    }

    async fn on_reasoning(&mut self, chunk: &str) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "reasoning",
            &SseChunkEvent {
                seq,
                turn_id: &self.turn_id,
                chunk,
            },
        );
        true
    }

    async fn on_reasoning_end(&mut self, seconds: f64) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "reasoning_end",
            &ReasoningEndEvent {
                seq,
                turn_id: &self.turn_id,
                seconds,
            },
        );
        self.publish_agent(crate::client::SessionAgentBody::ReasoningEnd { seconds });
        true
    }

    async fn on_tool_delta(
        &mut self,
        id: &str,
        name: &str,
        arguments_fragment: &str,
        summary: crate::daemon::tool_summary::ToolSummary,
    ) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "tool_delta",
            &SseToolDeltaEvent {
                seq,
                turn_id: &self.turn_id,
                id,
                name,
                variant: ToolVariant::infer(name),
                arguments_fragment,
                summary,
            },
        );
        true
    }

    async fn on_tool_start(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        summary: crate::daemon::tool_summary::ToolSummary,
    ) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "tool_start",
            &SseToolStartEvent {
                seq,
                turn_id: &self.turn_id,
                id,
                name,
                variant: ToolVariant::infer(name),
                arguments,
                summary,
            },
        );
        true
    }

    async fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &ToolResult,
        summary: crate::daemon::tool_summary::ToolSummary,
    ) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "tool_end",
            &SseToolEndEvent {
                seq,
                turn_id: &self.turn_id,
                id,
                name,
                variant: ToolVariant::infer(name),
                arguments,
                result,
                summary,
            },
        );
        true
    }

    async fn on_todo_update(&mut self, checklist: &crate::todo_parser::TodoChecklist) -> bool {
        let seq = self.next_seq();
        let event = SseTodoUpdateEvent::from_checklist(checklist, seq, self.turn_id.clone());
        self.publish_turn("todo_update", &event);
        true
    }

    async fn on_speed(&mut self, tokens_per_second: f64) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "speed",
            &SseSpeedEvent {
                seq,
                turn_id: &self.turn_id,
                tokens_per_second,
            },
        );
        true
    }

    async fn on_hold(&mut self, reason: crate::client::HoldReason, message: &str) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "hold",
            &SseHoldEvent {
                seq,
                turn_id: &self.turn_id,
                reason,
                message,
            },
        );
        self.publish_agent(crate::client::SessionAgentBody::Hold {
            reason,
            message: message.to_string(),
        });
        true
    }

    async fn on_retract(&mut self) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "retract",
            &SseRetractEvent {
                seq,
                turn_id: &self.turn_id,
            },
        );
        self.publish_agent(crate::client::SessionAgentBody::Retract);
        true
    }

    async fn on_error(&mut self, code: ApiErrorCode, message: &str, retryable: bool) -> bool {
        let seq = self.next_seq();
        self.publish_turn(
            "error",
            &SseErrorEvent {
                seq,
                turn_id: &self.turn_id,
                code,
                message,
                retryable,
            },
        );
        true
    }
}

/// Interval between `speed` events so fast providers don't flood the stream.
const SPEED_EMIT_INTERVAL: Duration = Duration::from_millis(300);

/// Emit a `speed` event when the interval has elapsed and the meter has a
/// stable reading. Returns the sink liveness flag, like the `on_*` calls.
async fn emit_speed(
    sink: &mut impl ChatSink,
    meter: &DecodeMeter,
    last_emit: &mut Option<Instant>,
) -> bool {
    if last_emit.is_some_and(|t| t.elapsed() < SPEED_EMIT_INTERVAL) {
        return true;
    }
    let Some(rate) = meter.rate() else {
        return true;
    };
    *last_emit = Some(Instant::now());
    sink.on_speed(rate).await
}

/// Buffer added to a provider-announced quota reset so the retry does not
/// race the limit's boundary.
const QUOTA_RESET_GRACE: Duration = Duration::from_secs(2);

/// Race the quota-reset wait against run cancellation.
///
/// `Ok` means the reset arrived and the run may retry; `Err(reason)` is
/// why the run was cancelled while parked.
async fn wait_quota_reset(
    guard: &SessionRunGuard,
    until: jiff::Timestamp,
) -> Result<(), &'static str> {
    let wait = until.duration_since(jiff::Timestamp::now()).unsigned_abs() + QUOTA_RESET_GRACE;
    wait_or_cancel(guard, wait).await
}

/// Race a wait of `wait` against run cancellation: `Err(reason)` is why the
/// run was cancelled while it waited.
async fn wait_or_cancel(guard: &SessionRunGuard, wait: Duration) -> Result<(), &'static str> {
    tokio::select! {
        biased;
        cancel = guard.cancellation().recv() => Err(cancellation_reason_of(&cancel)),
        () = tokio::time::sleep(wait) => Ok(()),
    }
}

const fn cancellation_reason_of<E>(cancel: &Result<CancelReason, E>) -> &'static str {
    match cancel {
        Ok(CancelReason::User) => "run cancelled by user",
        Ok(CancelReason::DaemonRestarting) => "run interrupted: the daemon is restarting",
        Err(_) => "run cancellation channel closed unexpectedly",
    }
}

/// Race the wait for the network to return against run cancellation:
/// `Err(reason)` is why the run was cancelled while it waited.
async fn wait_online_or_cancel(
    guard: &SessionRunGuard,
    connectivity: &aither_connectivity::Connectivity,
) -> Result<(), &'static str> {
    tokio::select! {
        biased;
        cancel = guard.cancellation().recv() => Err(cancellation_reason_of(&cancel)),
        () = connectivity.online() => Ok(()),
    }
}

/// How a retried attempt picks the turn back up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetryKind {
    /// The user message resubmits; the attempt produced nothing yet.
    Resubmit,
    /// The committed transcript continues; the attempt already ran tools.
    Resume,
}

/// What a retried attempt waits out before resubmitting.
enum RetryWait {
    /// The short backoff before the next provider candidate.
    Failover,
    /// The backoff before resubmitting to the same provider candidate;
    /// `retry` counts the retries on it, this one included.
    SameProvider { wait: Duration, retry: u32 },
    /// The upstream-announced quota reset instant.
    QuotaReset(jiff::Timestamp),
    /// The network's return, on the same provider candidate.
    Network,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PendingToolStream {
    name: String,
    buffered_arguments: String,
    emitted_start_to_sink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SinkToolEvent {
    Start {
        id: String,
        name: String,
        arguments: String,
    },
    Delta {
        id: String,
        name: String,
        arguments_fragment: String,
    },
}

/// Project the cumulative tool argument buffer into a structured summary.
///
/// We prefer the live `pending_tool_streams` buffer because it carries the
/// authoritative cumulative state for delta-streamed tools. When that's
/// missing (e.g. a `ToolCallStart`-only path that never went through
/// deltas, or after the pending entry is reaped at tool end), `fallback`
/// — typically the captured arguments string — fills in.
fn project_tool_summary_for_sink(
    pending_streams: &std::collections::HashMap<String, PendingToolStream>,
    id: &str,
    name: &str,
    fallback: Option<&str>,
) -> crate::daemon::tool_summary::ToolSummary {
    let raw = pending_streams
        .get(id)
        .map(|p| p.buffered_arguments.as_str())
        .or(fallback)
        .unwrap_or("");
    crate::daemon::tool_summary::project_tool_summary(raw, ToolVariant::infer(name))
}

fn normalize_tool_delta_for_sink(
    pending_streams: &mut std::collections::HashMap<String, PendingToolStream>,
    id: &str,
    name: &str,
    arguments_fragment: &str,
) -> SinkToolEvent {
    let pending = pending_streams.entry(id.to_string()).or_default();
    if !name.is_empty() {
        pending.name.clear();
        pending.name.push_str(name);
    }
    // `arguments_fragment` is a true increment: append to the call's buffer
    // and forward it verbatim.
    pending.buffered_arguments.push_str(arguments_fragment);

    if pending.emitted_start_to_sink {
        return SinkToolEvent::Delta {
            id: id.to_string(),
            name: pending.name.clone(),
            arguments_fragment: arguments_fragment.to_string(),
        };
    }

    pending.emitted_start_to_sink = true;
    SinkToolEvent::Start {
        id: id.to_string(),
        name: pending.name.clone(),
        arguments: pending.buffered_arguments.clone(),
    }
}

fn normalize_tool_start_for_sink(
    pending_streams: &mut std::collections::HashMap<String, PendingToolStream>,
    id: &str,
    name: &str,
    arguments: &str,
) -> Option<SinkToolEvent> {
    let pending = pending_streams.entry(id.to_string()).or_default();
    let name_changed = !name.is_empty() && pending.name != name;
    if !name.is_empty() {
        pending.name.clear();
        pending.name.push_str(name);
    }

    if pending.emitted_start_to_sink {
        let arguments_fragment = merge_tool_arguments(&mut pending.buffered_arguments, arguments);
        if arguments_fragment.is_empty() && !name_changed {
            return None;
        }
        return Some(SinkToolEvent::Delta {
            id: id.to_string(),
            name: pending.name.clone(),
            arguments_fragment,
        });
    }

    pending.buffered_arguments.clear();
    pending.buffered_arguments.push_str(arguments);
    pending.emitted_start_to_sink = true;
    Some(SinkToolEvent::Start {
        id: id.to_string(),
        name: pending.name.clone(),
        arguments: arguments.to_string(),
    })
}

/// Diff a `ToolCallStart` arguments snapshot against what deltas already
/// buffered; returns the suffix that hasn't streamed yet.
fn merge_tool_arguments(buffered_arguments: &mut String, incoming: &str) -> String {
    if incoming.is_empty() || incoming == buffered_arguments {
        return String::new();
    }

    if incoming.starts_with(buffered_arguments.as_str()) {
        let suffix = incoming[buffered_arguments.len()..].to_string();
        buffered_arguments.push_str(suffix.as_str());
        return suffix;
    }

    buffered_arguments.push_str(incoming);
    incoming.to_string()
}

fn log_first_agent_event(
    seen: &mut bool,
    started_at: &Instant,
    session_id: &str,
    assistant_id: &str,
    event_kind: &'static str,
) {
    if *seen {
        return;
    }
    *seen = true;
    tracing::info!(
        session_id,
        assistant_id,
        event_kind,
        elapsed_ms = elapsed_ms_saturating(started_at),
        "chat stream first agent event"
    );
}

fn log_first_visible_tool_event(
    seen: &mut bool,
    started_at: &Instant,
    session_id: &str,
    assistant_id: &str,
    tool_id: &str,
    tool_name: &str,
    source: &'static str,
) {
    if *seen {
        return;
    }
    *seen = true;
    tracing::info!(
        session_id,
        assistant_id,
        tool_id,
        tool_name,
        source,
        elapsed_ms = elapsed_ms_saturating(started_at),
        "chat stream first visible tool"
    );
}

fn log_first_text_event(
    seen: &mut bool,
    started_at: &Instant,
    session_id: &str,
    assistant_id: &str,
) {
    if *seen {
        return;
    }
    *seen = true;
    tracing::info!(
        session_id,
        assistant_id,
        elapsed_ms = elapsed_ms_saturating(started_at),
        "chat stream first text"
    );
}

/// Successful result from the shared agent loop.
#[derive(Debug)]
struct AgentRunResult {
    /// The stored row of the user message the turn answers, when it stored one.
    user_message_id: Option<i32>,
    /// The assistant rows the turn stored, oldest first.
    assistant_message_ids: Vec<i32>,
    full_text: String,
    reasoning_text: String,
    terminal_snapshot: TerminalStateSnapshot,
    usage: Usage,
    provider_id: String,
    model_context_window: Option<u32>,
    elapsed_ms: u64,
}

/// The assistant rows a turn stored after `boundary`, the largest row id
/// before it persisted anything.
async fn stored_assistant_message_ids(
    state: &Arc<DaemonState>,
    session_id: &str,
    boundary: i32,
) -> Result<Vec<i32>, crate::db::ChatDbError> {
    Ok(state
        .db()
        .get_messages_by_role(session_id, MessageRole::Assistant.as_str())
        .await?
        .into_iter()
        .map(|row| row.id)
        .filter(|id| *id > boundary)
        .collect())
}

/// The turn's `done` event: the reply carries the id of the first assistant
/// row the turn stored, the event the ids of all of them and of the user row,
/// so a client names its streamed messages as the session's REST messages.
fn done_event_from_run_result(
    seq: u64,
    turn_id: String,
    session_id: String,
    result: AgentRunResult,
) -> SseDoneEvent {
    let user_message_id = result.user_message_id.map(api_message_id);
    let message_ids = result
        .assistant_message_ids
        .iter()
        .copied()
        .map(api_message_id)
        .collect();
    SseDoneEvent {
        seq,
        turn_id,
        session_id,
        user_message_id,
        message_ids,
        assistant: api_message_from_run_result(result),
    }
}

fn api_message_from_run_result(result: AgentRunResult) -> ApiMessage {
    let first = result
        .assistant_message_ids
        .first()
        .copied()
        .expect("a finished turn stores an assistant row");
    ApiMessage {
        id: api_message_id(first),
        role: "assistant".to_string(),
        content: result.full_text,
        timestamp: jiff::Timestamp::now().to_string(),
        attachments: Vec::new(),
        thinking: if result.reasoning_text.is_empty() {
            None
        } else {
            Some(result.reasoning_text)
        },
        is_streaming: Some(false),
        is_thinking: Some(false),
        tool_event: None,
        terminal_executions: result.terminal_snapshot.all_executions(),
        stats: Some(RequestStats {
            provider_id: Some(result.provider_id),
            prompt_tokens: result.usage.prompt_tokens,
            completion_tokens: result.usage.completion_tokens,
            total_tokens: result.usage.total_tokens,
            cache_read_tokens: result.usage.cache_read_tokens,
            cache_write_tokens: result.usage.cache_write_tokens,
            context_window: result.model_context_window,
            elapsed_ms: Some(result.elapsed_ms),
        }),
        translation: None,
    }
}

/// The response that refuses a client turn: a conflict when another run holds
/// the session, an internal error when acquiring the run failed.
fn acquire_refusal(error: AcquireError) -> skyzen::Error {
    match error {
        AcquireError::Busy(_) => active_session_conflict_error(),
        AcquireError::Storage(error) => internal_error(error),
    }
}

fn active_session_conflict_error() -> skyzen::Error {
    skyzen::Error::msg("session is already active; send this message via steer or queue instead")
        .set_status(skyzen::StatusCode::CONFLICT)
}

fn draining_error() -> skyzen::Error {
    skyzen::Error::msg("the daemon is shutting down; the next daemon takes over")
        .set_status(skyzen::StatusCode::CONFLICT)
}

fn persisted_attachments(
    attachments: Vec<super::types::ApiAttachment>,
) -> Vec<PersistedAttachment> {
    attachments
        .into_iter()
        .map(|attachment| PersistedAttachment {
            id: attachment.id,
            name: attachment.name,
            attachment_type: attachment.attachment_type,
            url: attachment.url,
            upload_data_url: attachment.upload_data_url,
            text_content: attachment.text_content,
        })
        .collect()
}

fn pending_user_message_from_request(req: ChatRequest) -> PendingUserMessage {
    PendingUserMessage {
        id: None,
        state: PendingMessageState::default(),
        error: None,
        content: req.content,
        attachments: persisted_attachments(req.attachments),
        hidden_context_xml: req.hidden_context_xml,
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: req.delivery_mode,
    }
}

fn request_has_visible_user_input(req: &ChatRequest) -> bool {
    !req.content.trim().is_empty() || !req.attachments.is_empty()
}

pub fn api_attachments_from_persisted(
    attachments: Vec<PersistedAttachment>,
) -> Vec<super::types::ApiAttachment> {
    attachments
        .into_iter()
        .map(|attachment| super::types::ApiAttachment {
            id: attachment.id,
            name: attachment.name,
            attachment_type: attachment.attachment_type,
            url: attachment.url,
            upload_data_url: attachment.upload_data_url,
            text_content: attachment.text_content,
        })
        .collect()
}

/// Record that the run is executing again after a wait, as the lease holder.
async fn mark_run_running(
    state: &Arc<DaemonState>,
    fence: &RunFence,
) -> Result<(), crate::db::ChatDbError> {
    state
        .runtime_service
        .set_state_as_run(fence, SessionRuntimeState::Running)
        .await
}

async fn set_terminal_runtime_state(
    state: &Arc<DaemonState>,
    session_id: &str,
    terminal: TerminalRuntimeState,
) -> Result<(), crate::db::ChatDbError> {
    state
        .runtime_service
        .set_terminal_runtime(session_id, terminal)
        .await
        .map(|_| ())
}

async fn clear_terminal_runtime_state(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<(), crate::db::ChatDbError> {
    state
        .runtime_service
        .clear_terminal_runtime(session_id)
        .await
        .map(|_| ())
}

/// Dispatches the prompts task runs stored for their sessions, one wake per
/// stored prompt, the way a message from another agent is dispatched.
pub(super) fn spawn_task_wake_dispatcher(
    state: Arc<DaemonState>,
    wakes: async_channel::Receiver<String>,
) {
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Ok(session_id) = wakes.recv().await {
            spawn_follow_up_runs(state.clone(), session_id);
        }
    });
}

pub(super) fn spawn_follow_up_runs(state: Arc<DaemonState>, session_id: String) {
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        let guard = match state.runtime_service.runs().acquire(&session_id).await {
            Ok(guard) => guard,
            // The run holding the session looks for pending work again when
            // it ends, after releasing, so it sees what was enqueued here.
            Err(AcquireError::Busy(_)) => return,
            Err(AcquireError::Storage(error)) => {
                tracing::error!(session_id = %session_id, error = %error, "failed to acquire session run for follow-up dispatch");
                return;
            }
        };
        dispatch_follow_ups(state, guard).await;
    });
}

/// Continue a run that [`SessionRunGuard::finish`] re-acquired for pending work.
fn spawn_continued_run(state: Arc<DaemonState>, guard: SessionRunGuard) {
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(dispatch_follow_ups(state, guard));
}

/// Run the session's pending messages one after another under `guard`,
/// until the queue is empty and finishing the guard finds no new work.
async fn dispatch_follow_ups(state: Arc<DaemonState>, mut guard: SessionRunGuard) {
    let session_id = guard.session_id().to_string();
    // Whether the newest turn ran to completion, so the session is idle
    // once the queue drains.
    let mut last_turn_completed = false;
    loop {
        let next = match state
            .runtime_service
            .take_next_for_run(guard.fence(), &session_id)
            .await
        {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!(session_id = %session_id, error = %error, "failed to load next pending message");
                finish_failed_run(guard).await;
                return;
            }
        };
        let Some(next) = next else {
            match guard.finish(RunOutcome::Completed).await {
                Ok(Some(continued)) => {
                    guard = continued;
                    continue;
                }
                Ok(None) => {
                    if last_turn_completed {
                        session_became_idle(&state, &session_id);
                    }
                    return;
                }
                Err(error) => {
                    tracing::error!(session_id = %session_id, error = %error, "failed to release session run after follow-up dispatch");
                    return;
                }
            }
        };

        let request = ChatRequest {
            session_id: session_id.clone(),
            content: next.content,
            attachments: api_attachments_from_persisted(next.attachments),
            hidden_context_xml: next.hidden_context_xml,
            delivery_mode: next.delivery_mode,
            pending_entry_id: next.id,
        };
        match Box::pin(run_chat_collect_with_receiver(
            state.clone(),
            request,
            true,
            true,
            &guard,
        ))
        .await
        {
            Ok(end) => last_turn_completed = end == TurnEnd::Completed,
            Err(error) => {
                tracing::warn!(session_id = %session_id, error = %error, "follow-up run failed");
                finish_failed_run(guard).await;
                return;
            }
        }
    }
}

async fn finish_failed_run(guard: SessionRunGuard) {
    let session_id = guard.session_id().to_string();
    if let Err(error) = guard.finish_idle(RunOutcome::Failed).await {
        tracing::error!(session_id = %session_id, error = %error, "failed to release failed session run");
    }
}

pub(super) async fn resume_pending_follow_up_runs(
    state: Arc<DaemonState>,
) -> Result<(), crate::db::ChatDbError> {
    let session_ids = state.db().sessions_requiring_pending_dispatch().await?;
    for session_id in session_ids {
        spawn_follow_up_runs(state.clone(), session_id);
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum ResumeRequestKind {
    Permission,
    Workspace,
    /// An internal authorization prompt, such as an SSH connection.
    Authorization,
}

#[derive(Clone, Copy)]
pub(super) enum ResumeResolution {
    Approved,
    Denied,
}

#[derive(Template)]
#[template(path = "reminder_browser_takeover.txt", escape = "none")]
struct BrowserTakeoverReminder;

#[derive(Template)]
#[template(path = "reminder_resume_waiting_turn.txt", escape = "none")]
struct ResumeWaitingTurnReminder {
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
}

fn browser_takeover_alert() -> String {
    BrowserTakeoverReminder
        .render()
        .expect("browser takeover reminder must render")
}

fn resume_waiting_turn_reminder(
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
) -> Result<crate::XmlBlock, ChatMarkupError> {
    let text = ResumeWaitingTurnReminder {
        request_kind,
        resolution,
    }
    .render()
    .expect("resume waiting turn reminder must render");
    crate::XmlBlock::parse(text).map_err(ChatMarkupError::Parse)
}

/// How a resume after a decision was scheduled.
pub(super) enum WaitingTurnResume {
    /// The run is gone: this process took the session's lease and queued the
    /// resume reminder, and the guard dispatches it.
    Dispatch {
        guard: SessionRunGuard,
        runtime: crate::session::SessionRuntimeSummary,
    },
    /// A live run (possibly in another process) holds the lease. It was or
    /// will be answered through its own prompt, so nothing was written: no
    /// state change and no reminder that would reach the model after it.
    RunLive(crate::session::SessionRuntimeSummary),
}

/// Take the lease of a session whose run is gone and queue the reminder for
/// the decision it was waiting for. The lease is the gate: while a live run
/// holds it, nothing about the session is written.
pub(super) async fn queue_waiting_turn_resume(
    runtime_service: &crate::session::SessionRuntimeService,
    session_id: &str,
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
) -> Result<WaitingTurnResume, ScheduleWaitingTurnError> {
    let pending = PendingUserMessage {
        id: None,
        state: PendingMessageState::default(),
        error: None,
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: Some(resume_waiting_turn_reminder(request_kind, resolution)?),
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: crate::session::PendingUserMessageMode::Steer,
    };
    let guard = match runtime_service.runs().acquire(session_id).await {
        Ok(guard) => guard,
        Err(AcquireError::Busy(_)) => {
            return runtime_service
                .get(session_id)
                .await
                .map(WaitingTurnResume::RunLive)
                .map_err(ScheduleWaitingTurnError::Lease);
        }
        Err(AcquireError::Storage(error)) => return Err(ScheduleWaitingTurnError::Lease(error)),
    };
    match runtime_service
        .enqueue(
            session_id,
            pending,
            crate::session::PendingUserMessageMode::Steer,
        )
        .await
    {
        Ok(runtime) => Ok(WaitingTurnResume::Dispatch { guard, runtime }),
        Err(error) => {
            finish_failed_run(guard).await;
            Err(ScheduleWaitingTurnError::Enqueue(error))
        }
    }
}

pub(super) async fn schedule_waiting_turn_resume(
    state: &Arc<DaemonState>,
    session_id: &str,
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
) -> Result<crate::session::SessionRuntimeSummary, ScheduleWaitingTurnError> {
    match queue_waiting_turn_resume(&state.runtime_service, session_id, request_kind, resolution)
        .await?
    {
        WaitingTurnResume::Dispatch { guard, runtime } => {
            spawn_continued_run(state.clone(), guard);
            Ok(runtime)
        }
        WaitingTurnResume::RunLive(runtime) => Ok(runtime),
    }
}

async fn schedule_browser_takeover_resume(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<crate::session::SessionRuntimeSummary, crate::db::ChatDbError> {
    let pending = PendingUserMessage {
        id: None,
        state: PendingMessageState::default(),
        error: None,
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: None,
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: crate::session::PendingUserMessageMode::Steer,
    };
    state
        .runtime_service
        .enqueue(
            session_id,
            pending,
            crate::session::PendingUserMessageMode::Steer,
        )
        .await
}

/// Persist a finished tool call's span for the `/usage` wall-clock split.
async fn record_tool_interval(
    state: &Arc<DaemonState>,
    session_id: &str,
    started: jiff::Timestamp,
    ended: jiff::Timestamp,
) {
    let Some(interval) =
        crate::agent::Interval::new(started.as_millisecond(), ended.as_millisecond())
    else {
        tracing::warn!(session_id, "tool call ended before it started");
        return;
    };
    if let Err(error) = state.db().append_tool_interval(session_id, interval).await {
        tracing::warn!(error = %error, "failed to persist tool interval");
    }
}

async fn record_request_log(state: &Arc<DaemonState>, entry: RequestLogEntry) {
    let db = state.db();
    if let Err(error) = db
        .append_request_log(crate::db::RequestLogParams {
            session_id: entry.session_id.as_str(),
            timestamp: entry.timestamp.as_str(),
            model: entry.model.as_str(),
            provider_id: entry.provider_id.as_str(),
            prompt_tokens: entry.prompt_tokens,
            completion_tokens: entry.completion_tokens,
            total_tokens: entry.total_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            cache_write_tokens: entry.cache_write_tokens,
            context_window: entry.context_window,
            elapsed_ms: entry.elapsed_ms,
            ok: entry.ok,
            error: entry.error.as_deref(),
        })
        .await
    {
        tracing::warn!(error = %error, "failed to persist request log");
    }
    state.notify_ui_events(UiEventTopics::REQUEST_LOGS);
}

// NOTE: the public `POST /api/chat` non-streaming handler has been removed.
// All client-facing chat interaction now flows through `POST /api/chat/stream`
// so there is a single protocol path. `run_chat_collect_with_receiver` below is
// kept for internal follow-up queue dispatch where no SSE client is attached.

pub(super) async fn enqueue_chat(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    wake_on_user_message(&state).await?;
    enqueue_pending(&state, req).await.map(Json)
}

/// Hands `req` to the session as a pending message: the running turn takes
/// it as `req.delivery_mode` says, and an idle session starts a turn for it.
async fn enqueue_pending(
    state: &Arc<DaemonState>,
    req: ChatRequest,
) -> SkyResult<SessionRuntimeResponse> {
    let pending = pending_user_message_from_request(req.clone());
    let runtime = state
        .runtime_service
        .enqueue(&req.session_id, pending, req.delivery_mode)
        .await
        .map_err(internal_error)?;
    if runtime.can_dispatch_follow_up() {
        spawn_follow_up_runs(state.clone(), req.session_id.clone());
    }
    Ok(runtime.into())
}

/// Delivers a message from the user the way a client does: a turn for an
/// idle session, a pending message for a busy one.
pub(super) async fn deliver_user_message(
    state: &Arc<DaemonState>,
    req: ChatRequest,
) -> SkyResult<SessionRuntimeResponse> {
    let runtime = state
        .runtime_service
        .get(&req.session_id)
        .await
        .map_err(internal_error)?;
    if runtime.admits_new_request() {
        start_prompt_turn(state, req).await
    } else {
        enqueue_pending(state, req).await
    }
}

pub(super) async fn remove_pending_message(
    params: skyzen::routing::Params,
    Json(req): Json<RemovePendingMessageRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let session_id = params
        .get("id")
        .map_err(super::error::bad_request_error)?
        .to_string();
    let runtime = match req.kind {
        PendingMessageKind::PendingImmediate => state
            .runtime_service
            .remove_pending_immediate(&session_id)
            .await
            .map_err(internal_error)?,
        PendingMessageKind::Queued => state
            .runtime_service
            .remove_queued_at(
                &session_id,
                req.index.ok_or_else(|| {
                    super::error::bad_request_error("index is required for queued removal")
                })?,
            )
            .await
            .map_err(internal_error)?,
    };
    Ok(Json(runtime.into()))
}

pub(super) async fn stop_session_run(
    params: skyzen::routing::Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let session_id = params
        .get("id")
        .map_err(super::error::bad_request_error)?
        .to_string();
    let db = state.db();
    if !db
        .session_exists(&session_id)
        .await
        .map_err(internal_error)?
    {
        return Err(
            skyzen::Error::msg(format!("Session {session_id} not found"))
                .set_status(skyzen::StatusCode::NOT_FOUND),
        );
    }

    let cancelled = state
        .runtime_service
        .runs()
        .cancel(&session_id, CancelReason::User);
    tracing::info!(session_id = %session_id, cancelled, "received session stop request");

    let runtime = state
        .runtime_service
        .settle_stopped(&session_id)
        .await
        .map_err(internal_error)?;
    Ok(Json(runtime.into()))
}

/// `POST /api/sessions/{id}/messages`: starts a turn and returns at once.
///
/// The turn runs in the daemon and streams to every client of the session
/// over `/api/events` (`session_turn` / `session_agent`); a client that
/// disconnects does not stop it.
pub(super) async fn send_message(
    params: skyzen::routing::Params,
    Json(mut req): Json<ChatRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let session_id = params
        .get("id")
        .map_err(super::error::bad_request_error)?
        .to_string();
    req.session_id.clone_from(&session_id);
    wake_on_user_message(&state).await?;
    start_prompt_turn(&state, req).await.map(Json)
}

/// A message from the user ends the sleep state when it began an hour or
/// more ago; within the hour the user stays asleep.
async fn wake_on_user_message(state: &DaemonState) -> SkyResult<()> {
    state
        .sleep
        .user_message(jiff::Timestamp::now())
        .await
        .map(drop)
        .map_err(internal_error)
}

/// Starts the turn `req` asks for in its session and returns at once, with
/// the session's runtime as it stands then.
pub(super) async fn start_prompt_turn(
    state: &Arc<DaemonState>,
    req: ChatRequest,
) -> SkyResult<SessionRuntimeResponse> {
    let session_id = req.session_id.clone();
    if state.draining.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(draining_error());
    }
    let runtime = state
        .runtime_service
        .get(&session_id)
        .await
        .map_err(internal_error)?;
    if !runtime.admits_new_request() {
        return Err(active_session_conflict_error());
    }
    let run_guard = state
        .runtime_service
        .runs()
        .acquire(&session_id)
        .await
        .map_err(acquire_refusal)?;
    let tokio_handle = state.tokio_handle.clone();
    let run_state = state.clone();
    tokio_handle.spawn(async move {
        let session_id = req.session_id.clone();
        if let Err(error) = Box::pin(run_published_turn(
            run_state,
            req,
            run_guard,
            TurnKind::Prompt,
        ))
        .await
        {
            tracing::error!(session_id = %session_id, error = %error, "failed to settle a turn");
        }
    });
    let runtime = state
        .runtime_service
        .get(&session_id)
        .await
        .map_err(internal_error)?;
    Ok(runtime.into())
}

/// Tells `agent` of a change of the user's sleep state it has not heard of:
/// one alert per change, at the start of the session's next turn.
async fn tell_sleep_state(state: &DaemonState, session_id: &str, agent: &mut MayAgent) {
    match state.sleep.take_notice(session_id).await {
        Ok(Some(notice)) => agent.tell_sleep_notice(notice),
        Ok(None) => {}
        Err(error) => {
            tracing::error!(%session_id, %error, "failed to record the sleep state an agent was told");
        }
    }
}

/// `POST /api/sessions/{id}/bash`: run a `!` command for the user.
///
/// Runs on the user's behalf in the session's cwd — keychain secrets
/// injected, output masked — with the same runner the TUI's bash mode
/// uses. The caller turns the masked outcome into the agent-facing
/// transcript.
pub(super) async fn run_bash(
    params: skyzen::routing::Params,
    Json(req): Json<crate::daemon::types::BashRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<crate::daemon::types::BashResponse>> {
    let session_id = params
        .get("id")
        .map_err(super::error::bad_request_error)?
        .to_string();
    let command = req.command.trim().to_string();
    if command.is_empty() {
        return Err(super::error::bad_request_error("empty bash command"));
    }
    let session = state
        .db()
        .get_session(&session_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| super::error::bad_request_error("session not found"))?;
    let cwd = crate::session::SessionDirectory::of(&session, &state.may_dir).into_path();
    let secrets = crate::secrets::Secrets::load(
        &crate::secrets::SecretStore::new(&state.may_dir),
        state.config.load().as_ref(),
    );
    let run_secrets = secrets.clone();
    let outcome =
        blocking::unblock(move || crate::bash::run_bash_command(&command, &cwd, &run_secrets))
            .await;
    // The transcript returned to the client never carries the raw secret.
    let (masked_command, _) = secrets.mask(&outcome.command);
    Ok(Json(crate::daemon::types::BashResponse {
        command: masked_command,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        exit_code: outcome.exit_code,
        had_secret: outcome.had_secret,
        error: outcome.error,
    }))
}

pub(super) fn spawn_shared_tool_dispatchers(state: Arc<DaemonState>, queues: UiToolQueues) {
    let UiToolQueues {
        task,
        notify,
        browser,
        register_skills,
        tunnel,
    } = queues;

    let task_state = state.clone();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = task.next().await {
            let result = Box::pin(handle_task_request(&task_state, request.args.clone())).await;
            let _ = request.respond(result);
        }
    });

    let notification_service = state.notification_service.clone();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = notify.next().await {
            let result =
                handle_notify_request(notification_service.clone(), request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let browser_state = state.clone();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = browser.next().await {
            let result = handle_browser_request(&browser_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let register_skills_state = state.clone();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = register_skills.next().await {
            let result =
                handle_register_skills_request(&register_skills_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let tunnel_state = state;
    let tokio_handle = tunnel_state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = tunnel.next().await {
            let result =
                super::tunnel::handle_tunnel_request(&tunnel_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });
}

/// What one attempt's run starts from.
struct TurnRun<'r> {
    prompt: &'r str,
    hidden_context: Option<&'r str>,
    attachments: Vec<LlmAttachment>,
    /// Set once a retry follows tool activity: the attempt continues the
    /// committed transcript instead of resubmitting the prompt.
    continue_from_transcript: bool,
    attempt: usize,
}

/// Runs one attempt of a turn on `agent`, handing each event to `turn`
/// while the run keeps going; `Err` is why the run stopped before it
/// finished. The run's cancellation stops it between events.
async fn drive_turn<S: ChatSink>(
    turn: &mut turn_events::TurnEvents<'_, '_, S>,
    agent: &mut MayAgent,
    kind: &TurnKind,
    run: TurnRun<'_>,
) -> Result<(), turn_events::TurnStop> {
    let guard = turn.guard;
    let stop = async {
        turn_events::TurnStop::Cancelled(cancellation_reason_of(&guard.cancellation().recv().await))
    };
    let driven = match kind {
        TurnKind::Prompt if !run.continue_from_transcript => {
            agent
                .run_with_hidden_context(run.prompt, run.hidden_context, run.attachments)
                .drive(&mut *turn, stop, turn_events::step)
                .await
        }
        TurnKind::Prompt | TurnKind::Resume(_) => {
            // The wake events reach clients with the first attempt only; a
            // retry would show them twice.
            if let TurnKind::Resume(wake_events) = kind
                && run.attempt == 0
            {
                for timed in wake_events.iter().cloned() {
                    if let ControlFlow::Break(stop) = turn.handle(timed).await {
                        return Err(stop);
                    }
                }
            }
            agent
                .resume()
                .drive(&mut *turn, stop, turn_events::step)
                .await
        }
    };
    match driven {
        Ok(ControlFlow::Continue(_)) => Ok(()),
        Ok(ControlFlow::Break(stop)) => Err(stop),
        Err(error) => {
            // The failure closes an open reasoning phase before clients see
            // it, like any visible output.
            if let ControlFlow::Break(stop) = turn.close_reasoning_phase().await {
                return Err(stop);
            }
            turn.sink.on_agent_event(Err(&error));
            Err(turn_events::TurnStop::Failed(error))
        }
    }
}

/// Shared agent event loop used by both streaming and non-streaming chat.
///
/// Handles: retry logic, agent event processing, history persistence,
/// request logging, and post-completion hooks.
///
/// The loop's future is tens of kilobytes, so it is built on the heap here,
/// in a frame that has returned before it is first polled: a caller that
/// boxed it itself would reserve room for it in its own poll frame, which a
/// debug build keeps on the stack under every provider request.
fn run_agent_loop<'a>(
    state: &'a Arc<DaemonState>,
    req: &'a ChatRequest,
    sink: &'a mut impl ChatSink,
    guard: &'a SessionRunGuard,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    kind: &'a TurnKind,
) -> Pin<Box<impl Future<Output = Result<AgentRunResult, AgentLoopError>> + 'a>> {
    Box::pin(async move {
        // Whatever path started the turn, it supersedes the on-screen
        // suggestion and the idle-recap timer.
        suggest::cancel(state, &req.session_id);
        recap::unarm(state, &req.session_id);
        let mut attempt = 0usize;
        // The provider candidate the next attempt builds on, how many
        // candidates there are, and how many retries the current one had.
        let mut candidate = 0usize;
        let mut candidate_count = 1usize;
        let mut same_provider_retries = 0u32;
        // The current candidate's cache entry, as the agent that last made
        // progress on it left it. A rebuilt agent starts an entry of its own,
        // so the handle outlives the agent that failed.
        let mut candidate_warmth: Option<aither_agent::CacheWarmth> = None;
        let mut turn_hidden_context_xml: Option<String> = None;
        // The stored user message, from the first attempt.
        let mut turn_user_message_id: Option<i32> = None;
        // The largest history row id before this turn persists anything. A
        // retry that resubmits the prompt rebuilds context only up to it,
        // so the rows the turn itself wrote are not fed back alongside the
        // resubmitted prompt.
        let turn_boundary = crate::session::load_history_from_db(&state.db(), &req.session_id)
            .await
            .map_err(UiChatPreparationError::LoadHistory)?
            .messages()
            .iter()
            .filter_map(|message| message.id)
            .max()
            .unwrap_or(0);
        // Set once a retry follows tool activity: later attempts continue
        // the committed transcript with `resume` instead of resubmitting
        // the prompt.
        let mut continue_from_transcript = false;
        // A resumed turn exists only for what the agent it took collected,
        // and it never fails over (it has one provider candidate): a retry
        // waits and continues with that same agent.
        let mut retained_agent: Option<MayAgent> = None;

        loop {
            let assistant_id = format!("assistant-{}", crate::id::new());
            let started_at = Instant::now();

            if !sink.on_start(&req.session_id, &assistant_id).await {
                return Err(AgentLoopError::ClientDisconnectedBeforeStreamStart);
            }
            tracing::info!(
                session_id = %req.session_id,
                assistant_id = %assistant_id,
                attempt,
                elapsed_ms = elapsed_ms_saturating(&started_at),
                "chat stream start sent"
            );

            let PreparedAgent {
                session_id,
                mut agent,
                current_model,
                provider_id,
                profile_id,
                hidden_context_xml,
                prompt,
                attachments,
                provider_candidate_count,
                build_key,
                user_message_id,
                reasoning_effort,
            } = match prepare_agent_for_request(
                state,
                req,
                guard.fence(),
                persist_user_message && attempt == 0 && !kind.is_resume(),
                use_persisted_context_snapshot,
                TurnAttempt {
                    provider_attempt: candidate,
                    // A resubmitting retry rebuilds context only up to the
                    // turn boundary; a resume retry replays the committed
                    // transcript in full.
                    turn_boundary: if continue_from_transcript {
                        None
                    } else {
                        Some(turn_boundary)
                    },
                    retained_agent: retained_agent.take(),
                },
                kind,
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    // Clients waiting for the run's start see it fail.
                    sink.on_prepared(turn_user_message_id);
                    let _ = sink
                        .on_error(error.api_code(), &error.to_string(), false)
                        .await;
                    // Preparation never reached the transcript: the claimed
                    // queue entry goes back to pending for a later run.
                    if let Some(entry_id) = req.pending_entry_id {
                        let _ = state
                            .db()
                            .release_pending_claim(entry_id, guard.fence())
                            .await;
                    }
                    return Err(error.into());
                }
            };
            tracing::info!(
                session_id = %session_id,
                assistant_id = %assistant_id,
                model = %current_model,
                provider_id = %provider_id,
                attempt,
                elapsed_ms = elapsed_ms_saturating(&started_at),
                "chat agent prepared"
            );
            // A resumed turn runs on the agent that collected its events; a
            // provider retry would rebuild it without them.
            if attempt == 0 && !kind.is_resume() {
                candidate_count = provider_candidate_count.max(1);
            }
            if user_message_id.is_some() {
                turn_user_message_id = user_message_id;
            }
            sink.on_prepared(turn_user_message_id);
            if let Some(hidden_context_xml) = hidden_context_xml {
                turn_hidden_context_xml = Some(hidden_context_xml);
            }
            let model_context_window = agent.model_context_window().or_else(|| {
                lookup_model_info(crate::model_selector::selector_model_id(&current_model))
                    .and_then(aither_models::ModelEntry::max_input_tokens)
            });

            let session_paths =
                session_paths_for_session(state, profile_id.as_deref(), &session_id);
            let tasks_accepted = match session_paths.tasks_content().await {
                Ok(content) => content,
                Err(error) => {
                    tracing::warn!(%error, "failed to read session tasks.md");
                    None
                }
            };
            // The reasoning phase is armed only when the request asked the
            // provider for reasoning text.
            let reasoning_display = crate::agent::reasoning_text_requested(
                state
                    .config
                    .load()
                    .provider
                    .get(&provider_id)
                    .map(|instance| instance.resolve_type(&provider_id)),
                crate::model_selector::selector_model_id(&current_model),
                reasoning_effort,
            );
            let mut turn = turn_events::TurnEvents {
                state,
                guard,
                sink: &mut *sink,
                session_id: &session_id,
                assistant_id: assistant_id.as_str(),
                session_paths: &session_paths,
                started_at,
                reasoning_display,
                reasoning_phase: reasoning_display.then_some(started_at),
                provider_id: &provider_id,
                model: &current_model,
                request_prefix: None,
                usage: Usage::default(),
                ledger: request_ledger::RequestLedger::new(
                    session_id.as_str(),
                    &current_model,
                    &provider_id,
                    model_context_window,
                    Instant::now(),
                ),
                decode_meter: DecodeMeter::default(),
                last_speed_emit: None,
                segment_text: String::new(),
                segment_at: None,
                segment_translation: None,
                last_event_at: None,
                full_text: String::new(),
                reasoning_text: String::new(),
                terminal_snapshot: TerminalStateSnapshot::default(),
                saw_tool_activity: false,
                tool_arguments_by_id: std::collections::HashMap::new(),
                tool_started_at: std::collections::HashMap::new(),
                pending_tool_streams: std::collections::HashMap::new(),
                last_todo_hash: None,
                tasks_accepted,
                turn_persister: turn_persister::TurnPersister::new(guard.fence().clone()),
                first_agent_event_seen: false,
                first_visible_tool_seen: false,
                first_text_seen: false,
            };
            agent.sync_runtime_context().await;
            tell_sleep_state(state, req.session_id.as_str(), &mut agent).await;
            let stopped = drive_turn(
                &mut turn,
                &mut agent,
                kind,
                TurnRun {
                    prompt: &prompt,
                    hidden_context: turn_hidden_context_xml.as_deref(),
                    attachments,
                    continue_from_transcript,
                    attempt,
                },
            )
            .await;
            let turn_events::TurnEvents {
                sink: _,
                usage,
                mut ledger,
                mut decode_meter,
                segment_text,
                segment_at,
                last_event_at,
                full_text,
                reasoning_text,
                mut terminal_snapshot,
                saw_tool_activity,
                mut turn_persister,
                mut reasoning_phase,
                ..
            } = turn;
            let mut terminal_error: Option<aither_agent::AgentError> = None;
            let mut cancellation_reason: Option<&'static str> = None;
            let mut browser_takeover_paused = false;
            match stopped {
                Ok(()) => {}
                Err(turn_events::TurnStop::Cancelled(reason)) => {
                    cancellation_reason = Some(reason);
                }
                Err(turn_events::TurnStop::TakeoverPaused) => browser_takeover_paused = true,
                Err(turn_events::TurnStop::Failed(error)) => terminal_error = Some(error),
                Err(turn_events::TurnStop::Persist(error)) => return Err(error.into()),
            }

            // A stream that ended mid-phase still reports the reasoning wait.
            if let Some(started) = reasoning_phase.take() {
                let _ = sink.on_reasoning_end(started.elapsed().as_secs_f64()).await;
            }

            decode_meter.note_boundary();

            // Browser takeover may have cancelled the stream before another agent
            // event arrived, so classify the terminal state once more after the
            // loop. This turns the immediate cancellation into a resumable
            // takeover pause and preserves the post-takeover system alert.
            if !browser_takeover_paused && is_takeover_active_for_session(state, &session_id).await
            {
                browser_takeover_paused = true;
                cancellation_reason = None;
            }

            if browser_takeover_paused {
                const TAKEOVER_PAUSED_MESSAGE: &str =
                    "Browser takeover is active for this session; model execution paused";

                persist_stopped_terminal_calls(&mut terminal_snapshot, &mut turn_persister)?;
                if !segment_text.trim().is_empty() {
                    turn_persister
                        .push_assistant(segment_text.clone(), segment_emitted_at(segment_at));
                }

                let has_assistant_text = !full_text.trim().is_empty();
                let has_tool_activity =
                    !terminal_snapshot.executions.is_empty() || saw_tool_activity;
                if !has_assistant_text && has_tool_activity {
                    turn_persister.push_assistant(
                        String::new(),
                        last_event_at.expect("tool activity implies an agent event"),
                    );
                }

                // Flush the entire turn atomically
                turn_persister.flush(&state.db()).await?;
                let assistant_message_ids =
                    stored_assistant_message_ids(state, session_id.as_str(), turn_boundary).await?;
                clear_terminal_runtime_state(state, session_id.as_str()).await?;

                record_request_log(
                    state,
                    ledger.interrupted(Instant::now(), TAKEOVER_PAUSED_MESSAGE),
                )
                .await;
                schedule_browser_takeover_resume(state, session_id.as_str()).await?;

                return Err(AgentLoopError::BrowserTakeoverPaused(Box::new(
                    AgentRunResult {
                        user_message_id: turn_user_message_id,
                        assistant_message_ids,
                        full_text,
                        reasoning_text,
                        terminal_snapshot,
                        usage,
                        provider_id,
                        model_context_window,
                        elapsed_ms: elapsed_ms_saturating(&started_at),
                    },
                )));
            }

            // Handle terminal errors with provider failover.
            if let Some(err) = terminal_error {
                let attempt_ran_tools =
                    saw_tool_activity || !terminal_snapshot.executions.is_empty();
                let err_msg = err.to_string();
                // The model already ran tools: resubmitting the prompt would
                // replay them, so a `Resume` retry commits the partial turn
                // and continues the transcript instead. Partial text alone
                // is discarded and the prompt resubmits.
                let retry_kind = if attempt_ran_tools {
                    RetryKind::Resume
                } else {
                    RetryKind::Resubmit
                };
                // A request that streamed anything refreshed the provider's
                // cache entry; a bare failure did not.
                if candidate_warmth.is_none() || attempt_ran_tools || !full_text.is_empty() {
                    candidate_warmth = Some(agent.cache_warmth());
                }
                let warmth = candidate_warmth.as_ref().map_or(
                    aither_agent::CacheState::Cold,
                    aither_agent::CacheWarmth::state,
                );
                let candidate_left = candidate + 1 < candidate_count;
                // What a retry waits out first: the same provider's backoff
                // while its cache is warm, the next provider's short failover
                // backoff, or the quota reset the upstream announced.
                let retry_wait = match retry::decide(
                    err.failure(),
                    warmth,
                    candidate_left,
                    same_provider_retries,
                    state.connectivity.is_online(),
                ) {
                    retry::RetryDecision::Fail => None,
                    retry::RetryDecision::WaitForNetwork => Some(RetryWait::Network),
                    retry::RetryDecision::SameProvider { wait } => Some(RetryWait::SameProvider {
                        wait,
                        retry: same_provider_retries + 1,
                    }),
                    retry::RetryDecision::NextProvider => Some(RetryWait::Failover),
                    retry::RetryDecision::QuotaReset => state
                        .config_snapshot()
                        .preferences
                        .auto_continue_on_quota_reset
                        .then(|| quota::reset_at(&err_msg, jiff::Timestamp::now()))
                        .flatten()
                        .map(RetryWait::QuotaReset),
                };
                if let Some(wait) = retry_wait {
                    let turn_is_resume = kind.is_resume();
                    let kind = retry_kind;
                    record_request_log(state, ledger.interrupted(Instant::now(), &err_msg)).await;
                    // Text of a cut stream is discarded with the attempt;
                    // clients drop what they already rendered of it.
                    if kind == RetryKind::Resubmit && !full_text.is_empty() {
                        let _ = sink.on_retract().await;
                    }
                    match wait {
                        RetryWait::Failover => {
                            tokio::time::sleep(Duration::from_millis(
                                RETRY_BACKOFF_MS * (candidate as u64 + 1),
                            ))
                            .await;
                            candidate += 1;
                            same_provider_retries = 0;
                            candidate_warmth = None;
                        }
                        RetryWait::SameProvider { wait, retry } => {
                            let until = jiff::Timestamp::now()
                                .checked_add(wait)
                                .expect("a retry wait is far below the timestamp range");
                            let _ = sink
                                .on_hold(
                                    crate::client::HoldReason::Retry { until, retry },
                                    &err_msg,
                                )
                                .await;
                            if let Err(reason) = wait_or_cancel(guard, wait).await {
                                cancellation_reason = Some(reason);
                            }
                            same_provider_retries = retry;
                        }
                        RetryWait::QuotaReset(until) => {
                            let _ = sink
                                .on_hold(crate::client::HoldReason::Quota { until }, &err_msg)
                                .await;
                            if let Err(reason) = wait_quota_reset(guard, until).await {
                                cancellation_reason = Some(reason);
                            }
                        }
                        RetryWait::Network => {
                            let _ = sink
                                .on_hold(crate::client::HoldReason::Network, &err_msg)
                                .await;
                            if let Err(reason) =
                                wait_online_or_cancel(guard, &state.connectivity).await
                            {
                                cancellation_reason = Some(reason);
                            }
                            // The outage says nothing about the provider: the
                            // next failure starts its backoff afresh.
                            same_provider_retries = 0;
                        }
                    }
                    if cancellation_reason.is_none() {
                        if kind == RetryKind::Resume {
                            persist_stopped_terminal_calls(
                                &mut terminal_snapshot,
                                &mut turn_persister,
                            )?;
                            if !segment_text.trim().is_empty() {
                                turn_persister.push_assistant(
                                    segment_text.clone(),
                                    segment_emitted_at(segment_at),
                                );
                            }
                            turn_persister.flush(&state.db()).await?;
                            continue_from_transcript = true;
                        }
                        if turn_is_resume {
                            retained_agent = Some(agent);
                        }
                        attempt += 1;
                        continue;
                    }
                }

                if cancellation_reason.is_none() {
                    let retryable = err.failure().is_some_and(Failure::is_retryable);
                    record_request_log(state, ledger.interrupted(Instant::now(), &err_msg)).await;
                    let _ = sink
                        .on_error(ApiErrorCode::InternalError, &err_msg, retryable)
                        .await;
                    if let Some(entry_id) = req.pending_entry_id {
                        // Only a permanent failure fails the entry; a later
                        // run picks up anything else, an exhausted quota included.
                        let permanent = err
                            .failure()
                            .is_none_or(|failure| failure.kind == FailureKind::Permanent);
                        if permanent {
                            let _ = state
                                .db()
                                .fail_pending_claim(entry_id, guard.fence(), &err_msg)
                                .await;
                        } else {
                            let _ = state
                                .db()
                                .release_pending_claim(entry_id, guard.fence())
                                .await;
                        }
                    }
                    return Err(err.into());
                }
            }

            if let Some(reason) = cancellation_reason {
                if reason == "run cancelled by user" {
                    let killed = agent.kill_running_terminal_jobs().await;
                    tracing::info!(
                        session_id = %session_id,
                        killed,
                        "stopped terminal jobs after user cancelled the run"
                    );
                    persist_stopped_terminal_calls(&mut terminal_snapshot, &mut turn_persister)?;
                }
                if !segment_text.trim().is_empty() {
                    turn_persister
                        .push_assistant(segment_text.clone(), segment_emitted_at(segment_at));
                }
                // Flush the entire turn atomically before returning
                turn_persister.flush(&state.db()).await?;
                record_request_log(state, ledger.interrupted(Instant::now(), reason)).await;
                return Err(AgentLoopError::Cancelled(reason));
            }

            // Buffer remaining text segment.
            if !segment_text.trim().is_empty() {
                turn_persister.push_assistant(segment_text.clone(), segment_emitted_at(segment_at));
            }

            let has_assistant_text = !full_text.trim().is_empty();
            let has_terminal_output = !terminal_snapshot.executions.is_empty();
            let has_tool_activity = has_terminal_output || saw_tool_activity;
            if !has_assistant_text && !has_tool_activity {
                let err = "Model returned an empty assistant response";
                record_request_log(state, ledger.interrupted(Instant::now(), err)).await;
                let _ = sink.on_error(ApiErrorCode::InternalError, err, false).await;
                return Err(AgentLoopError::EmptyAssistantResponse);
            }
            if !has_assistant_text && has_tool_activity {
                turn_persister.push_assistant(
                    String::new(),
                    last_event_at.expect("tool activity implies an agent event"),
                );
            }

            // Flush the entire turn atomically
            turn_persister.flush(&state.db()).await?;
            let assistant_message_ids =
                stored_assistant_message_ids(state, session_id.as_str(), turn_boundary).await?;
            clear_terminal_runtime_state(state, session_id.as_str()).await?;

            // Final sample carries the exact rate now that Usage has arrived.
            if let Some(rate) = decode_meter.rate() {
                let _ = sink.on_speed(rate).await;
            }

            let elapsed_ms = elapsed_ms_saturating(&started_at);
            persist_latest_assistant_stats(
                state,
                &session_id,
                &provider_id,
                &usage,
                model_context_window,
                elapsed_ms,
            )
            .await?;
            if let Some(entry) = ledger.finished(Instant::now()) {
                record_request_log(state, entry).await;
            }

            // Post-completion hooks
            schedule_profile_memory_processing(state, profile_id.as_deref()).await;
            title_generation::enqueue(state, &session_id, req.content.clone(), full_text.clone());
            enqueue_event_triggered_tasks(state, &session_id, profile_id.as_deref());
            // The turn's messages are in the search outbox now.
            let _ = state.search_index_wake_tx.try_send(());
            // The agent stays as the session's idle agent until the next turn;
            // it waits on background commands once this run releases the
            // session.
            state.parked_agents.park(
                state,
                session_id.as_str(),
                agent,
                build_key,
                guard.released(),
            );

            return Ok(AgentRunResult {
                user_message_id: turn_user_message_id,
                assistant_message_ids,
                full_text,
                reasoning_text,
                terminal_snapshot,
                usage,
                provider_id,
                model_context_window,
                elapsed_ms,
            });
        }
    })
}

/// Whether an agent event is the first visible output that ends a pending
/// reasoning phase. `Complete` is excluded: an armed phase that reaches
/// stream end without output is closed once, after the loop.
///
/// `TurnComplete { has_tool_calls: true }` is also excluded. Tools already
/// closed the phase at `ToolCallStart`; `ToolCallEnd` then re-arms it for
/// the next model call. Treating the boundary marker as visible output
/// closed that next phase at 0 seconds and left a bare `Thought` in front
/// of the following burst.
const fn closes_reasoning_phase(event: &aither_agent::AgentEvent) -> bool {
    matches!(
        event,
        aither_agent::AgentEvent::Text(_)
            | aither_agent::AgentEvent::ToolCallDelta { .. }
            | aither_agent::AgentEvent::ToolCallStart { .. }
            | aither_agent::AgentEvent::BuiltInToolCall { .. }
            | aither_agent::AgentEvent::TurnComplete {
                has_tool_calls: false,
                ..
            }
            | aither_agent::AgentEvent::Error(_)
    )
}

/// Runs one turn under `guard`, publishing it to the session's clients, and
/// settles the guard: a continued guard (more queued work) runs next.
/// Starts a turn for `session_id` whose idle agent collected background
/// commands that need the model (`wake`). An idle agent waits on background
/// commands only after the run that parked it released the session, so the
/// only thing that can hold the session here is a new turn, which takes the
/// parked agent and sees the results; nothing runs then.
pub(super) async fn start_background_turn(
    state: &Arc<DaemonState>,
    session_id: &str,
    wake: aither_agent::BackgroundWake,
) {
    if !state.parked_agents.contains(session_id) {
        return;
    }
    let guard = match state.runtime_service.runs().acquire(session_id).await {
        Ok(guard) => guard,
        Err(error) => {
            tracing::info!(%session_id, %error, "a new turn holds the session and takes the parked agent, which sees the background results");
            return;
        }
    };
    let req = ChatRequest {
        session_id: session_id.to_string(),
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: None,
        delivery_mode: crate::session::PendingUserMessageMode::default(),
        pending_entry_id: None,
    };
    tracing::info!(%session_id, events = wake.events.len(), "background commands resume the session");
    if let Err(error) = Box::pin(run_published_turn(
        state.clone(),
        req,
        guard,
        TurnKind::Resume(wake.events),
    ))
    .await
    {
        tracing::error!(%session_id, %error, "failed to settle a background turn");
    }
}

/// Applies the current `[suggestions]` and `[recap]` settings to work
/// already scheduled: a setting turned off cancels its armed timers and
/// in-flight generations.
fn apply_aux_settings(state: &Arc<DaemonState>) {
    suggest::apply_settings(state);
    recap::apply_settings(state);
}

/// Applies the aux settings whenever the configuration changes, for the
/// daemon's lifetime.
pub(super) fn watch_aux_settings(state: &Arc<DaemonState>) {
    let mut events = state.ui_event_bus.subscribe();
    let state = Arc::clone(state);
    let handle = state.tokio_handle.clone();
    handle.spawn(async move {
        loop {
            match events.recv().await {
                Ok(topics) if topics.intersects(UiEventTopics::CONFIG | UiEventTopics::SLEEP) => {
                    apply_aux_settings(&state);
                }
                // A lagged receiver may have missed the change.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    apply_aux_settings(&state);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// A completed turn left `session_id` idle with nothing pending: it gets one
/// suggested next prompt and an armed idle-recap timer. Every path that ends
/// a session's turns calls this, so a published, queued or background turn
/// leaves the same state behind.
fn session_became_idle(state: &Arc<DaemonState>, session_id: &str) {
    suggest::maybe_suggest(state, session_id);
    recap::arm(state, session_id);
}

async fn run_published_turn(
    state: Arc<DaemonState>,
    req: ChatRequest,
    guard: SessionRunGuard,
    kind: TurnKind,
) -> Result<(), crate::db::ChatDbError> {
    let mut sink = HubSink::new(&state, &req);
    let result = run_agent_loop(&state, &req, &mut sink, &guard, true, true, &kind).await;
    let response_session_id = req.session_id.clone();
    match result {
        Ok(result) => {
            let done_event = done_event_from_run_result(
                sink.next_seq(),
                sink.turn_id.clone(),
                response_session_id.clone(),
                result,
            );
            // Release the session before announcing the end of the turn, so
            // a client that sends its next message on `done` is never refused
            // as a concurrent run. Finishing continues the session under a
            // new guard only when there is pending work to dispatch.
            let continued = guard.finish(RunOutcome::Completed).await?;
            sink.finish(
                Some(("done", &done_event)),
                crate::client::RunEnd::Completed,
            );
            if let Some(continued) = continued {
                spawn_continued_run(state.clone(), continued);
            } else {
                session_became_idle(&state, &response_session_id);
            }
        }
        Err(AgentLoopError::BrowserTakeoverPaused(result)) => {
            let done_event = done_event_from_run_result(
                sink.next_seq(),
                sink.turn_id.clone(),
                response_session_id.clone(),
                *result,
            );
            let continued = guard.finish(RunOutcome::Completed).await?;
            sink.finish(Some(("paused", &done_event)), crate::client::RunEnd::Paused);
            if let Some(continued) = continued {
                spawn_continued_run(state.clone(), continued);
            }
        }
        // Errors are already published through on_error in run_agent_loop.
        Err(AgentLoopError::Cancelled(_)) => {
            guard.finish_idle(RunOutcome::Cancelled).await?;
            sink.finish(None, crate::client::RunEnd::Cancelled);
        }
        Err(_) => {
            guard.finish_idle(RunOutcome::Failed).await?;
            sink.finish(None, crate::client::RunEnd::Failed);
        }
    }
    Ok(())
}

/// How a turn driven by [`run_chat_collect_with_receiver`] ended without
/// failing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnEnd {
    Completed,
    /// A browser takeover paused the turn; the session is not idle.
    Paused,
}

/// Drive a chat turn without an attached SSE client — used by the follow-up
/// queue to dispatch server-side enqueued messages. All assistant text and
/// tool events are still persisted by the main loop; the sink only
/// suppresses network emission. The caller keeps the run's guard and settles
/// it once the queue is drained.
async fn run_chat_collect_with_receiver(
    state: Arc<DaemonState>,
    req: ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    guard: &SessionRunGuard,
) -> Result<TurnEnd, ChatCollectError> {
    let mut sink = HubSink::new(&state, &req);
    let result = run_agent_loop(
        &state,
        &req,
        &mut sink,
        guard,
        persist_user_message,
        use_persisted_context_snapshot,
        &TurnKind::Prompt,
    )
    .await;
    match result {
        Ok(_) => {
            sink.finish(None, crate::client::RunEnd::Completed);
            Ok(TurnEnd::Completed)
        }
        Err(AgentLoopError::BrowserTakeoverPaused(_)) => {
            sink.finish(None, crate::client::RunEnd::Paused);
            Ok(TurnEnd::Paused)
        }
        Err(AgentLoopError::Preparation(error)) => {
            sink.finish(None, crate::client::RunEnd::Failed);
            Err(ChatCollectError::Preparation(error))
        }
        Err(AgentLoopError::Cancelled(reason)) => {
            sink.finish(None, crate::client::RunEnd::Cancelled);
            Err(ChatCollectError::Sky(internal_error(
                AgentLoopError::Cancelled(reason),
            )))
        }
        Err(other) => {
            sink.finish(None, crate::client::RunEnd::Failed);
            Err(ChatCollectError::Sky(internal_error(other)))
        }
    }
}

async fn schedule_profile_memory_processing(state: &Arc<DaemonState>, profile_id: Option<&str>) {
    let Some(profile_id) = profile_id else {
        return;
    };
    let Some(processor) = state.memory_processor.as_ref() else {
        return;
    };
    processor.mark_profile_dirty(profile_id).await;
}

async fn persist_latest_assistant_stats(
    state: &Arc<DaemonState>,
    session_id: &SessionId,
    provider_id: &str,
    usage: &Usage,
    model_context_window: Option<u32>,
    elapsed_ms: u64,
) -> Result<(), crate::db::ChatDbError> {
    let total_tokens = usage.total_tokens.or_else(|| {
        usage
            .prompt_tokens
            .zip(usage.completion_tokens)
            .map(|(p, c)| p + c)
    });
    state
        .db()
        .set_latest_assistant_chat_stats(
            session_id.as_str(),
            &ChatMessageStats {
                provider_id: Some(provider_id.to_string()),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                context_window: model_context_window,
                elapsed_ms: Some(elapsed_ms),
            },
        )
        .await
}

const fn map_llm_role(role: LlmRole) -> &'static str {
    match role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
        LlmRole::Tool => "tool",
    }
}

fn map_context_debug_message(message: &LlmMessage) -> crate::db::ContextDebugSnapshotMessage {
    crate::db::ContextDebugSnapshotMessage {
        role: map_llm_role(message.role()).to_string(),
        content: message.content().to_string(),
        attachments: message
            .attachments()
            .iter()
            .map(|attachment| attachment.url().to_string())
            .collect(),
        tool_call_id: message.tool_call_id().map(ToString::to_string),
        tool_call_names: message
            .tool_calls()
            .iter()
            .map(|call| call.name.clone())
            .collect(),
    }
}

async fn persist_context_debug_snapshot(
    state: &Arc<DaemonState>,
    session_id: &SessionId,
    model: &str,
    provider_id: &str,
    prompt: &str,
    prompt_attachments: &[LlmAttachment],
    assembled_messages: &[LlmMessage],
) -> Result<(), crate::db::ChatDbError> {
    let prompt_attachments = prompt_attachments
        .iter()
        .map(|attachment| attachment.url().to_string())
        .collect::<Vec<_>>();
    let assembled_messages = assembled_messages
        .iter()
        .map(map_context_debug_message)
        .collect::<Vec<_>>();
    let timestamp = jiff::Timestamp::now().to_string();

    let db = state.db();
    db.upsert_context_debug_snapshot(&crate::db::ContextDebugSnapshotWrite {
        session_id: session_id.as_str(),
        timestamp: &timestamp,
        model,
        provider_id,
        prompt,
        prompt_attachments: &prompt_attachments,
        assembled_messages: &assembled_messages,
    })
    .await
}

fn enqueue_event_triggered_tasks(
    state: &Arc<DaemonState>,
    session_id: &SessionId,
    profile_id: Option<&str>,
) {
    let state = state.clone();
    let session_id = session_id.as_str().to_string();
    let profile_id = profile_id.map(ToOwned::to_owned);

    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        if let Err(e) = state
            .task_runtime
            .dispatch_event(
                EVENT_CHAT_TURN_COMPLETED,
                profile_id.as_deref(),
                Some(session_id.as_str()),
            )
            .await
        {
            tracing::error!(error = %e, "failed to dispatch event-triggered tasks");
        }
    });
}

// ── Agent preparation: typed intermediate structs ─────────

struct SessionContext {
    /// Row id of the user message this turn stored, when it stored one.
    user_message_id: Option<i32>,
    session_id: SessionId,
    current_model: String,
    provider_candidate_count: usize,
    sandbox_dir: std::path::PathBuf,
    /// Host directory the session works in.
    cwd: std::path::PathBuf,
    /// Whether nobody answers this session's prompts.
    non_interactive: bool,
    /// The rules this session imports, for a session `/import-claude` or
    /// `/import-codex` opened.
    rules_import: Option<crate::rules_import::RulesImport>,
    /// Model transcript entries as `(seq, message)`: exactly what was
    /// committed to the model context, never UI history.
    transcript: Vec<(i64, LlmMessage)>,
    /// UI-history messages to seed a first-ever agent with when the
    /// transcript is empty; the agent assigns its own seqs before the
    /// transcript store records them.
    seed_messages: Vec<LlmMessage>,
    context_snapshot: Option<crate::session::PersistedContextSnapshot>,
    hidden_context_xml: Option<String>,
    profile_persona: Option<String>,
    profile_memories: Option<crate::memory::ProfileMemories>,
    profile_model: Option<crate::db::entity::profiles::Model>,
    profile_environment: Option<crate::profile::ProfileEnvironment>,
    profile_id: Option<String>,
    plan_state: crate::session::SessionPlanState,
    reasoning_effort: Option<aither_core::llm::model::ReasoningEffort>,
}

struct PreparedAgent {
    session_id: SessionId,
    agent: MayAgent,
    current_model: String,
    provider_id: String,
    profile_id: Option<String>,
    hidden_context_xml: Option<String>,
    prompt: String,
    attachments: Vec<LlmAttachment>,
    provider_candidate_count: usize,
    /// [`agent_build_key`] of the inputs the agent was built from.
    build_key: u64,
    /// Row id of the user message this turn stored, when it stored one.
    user_message_id: Option<i32>,
    /// Reasoning effort resolved for this turn's requests.
    reasoning_effort: Option<aither_core::llm::model::ReasoningEffort>,
}

struct ToolBrokers {
    workspace: crate::workspace_request::WorkspaceRequestBroker,
    ask_user: ask_user::AskUserBroker,
    task: crate::agent::task_tool::TaskBroker,
    notify: crate::agent::notify_tool::NotifyBroker,
    browser: crate::agent::browser_tool::BrowserBroker,
    register_skills: crate::agent::register_tool::RegisterBroker,
    tunnel: crate::agent::tunnel_tool::TunnelBroker,
}

#[derive(Debug, thiserror::Error)]
enum UiChatPreparationError {
    /// The idle agent whose background commands asked for a turn is gone:
    /// a turn of the session took it first.
    #[error("the session's idle agent was taken by another turn")]
    NothingToResume,
    #[error("Browser takeover is active for this session; wait until user returns control")]
    BrowserTakeoverActive,
    #[error("Session {0} not found")]
    SessionNotFound(SessionId),
    #[error("failed to load session: {0}")]
    LoadSession(#[source] crate::db::ChatDbError),
    #[error("failed to load session history: {0}")]
    LoadHistory(#[source] crate::session::SessionError),
    #[error("No model configured")]
    MissingModelConfiguration,
    #[error("No provider available for model '{model}'")]
    NoProviderAvailable { model: String },
    #[error("failed to load profile: {0}")]
    LoadProfile(#[source] crate::db::ChatDbError),
    #[error("failed to parse profile environment: {0}")]
    ParseProfileEnvironment(#[source] crate::db::ChatDbError),
    #[error("failed to load profile memories: {0}")]
    LoadProfileMemories(#[source] crate::db::ChatDbError),
    #[error("failed to load session runtime: {0}")]
    LoadRuntime(#[source] crate::db::ChatDbError),
    #[error("failed to load session context snapshot: {0}")]
    LoadContextSnapshot(#[source] crate::db::ChatDbError),
    #[error("failed to replay session history: {0}")]
    HistoryReplay(#[source] crate::session::HistoryReplayError),
    #[error("failed to build session mentions: {0}")]
    BuildSessionMentions(#[source] crate::prompt_mentions::PromptMentionError),
    #[error("{0}")]
    InvalidFileMentionPath(crate::prompt_mentions::PromptMentionError),
    #[error("failed to build file mentions: {0}")]
    BuildFileMentions(#[source] crate::prompt_mentions::PromptMentionError),
    #[error("failed to update session title: {0}")]
    UpdateSessionTitle(#[source] crate::db::ChatDbError),
    #[error("failed to append user message: {0}")]
    AppendUserMessage(#[source] crate::db::ChatDbError),
    #[error("failed to persist hidden context: {0}")]
    PersistHiddenContext(#[source] crate::db::ChatDbError),
    #[error("container profile model is missing")]
    MissingContainerProfileModel,
    #[error("failed to reconcile container session layout: {0}")]
    ReconcileContainerLayout(#[source] crate::session::StoreError),
    #[error("no container runtime available for container profile: {0}")]
    ResolveContainerRuntime(#[source] crate::container::ContainerError),
    #[error("container image pull failed: {0}")]
    EnsureContainerImage(#[source] crate::container::ContainerError),
    #[error("container start failed: {0}")]
    StartContainer(#[source] crate::container::ContainerError),
    #[error("container IPC route resolution failed: {0}")]
    ResolveContainerIpcRoute(#[source] crate::container::ContainerError),
    #[error("location fetch failed: {0}")]
    LoadLocation(#[source] crate::location::LocationError),
    #[error("failed to create agent: {0}")]
    BuildAgent(#[from] crate::agent::MayAgentError),
    #[error("failed to restore agent checkpoint: {0}")]
    RestoreCheckpoint(#[source] aither_agent::AgentError),
    #[error("failed to build prompt attachments: {0}")]
    BuildPrompt(skyzen::Error),
    #[error("failed to preview request messages: {0}")]
    PreviewRequest(#[source] aither_agent::AgentError),
    #[error("failed to persist context debug snapshot: {0}")]
    PersistContextDebugSnapshot(#[source] crate::db::ChatDbError),
}

impl UiChatPreparationError {
    const fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::BrowserTakeoverActive => ApiErrorCode::BrowserTakeoverActive,
            Self::SessionNotFound(_) => ApiErrorCode::SessionNotFound,
            Self::MissingModelConfiguration => ApiErrorCode::ModelNotConfigured,
            Self::NoProviderAvailable { .. } => ApiErrorCode::ProviderUnavailable,
            Self::InvalidFileMentionPath(_) => ApiErrorCode::BadRequest,
            Self::MissingContainerProfileModel | Self::ParseProfileEnvironment(_) => {
                ApiErrorCode::ProfileConfigurationInvalid
            }
            Self::ResolveContainerRuntime(_) | Self::ResolveContainerIpcRoute(_) => {
                ApiErrorCode::ContainerRuntimeUnavailable
            }
            Self::EnsureContainerImage(_) => ApiErrorCode::ContainerPullFailed,
            Self::StartContainer(_) => ApiErrorCode::ContainerStartFailed,
            Self::LoadLocation(_) => ApiErrorCode::LocationUnavailable,
            Self::LoadSession(_)
            | Self::LoadHistory(_)
            | Self::LoadProfile(_)
            | Self::LoadProfileMemories(_)
            | Self::LoadRuntime(_)
            | Self::LoadContextSnapshot(_)
            | Self::BuildSessionMentions(_)
            | Self::BuildFileMentions(_)
            | Self::UpdateSessionTitle(_)
            | Self::AppendUserMessage(_)
            | Self::PersistHiddenContext(_)
            | Self::ReconcileContainerLayout(_)
            | Self::BuildAgent(_)
            | Self::RestoreCheckpoint(_)
            | Self::BuildPrompt(_)
            | Self::PreviewRequest(_)
            | Self::PersistContextDebugSnapshot(_)
            | Self::HistoryReplay(_)
            | Self::NothingToResume => ApiErrorCode::InternalError,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ChatMarkupError {
    #[error("failed to parse XML fragment: {0}")]
    Parse(#[source] crate::XmlBlockError),
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ScheduleWaitingTurnError {
    #[error(transparent)]
    Markup(#[from] ChatMarkupError),
    #[error("failed to enqueue waiting-turn resume message: {0}")]
    Enqueue(#[source] crate::db::ChatDbError),
    #[error("failed to take the session run lease for the resume: {0}")]
    Lease(#[source] crate::db::ChatDbError),
}

#[derive(Debug, thiserror::Error)]
enum AgentLoopError {
    #[error(transparent)]
    Preparation(#[from] UiChatPreparationError),
    #[error("failed to persist chat state: {0}")]
    Storage(#[from] crate::db::ChatDbError),
    #[error("client disconnected before stream start")]
    ClientDisconnectedBeforeStreamStart,
    #[error(transparent)]
    Agent(#[from] aither_agent::AgentError),
    #[error("{0}")]
    Cancelled(&'static str),
    #[error("Browser takeover is active for this session; model execution paused")]
    BrowserTakeoverPaused(Box<AgentRunResult>),
    #[error("Model returned an empty assistant response")]
    EmptyAssistantResponse,
}

enum ChatCollectError {
    Preparation(UiChatPreparationError),
    Sky(skyzen::Error),
}

impl std::fmt::Display for ChatCollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preparation(error) => std::fmt::Display::fmt(error, f),
            Self::Sky(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl From<skyzen::Error> for ChatCollectError {
    fn from(error: skyzen::Error) -> Self {
        Self::Sky(error)
    }
}

// ── Agent preparation: decomposed steps ───────────────────

async fn resolve_session_context(
    state: &Arc<DaemonState>,
    req: &ChatRequest,
    fence: &RunFence,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    turn_boundary: Option<i32>,
) -> Result<SessionContext, UiChatPreparationError> {
    let session_id = SessionId::from_string(req.session_id.clone());

    if is_takeover_active_for_session(state, &session_id).await {
        return Err(UiChatPreparationError::BrowserTakeoverActive);
    }

    let db = state.db();
    let session = db
        .get_session(session_id.as_str())
        .await
        .map_err(UiChatPreparationError::LoadSession)?
        .ok_or_else(|| UiChatPreparationError::SessionNotFound(session_id.clone()))?;
    let mut history = crate::session::load_history_from_db(&db, session_id.as_str())
        .await
        .map_err(UiChatPreparationError::LoadHistory)?;
    if let Some(boundary) = turn_boundary {
        // A resubmitted prompt attempt rebuilds context from before the
        // turn: its own rows were persisted by attempt 0 and the prompt
        // carries the user message, so replaying them too would double
        // the input.
        history
            .messages_mut()
            .retain(|message| message.id.is_none_or(|id| id <= boundary));
    }

    let session_model = session.model.clone();
    let cfg = state.config_snapshot();
    let current_model = resolve_default_model(&cfg, session_model)
        .ok_or(UiChatPreparationError::MissingModelConfiguration)?;
    let provider_candidate_count = crate::agent::provider_candidate_count(&cfg, &current_model);
    if provider_candidate_count == 0 {
        return Err(UiChatPreparationError::NoProviderAvailable {
            model: current_model.clone(),
        });
    }
    drop(cfg);

    let profile_model = if let Some(profile_id) = session.profile_id.clone() {
        db.get_profile(&profile_id)
            .await
            .map_err(UiChatPreparationError::LoadProfile)?
    } else {
        None
    };
    let profile_persona = profile_model.as_ref().and_then(|p| p.persona.clone());
    let profile_environment = match profile_model.clone() {
        Some(profile) => Some(
            db.profile_info_from_model(profile)
                .await
                .map_err(UiChatPreparationError::ParseProfileEnvironment)?
                .environment
                .into_kind(),
        ),
        None => None,
    };

    let profile_memories = if let Some(ref profile) = profile_model {
        let memories = crate::memory::ProfileMemories::load(&db, &profile.id)
            .await
            .map_err(UiChatPreparationError::LoadProfileMemories)?;
        if memories.is_empty() {
            None
        } else {
            Some(memories)
        }
    } else {
        None
    };
    let runtime = db
        .get_session_runtime(session_id.as_str())
        .await
        .map_err(UiChatPreparationError::LoadRuntime)?
        .unwrap_or_default();

    let mut transcript = Vec::<(i64, LlmMessage)>::new();
    for entry in db
        .transcript_entries_after(session_id.as_str(), -1)
        .await
        .map_err(UiChatPreparationError::LoadContextSnapshot)?
    {
        let message: LlmMessage = rmp_serde::from_slice(&entry.message_blob).map_err(|error| {
            UiChatPreparationError::LoadContextSnapshot(crate::db::ChatDbError::Storage(format!(
                "failed to decode transcript entry {}: {error}",
                entry.seq
            )))
        })?;
        transcript.push((entry.seq, message));
    }
    // One-time migration: a session with only UI history feeds it to the
    // fresh agent once; `build_session_agent` then persists the agent's own
    // checkpoint, so every transcript seq is one aither assigned.
    let seed_messages = if transcript.is_empty() {
        history
            .to_aither_messages()
            .map_err(UiChatPreparationError::HistoryReplay)?
    } else {
        Vec::new()
    };
    let context_snapshot = if use_persisted_context_snapshot {
        db.get_session_context_snapshot(session_id.as_str())
            .await
            .map_err(UiChatPreparationError::LoadContextSnapshot)?
            .map(crate::session::parse_persisted_context_snapshot)
    } else {
        None
    };

    let mut hidden_context_parts = Vec::<String>::new();
    // A claimed entry with no visible input commits with the first hidden
    // context row the run persists for it; a visible message commits it
    // instead, so a failure between the two never leaves it half-delivered.
    let mut hidden_commit_entry = req
        .pending_entry_id
        .filter(|_| persist_user_message && !request_has_visible_user_input(req));
    if persist_user_message && take_over_alert_pending(state, &session_id).await {
        push_hidden_context_part(
            &db,
            fence,
            &history,
            &mut hidden_context_parts,
            browser_takeover_alert(),
            persist_user_message,
            &mut hidden_commit_entry,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    if let Some(hidden_context_xml) = req
        .hidden_context_xml
        .as_deref()
        .map(str::trim)
        .filter(|xml| !xml.is_empty())
    {
        push_hidden_context_part(
            &db,
            fence,
            &history,
            &mut hidden_context_parts,
            hidden_context_xml.to_string(),
            persist_user_message,
            &mut hidden_commit_entry,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    if let Some(session_mentions_xml) = prompt_mentions::build_session_mentions(&db, &req.content)
        .await
        .map_err(UiChatPreparationError::BuildSessionMentions)?
    {
        push_hidden_context_part(
            &db,
            fence,
            &history,
            &mut hidden_context_parts,
            session_mentions_xml,
            persist_user_message,
            &mut hidden_commit_entry,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    let session_paths =
        session_paths_for_session(state, session.profile_id.as_deref(), &session_id);
    if let Some(file_mentions_xml) =
        prompt_mentions::build_file_mentions(&session_paths.files_dir(), &req.content, 400)
            .await
            .map_err(|error| match error {
                prompt_mentions::PromptMentionError::AbsolutePath
                | prompt_mentions::PromptMentionError::ParentTraversal => {
                    UiChatPreparationError::InvalidFileMentionPath(error)
                }
                prompt_mentions::PromptMentionError::SessionLookup(_) => {
                    UiChatPreparationError::BuildFileMentions(error)
                }
            })?
    {
        push_hidden_context_part(
            &db,
            fence,
            &history,
            &mut hidden_context_parts,
            file_mentions_xml,
            persist_user_message,
            &mut hidden_commit_entry,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    if let Some(entry_id) = hidden_commit_entry.take() {
        // Every hidden part was already in the transcript: nothing carried
        // the commit, so it lands on its own.
        db.commit_pending_entry_as_run(fence, entry_id)
            .await
            .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    let user_message_id = if persist_user_message && request_has_visible_user_input(req) {
        let user_message = ChatMessage::user_with_attachments(
            req.content.clone(),
            persisted_attachments(req.attachments.clone()),
        );
        // A claimed queue entry commits in the same fenced transaction as
        // its user message: either both land or neither does.
        let row = if let Some(entry_id) = req.pending_entry_id {
            db.append_chat_message_as_run_and_commit_pending(fence, entry_id, &user_message)
                .await
        } else {
            db.append_chat_message_as_run(fence, &user_message).await
        }
        .map_err(UiChatPreparationError::AppendUserMessage)?;
        if session.title.is_none() {
            // The prompt row lands first: a client reloading history on the
            // title's `updated_at` drift already sees the message.
            let title = truncate_preview(&req.content);
            db.update_session(session_id.as_str(), Some(title.as_str()), None, None)
                .await
                .map_err(UiChatPreparationError::UpdateSessionTitle)?;
            state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
        }
        Some(row.id)
    } else {
        None
    };

    let sandbox_dir = SessionStore::new(MayHomePaths::new(state.may_dir.clone()))
        .sandbox_dir(session_id.as_str(), session.profile_id.as_deref());

    // Effective effort: the rung the session selected, else the last rung
    // selected anywhere, else medium — "default" is the last selection, not
    // a provider-decided level.
    let config = state.config_snapshot();
    let reasoning_effort = session
        .reasoning_effort
        .as_deref()
        .or_else(|| config.last_effort.as_deref())
        .unwrap_or("medium")
        .to_string();
    let reasoning_effort = parse_reasoning_effort_token(&reasoning_effort);

    let cwd = crate::session::SessionDirectory::of(&session, &state.may_dir).into_path();
    let rules_import = session.rules_import.as_deref().map(|label| {
        let source = crate::session::import::ForeignAgent::from_label(label)
            .unwrap_or_else(|| panic!("session {session_id} imports rules from unknown `{label}`"));
        let home = crate::config::MayConfig::home_dir()
            .expect("the home directory resolved when the May directory was derived");
        crate::rules_import::RulesImport::new(source, &home, &state.may_dir)
    });

    Ok(SessionContext {
        user_message_id,
        session_id,
        current_model,
        provider_candidate_count,
        sandbox_dir,
        cwd,
        non_interactive: session.non_interactive,
        rules_import,
        transcript,
        seed_messages,
        context_snapshot,
        hidden_context_xml: (!hidden_context_parts.is_empty())
            .then(|| hidden_context_parts.join("\n\n")),
        profile_persona,
        profile_memories,
        profile_model,
        profile_environment,
        profile_id: session.profile_id,
        plan_state: runtime.plan_state,
        reasoning_effort,
    })
}

fn parse_reasoning_effort_token(token: &str) -> Option<aither_core::llm::model::ReasoningEffort> {
    use aither_core::llm::model::ReasoningEffort;
    match token.trim().to_ascii_lowercase().as_str() {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::XHigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

fn session_paths_for_session(
    state: &DaemonState,
    profile_id: Option<&str>,
    session_id: &SessionId,
) -> SessionPaths {
    let home_paths = MayHomePaths::new(state.may_dir.clone());
    profile_id.map_or_else(
        || home_paths.session_paths(session_id.as_str()),
        |profile_id| {
            home_paths
                .profile_home(profile_id)
                .session_paths(session_id.as_str())
        },
    )
}

async fn push_hidden_context_part(
    db: &crate::db::ChatDb,
    fence: &RunFence,
    history: &ChatHistory,
    hidden_context_parts: &mut Vec<String>,
    content: String,
    persist_user_message: bool,
    commit_entry: &mut Option<i64>,
) -> Result<(), crate::db::ChatDbError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let already_queued = hidden_context_parts.iter().any(|part| part == trimmed);
    if persist_user_message
        && !already_queued
        && !history.messages().iter().any(|message| {
            matches!(message.role, MessageRole::HiddenContext) && message.content == trimmed
        })
    {
        let message = ChatMessage::hidden_context(trimmed.to_string());
        match commit_entry.take() {
            Some(entry_id) => {
                db.append_chat_message_as_run_and_commit_pending(fence, entry_id, &message)
                    .await?;
            }
            None => {
                db.append_chat_message_as_run(fence, &message).await?;
            }
        }
    }

    if !already_queued {
        hidden_context_parts.push(trimmed.to_string());
    }

    Ok(())
}

async fn setup_agent_environment(
    state: &Arc<DaemonState>,
    ctx: &SessionContext,
) -> Result<
    (
        crate::agent::AgentEnvironment,
        Option<crate::location::LocationInfo>,
    ),
    UiChatPreparationError,
> {
    let environment = match ctx.profile_environment.as_ref() {
        Some(crate::profile::ProfileEnvironment::Container { .. }) => {
            let profile = ctx
                .profile_model
                .as_ref()
                .ok_or(UiChatPreparationError::MissingContainerProfileModel)?;
            let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
            store
                .reconcile_container_mount_points(
                    ctx.session_id.as_str(),
                    ctx.profile_id.as_deref(),
                )
                .await
                .map_err(UiChatPreparationError::ReconcileContainerLayout)?;
            let container_manager = state
                .resolve_container_manager(false)
                .await
                .map_err(UiChatPreparationError::ResolveContainerRuntime)?;
            let container_preferences = state
                .db()
                .load_container_profile_preferences(profile)
                .await
                .map_err(UiChatPreparationError::ParseProfileEnvironment)?;
            let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences);
            let image = crate::container::image::MAY_WORKSPACE_IMAGE;

            crate::container::image::ensure_image(container_manager.runtime(), image)
                .await
                .map_err(UiChatPreparationError::EnsureContainerImage)?;

            let container_name = crate::container::profile::container_name_for_profile_session(
                profile,
                &ctx.session_id,
            );
            let host_gateway = container_manager.runtime().host_gateway();

            let mut extra_env = Vec::new();
            let browser_cdp_endpoint: Option<String> = if let Some(bm) = state.browser_manager() {
                if let Err(error) = ensure_browser_runtime_started(state.as_ref()).await {
                    tracing::warn!(
                        error = %error_chain_text(&error),
                        "Failed to auto-start browser for container session"
                    );
                    None
                } else {
                    let config = state.config_snapshot();
                    let endpoint = if config.browser.requires_managed_runtime() {
                        format!("http://{host_gateway}:{}", config.browser.port)
                    } else {
                        bm.cdp_endpoint().unwrap_or_else(|| {
                            panic!(
                                "browser manager reported running remote backend without cdp endpoint"
                            )
                        })
                    };
                    extra_env.push(format!("BROWSER_CDP_ENDPOINT={endpoint}"));
                    Some(endpoint)
                }
            } else {
                None
            };

            let mut launch_spec = ContainerLaunchSpec::new(
                container_name,
                image.to_string(),
                ctx.sandbox_dir.clone(),
            );
            for mount in extra_mounts {
                launch_spec = launch_spec.with_mount(mount);
            }
            for env in extra_env {
                launch_spec = launch_spec.with_env(env);
            }

            let container_id = container_manager
                .ensure_running(&launch_spec)
                .await
                .map_err(UiChatPreparationError::StartContainer)?;

            let container_ipc_route = container_manager
                .runtime()
                .ipc_route(&container_id)
                .await
                .map_err(UiChatPreparationError::ResolveContainerIpcRoute)?;
            let container_exec =
                AnyContainerExec::new(std::sync::Arc::new(BollardContainerExec::new(
                    std::sync::Arc::new(container_manager.runtime().client.clone()),
                )));

            crate::agent::AgentEnvironment::Container {
                container_id,
                container_ipc_route,
                container_exec,
                browser_cdp_endpoint,
            }
        }
        _ => crate::agent::AgentEnvironment::Local,
    };

    let location = if ctx.profile_model.as_ref().is_some_and(|p| p.location_aware) {
        Some(
            state
                .location_provider
                .get()
                .await
                .map_err(UiChatPreparationError::LoadLocation)?,
        )
    } else {
        None
    };

    Ok((environment, location))
}

fn spawn_ask_user_forwarder(
    state: &Arc<DaemonState>,
    session_id: &SessionId,
) -> ask_user::AskUserBroker {
    let (ask_user_broker, ask_user_queue) = ask_user::channel();
    let ask_user_approver = state.ask_user_approver.clone();
    let ask_user_session_id = session_id.as_str().to_string();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = ask_user_queue.next().await {
            ask_user_approver
                .enqueue(request, Some(ask_user_session_id.clone()))
                .await;
        }
    });
    ask_user_broker
}

fn spawn_workspace_forwarder(
    state: &Arc<DaemonState>,
    session_id: &SessionId,
) -> crate::workspace_request::WorkspaceRequestBroker {
    let (workspace_broker, workspace_queue) = crate::workspace_request::channel();
    let workspace_approver = state.workspace_approver.clone();
    let workspace_session_id = session_id.as_str().to_string();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = workspace_queue.next().await {
            let approved = match workspace_approver
                .request_and_wait(workspace_session_id.as_str(), request.args.request.clone())
                .await
            {
                Ok(approved) => approved,
                Err(error) => {
                    tracing::error!(session_id = %workspace_session_id, error = %error, "workspace request failed");
                    false
                }
            };
            let _ = request.respond(approved);
        }
    });
    workspace_broker
}

fn build_tool_brokers(state: &Arc<DaemonState>, session_id: &SessionId) -> ToolBrokers {
    ToolBrokers {
        workspace: spawn_workspace_forwarder(state, session_id),
        ask_user: spawn_ask_user_forwarder(state, session_id),
        task: state.tool_dispatchers.task.clone(),
        notify: state.tool_dispatchers.notify.clone(),
        browser: state.tool_dispatchers.browser.clone(),
        register_skills: state.tool_dispatchers.register_skills.clone(),
        tunnel: state.tool_dispatchers.tunnel.clone(),
    }
}

/// Agent-to-agent messaging as the daemon's sessions see it: `send` stores
/// in the shared queue and `wake` dispatches a follow-up run in-process.
fn agent_messaging(state: &Arc<DaemonState>) -> crate::agent_link::AgentMessaging {
    crate::agent_link::AgentMessaging {
        directory: crate::agent_link::AgentDirectory::new(
            state.runtime_service.clone(),
            state.may_dir.clone(),
            {
                let state = state.clone();
                move |session_id: &str| spawn_follow_up_runs(state.clone(), session_id.to_string())
            },
        ),
        runtime_service: state.runtime_service.clone(),
    }
}

/// One attempt of a turn: the provider candidate it builds on, and the
/// history row boundary it rebuilds context up to when it resubmits the
/// prompt (`None` replays the committed transcript in full).
struct TurnAttempt {
    provider_attempt: usize,
    turn_boundary: Option<i32>,
    /// The agent a retry on the same provider continues with: the one the
    /// failed attempt took, which holds what a resumed turn exists for.
    retained_agent: Option<MayAgent>,
}

/// Resolves the session context and builds the agent for one chat turn.
///
/// Built on the heap for the reason given on [`run_agent_loop`].
fn prepare_agent_for_request<'a>(
    state: &'a Arc<DaemonState>,
    req: &'a ChatRequest,
    fence: &'a RunFence,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    attempt: TurnAttempt,
    kind: &'a TurnKind,
) -> Pin<Box<impl Future<Output = Result<PreparedAgent, UiChatPreparationError>> + 'a>> {
    Box::pin(async move {
        let ctx = Box::pin(resolve_session_context(
            state,
            req,
            fence,
            persist_user_message,
            use_persisted_context_snapshot,
            attempt.turn_boundary,
        ))
        .await?;
        let config = state.config_snapshot();
        let workspace_config = {
            let path =
                crate::session::SessionPaths::new(ctx.sandbox_dir.clone()).workspace_config();
            blocking::unblock(move || crate::workspace::WorkspaceConfig::load(&path))
                .await
                .map_err(|error| UiChatPreparationError::BuildAgent(error.into()))?
        };
        let build_key = agent_build_key(&config, &workspace_config, &ctx);
        // The session's idle agent continues the conversation when it was
        // built from the same inputs; a resumed turn exists only for what
        // that agent collected, so it takes the agent whatever changed.
        let reused = if let Some(agent) = attempt.retained_agent {
            Some(agent)
        } else if attempt.provider_attempt == 0 {
            let fits = (!kind.is_resume()).then_some(build_key);
            state
                .parked_agents
                .take(ctx.session_id.as_str(), fits)
                .await
        } else {
            None
        };
        let mut agent = match reused {
            Some(agent) => agent,
            None if kind.is_resume() => return Err(UiChatPreparationError::NothingToResume),
            None => {
                Box::pin(build_session_agent(
                    state,
                    &config,
                    &ctx,
                    fence,
                    attempt.provider_attempt,
                ))
                .await?
            }
        };

        let provider_id = agent.provider_id().to_string();

        let (prompt, attachments) = if kind.is_resume() {
            (String::new(), Vec::new())
        } else {
            let (prompt, attachments) =
                Box::pin(build_prompt_and_attachments(state, req, &ctx.current_model))
                    .await
                    .map_err(UiChatPreparationError::BuildPrompt)?;
            let preview_messages = agent
                .preview_request_messages_with_hidden_context(
                    &prompt,
                    ctx.hidden_context_xml.as_deref(),
                    attachments.iter().cloned(),
                )
                .await
                .map_err(UiChatPreparationError::PreviewRequest)?;
            persist_context_debug_snapshot(
                state,
                &ctx.session_id,
                &ctx.current_model,
                &provider_id,
                &prompt,
                &attachments,
                &preview_messages,
            )
            .await
            .map_err(UiChatPreparationError::PersistContextDebugSnapshot)?;
            (prompt, attachments)
        };

        Ok(PreparedAgent {
            session_id: ctx.session_id,
            agent,
            current_model: ctx.current_model,
            provider_id,
            profile_id: ctx.profile_id,
            hidden_context_xml: ctx.hidden_context_xml,
            prompt,
            attachments,
            provider_candidate_count: ctx.provider_candidate_count,
            build_key,
            user_message_id: ctx.user_message_id,
            reasoning_effort: ctx.reasoning_effort,
        })
    })
}

/// Builds the session's agent for `ctx` and restores its conversation, for a
/// turn that has no idle agent to continue with.
fn build_session_agent<'a>(
    state: &'a Arc<DaemonState>,
    config: &'a crate::config::MayConfig,
    ctx: &'a SessionContext,
    fence: &'a RunFence,
    provider_attempt: usize,
) -> Pin<Box<impl Future<Output = Result<MayAgent, UiChatPreparationError>> + 'a>> {
    Box::pin(async move {
        let (environment, location) = Box::pin(setup_agent_environment(state, ctx)).await?;
        let brokers = build_tool_brokers(state, &ctx.session_id);

        let mut agent = MayAgent::from_config_with_model_and_permissions(
            config,
            crate::agent::MayAgentSessionInput {
                sandbox_dir: ctx.sandbox_dir.clone(),
                may_dir: state.may_dir.clone(),
                cwd: ctx.cwd.clone(),
                session_id: ctx.session_id.clone(),
                workspace_request_broker: brokers.workspace,
                ask_user_broker: brokers.ask_user,
                db: state.db(),
                model: &ctx.current_model,
                reasoning_effort: ctx.reasoning_effort,
                profile_id: ctx.profile_id.clone(),
                permission_approver: Some({
                    let approver = state
                        .permission_approver
                        .for_session(ctx.session_id.as_str().to_string());
                    if ctx.non_interactive {
                        approver.without_interactive_approval("restricted oneshot mode")
                    } else {
                        approver
                    }
                }),
                live_config: Some(state.config.clone()),
                agent_messaging: Some(agent_messaging(state)),
                profile_persona: ctx.profile_persona.clone(),
                profile_memories: ctx.profile_memories.clone(),
                plan_state: ctx.plan_state,
                location,
                task_broker: Some(brokers.task),
                notify_broker: Some(brokers.notify),
                tunnel_broker: Some(brokers.tunnel),
                browser_broker: state.browser_manager().map(|_| brokers.browser),
                acp_host: Some(state.acp_host.clone()),
                #[cfg(target_os = "macos")]
                computer: (!matches!(
                    environment,
                    crate::agent::AgentEnvironment::Container { .. }
                ))
                .then(|| crate::agent::computer_tool::ComputerHandles {
                    agent: state.computer.clone(),
                }),
                register_skills_broker: Some(brokers.register_skills),
                environment,
                tool_distribution: state.tool_distribution.clone(),
                tool_environments: state.tool_environments.clone(),
                provider_attempt,
                prompt_cache_epoch: ctx.context_snapshot.as_ref().map_or(0, |snapshot| {
                    snapshot
                        .checkpoint
                        .context_window
                        .metrics
                        .prompt_cache_epoch
                }),
                subagents: Some(state.session_background.subagents(ctx.session_id.as_str())),
                background_jobs: aither_agent::BackgroundJobPolicy::Detach,
                connectivity: state.connectivity.clone(),
                rules_import: ctx.rules_import.clone(),
            },
        )
        .await
        .map_err(UiChatPreparationError::BuildAgent)?;
        state
            .runtime_service
            .set_terminal_grants(ctx.session_id.as_str(), agent.terminal_grants());
        state
            .session_background
            .add_job_registry(ctx.session_id.as_str(), agent.job_registry());

        if let Some(snapshot) = ctx.context_snapshot.as_ref() {
            // Restore the checkpoint, then append only transcript entries
            // committed after it — never UI history, so the checkpoint's own
            // answer cannot appear twice.
            agent
                .restore_checkpoint(snapshot.checkpoint.clone())
                .map_err(UiChatPreparationError::RestoreCheckpoint)?;
            for (_, message) in ctx
                .transcript
                .iter()
                .filter(|(seq, _)| *seq > snapshot.applied_seq)
            {
                agent.push_message(message.clone());
            }
        } else if ctx.transcript.is_empty() && !ctx.seed_messages.is_empty() {
            // First-ever transcript: the agent assigns seqs to the seeded
            // messages; persist its checkpoint so the store only ever holds
            // aither-assigned seqs.
            for message in &ctx.seed_messages {
                agent.push_message(message.clone());
            }
            crate::session::persist_agent_context_snapshot(&state.db(), fence, &mut agent)
                .await
                .map_err(|error| {
                    UiChatPreparationError::LoadContextSnapshot(crate::db::ChatDbError::Storage(
                        format!("failed to persist seeded context: {error}"),
                    ))
                })?;
        } else {
            for (_, message) in &ctx.transcript {
                agent.push_message(message.clone());
            }
        }

        Ok(agent)
    })
}

/// A fingerprint of everything `ctx`'s agent is built from, so a turn can
/// tell whether the session's idle agent still fits: the configuration, the
/// model and its reasoning effort, the profile, its persona and memories,
/// the plan state, where the session works and the workspace configuration
/// (trust, workdirs) its grants, mounts, clone and PR contexts come from.
fn agent_build_key(
    config: &crate::config::MayConfig,
    workspace: &crate::workspace::WorkspaceConfig,
    ctx: &SessionContext,
) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::hash::DefaultHasher::new();
    serde_json::to_string(config)
        .unwrap_or_default()
        .hash(&mut hasher);
    toml::to_string(workspace)
        .expect("a parsed workspace configuration serializes")
        .hash(&mut hasher);
    ctx.current_model.hash(&mut hasher);
    format!("{:?}", ctx.reasoning_effort).hash(&mut hasher);
    ctx.profile_id.hash(&mut hasher);
    format!("{:?}", ctx.profile_model).hash(&mut hasher);
    format!("{:?}", ctx.profile_environment).hash(&mut hasher);
    ctx.profile_persona.hash(&mut hasher);
    format!("{:?}", ctx.profile_memories).hash(&mut hasher);
    format!("{:?}", ctx.plan_state).hash(&mut hasher);
    ctx.cwd.hash(&mut hasher);
    ctx.non_interactive.hash(&mut hasher);
    ctx.rules_import.hash(&mut hasher);
    hasher.finish()
}

/// What a turn starts from.
#[derive(Debug, Default)]
pub enum TurnKind {
    /// A user message, or a queued one.
    #[default]
    Prompt,
    /// The session's idle agent collected background commands that need the
    /// model; their events open the turn.
    Resume(Vec<aither_agent::TimedEvent>),
}

impl TurnKind {
    const fn is_resume(&self) -> bool {
        matches!(self, Self::Resume(_))
    }
}

/// Hold the session's `tasks.md` to the one-owner-per-open-issue rule: a
/// change that adds an issue another agent owns is reverted, and the session
/// is told through a steer message.
async fn enforce_issue_ownership(
    state: &Arc<DaemonState>,
    session_id: &SessionId,
    accepted: &mut Option<String>,
) {
    use crate::issue::owner::{IssueOwners, OwnershipError};

    let directory = agent_messaging(state).directory;
    match IssueOwners::new(directory.clone())
        .admit(session_id.as_str(), accepted)
        .await
    {
        Ok(()) => {}
        Err(OwnershipError::Owned(owned)) => {
            if let Err(error) = directory.deliver(session_id.as_str(), owned.notice()).await {
                tracing::warn!(%session_id, %error, "failed to tell the session its tasks.md change was reverted");
            }
        }
        Err(error) => {
            tracing::warn!(%session_id, %error, "issue ownership of tasks.md could not be checked");
        }
    }
}

/// Read tasks.md and return the parsed checklist if its content changed since the last check.
/// The sink is responsible for wrapping it in the SSE envelope with `seq/turn_id`.
async fn check_tasks_update(
    paths: &SessionPaths,
    last_hash: &mut Option<u64>,
) -> Option<crate::todo_parser::TodoChecklist> {
    use std::hash::{Hash, Hasher};

    let content = match paths.tasks_content().await {
        Ok(Some(content)) => content,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(path = %paths.tasks_path().display(), error = %error, "failed to read session tasks.md");
            return None;
        }
    };
    let mut hasher = std::hash::DefaultHasher::new();
    content.hash(&mut hasher);
    let hash = hasher.finish();

    if *last_hash == Some(hash) {
        return None;
    }
    *last_hash = Some(hash);

    let checklist = crate::todo_parser::TodoChecklist::parse(&content);
    if checklist.is_empty() {
        return None;
    }
    Some(checklist)
}

async fn is_takeover_active_for_session(state: &DaemonState, session_id: &SessionId) -> bool {
    match state
        .runtime_service
        .has_browser_takeover(session_id.as_str())
        .await
    {
        Ok(active) => active,
        Err(error) => {
            tracing::warn!(session_id = %session_id, error = %error, "failed to inspect browser takeover state");
            false
        }
    }
}

async fn take_over_alert_pending(state: &DaemonState, session_id: &SessionId) -> bool {
    match state
        .runtime_service
        .take_browser_takeover_alert_pending(session_id.as_str())
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(session_id = %session_id, error = %error, "failed to consume browser takeover alert state");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use skyzen::{HttpError, StatusCode};
    use std::collections::HashMap;
    use std::future::Future;

    use super::{
        ApiErrorCode, ChatRequest, DaemonState, TurnKind, build_session_agent,
        resolve_session_context, run_agent_loop,
    };
    use crate::config::MayConfig;
    use aither_core::llm::ToolResult;

    use super::{ResumeRequestKind, ResumeResolution};

    #[test]
    fn tool_turn_complete_does_not_close_the_next_reasoning_phase() {
        assert!(!super::closes_reasoning_phase(
            &aither_agent::AgentEvent::turn_complete(1, true)
        ));
        assert!(super::closes_reasoning_phase(
            &aither_agent::AgentEvent::turn_complete(1, false)
        ));
        assert!(super::closes_reasoning_phase(
            &aither_agent::AgentEvent::text("visible")
        ));
        assert!(super::closes_reasoning_phase(
            &aither_agent::AgentEvent::tool_start("id", "terminal", "{}")
        ));
        assert!(!super::closes_reasoning_phase(
            &aither_agent::AgentEvent::reasoning("think")
        ));
    }

    #[test]
    fn browser_takeover_alert_tells_the_model_to_recheck_the_page() {
        assert_eq!(
            super::browser_takeover_alert(),
            "<system-reminder>\nThe user manually controlled the browser and just returned control. Re-check the current page state before continuing automation.\n</system-reminder>"
        );
    }

    #[test]
    fn resume_reminder_states_what_was_awaited_and_how_it_resolved() {
        let reminder = super::resume_waiting_turn_reminder(
            ResumeRequestKind::Permission,
            ResumeResolution::Denied,
        )
        .expect("reminder text is not empty");
        assert_eq!(
            reminder.as_ref(),
            "<system-reminder>\nThe previous run was interrupted while waiting for a permission decision. The user denied it. Reassess the latest state, use the recorded response, and continue from the most sensible next action.\n</system-reminder>"
        );
    }

    #[test]
    fn run_claim_storage_failure_is_an_internal_error_not_a_conflict() {
        let refusal = super::acquire_refusal(crate::session::AcquireError::Storage(
            crate::db::ChatDbError::Storage("database is locked".to_string()),
        ))
        .into_boxed_http_error();
        assert_eq!(refusal.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            refusal.to_string().contains("database is locked"),
            "the refusal should carry the storage failure: {refusal}"
        );
    }

    #[test]
    fn run_claim_held_by_another_run_is_a_conflict() {
        let refusal = super::acquire_refusal(crate::session::AcquireError::Busy(
            crate::session::SessionBusy {
                session_id: "session".to_string(),
            },
        ))
        .into_boxed_http_error();
        assert_eq!(refusal.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn parse_persisted_context_snapshot_reads_applied_seq() {
        let snapshot = crate::session::parse_persisted_context_snapshot(
            crate::db::StoredSessionContextSnapshot {
                checkpoint: aither_agent::AgentCheckpoint {
                    context: aither_agent::ContextCheckpoint::new(None, Vec::new()),
                    tool_surface_hash: "abc123".to_string(),
                    context_window: aither_agent::ContextWindowSnapshot {
                        phase: aither_agent::ContextWindowPhase::Stable,
                        metrics: aither_agent::ContextWindowMetrics {
                            usage_fraction: 0.0,
                            context_window: 0,
                            estimated_tokens: 0,
                            chars_per_token: 4.0,
                            system_block_count: 0,
                            recent_message_count: 0,
                            recent_system_message_count: 0,
                            has_handoff: false,
                            has_staged_system_updates: false,
                            prompt_cache_epoch: 0,
                        },
                        messages: Vec::new(),
                    },
                    has_background_tasks: false,
                },
                applied_seq: 42,
                updated_at: "2026-03-14T00:00:00Z".to_string(),
            },
        );

        assert_eq!(snapshot.applied_seq, 42);
        assert!(snapshot.checkpoint.context.is_empty());
    }

    /// aither emits `ToolCallDelta` with `arguments_fragment` as a true
    /// increment. `normalize_tool_delta_for_sink` forwards them verbatim so the
    /// frontend can concatenate them into valid JSON. A regression here would
    /// drop the opening `{"` and break every tool summary.
    #[test]
    fn tool_deltas_emit_incremental_fragments_verbatim() {
        let mut pending = HashMap::new();

        // OutputItemAdded fires an initial empty delta — becomes Start(args="").
        let e1 = super::normalize_tool_delta_for_sink(&mut pending, "tool-1", "terminal", "");
        assert_eq!(
            e1,
            super::SinkToolEvent::Start {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments: String::new(),
            }
        );

        // Each delta is already an increment and passes through verbatim.
        let e2 = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"description",
        );
        assert_eq!(
            e2,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "{\"description".to_string(),
            }
        );

        let e3 =
            super::normalize_tool_delta_for_sink(&mut pending, "tool-1", "terminal", "\":\"Get");
        assert_eq!(
            e3,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "\":\"Get".to_string(),
            }
        );
    }

    #[test]
    fn first_tool_delta_emits_synthetic_start_then_later_deltas_stream_normally() {
        let mut pending = HashMap::new();

        let first = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date",
        );
        assert_eq!(
            first,
            super::SinkToolEvent::Start {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments: "{\"command\":\"date".to_string(),
            }
        );

        let second = super::normalize_tool_delta_for_sink(&mut pending, "tool-1", "", "\"}");
        assert_eq!(
            second,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "\"}".to_string(),
            }
        );
    }

    #[test]
    fn arguments_only_tool_delta_emits_placeholder_start_immediately() {
        let mut pending = HashMap::new();

        let first =
            super::normalize_tool_delta_for_sink(&mut pending, "tool-1", "", "{\"command\":\"date");
        assert_eq!(
            first,
            super::SinkToolEvent::Start {
                id: "tool-1".to_string(),
                name: String::new(),
                arguments: "{\"command\":\"date".to_string(),
            }
        );

        let named = super::normalize_tool_delta_for_sink(&mut pending, "tool-1", "terminal", "\"}");
        assert_eq!(
            named,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "\"}".to_string(),
            }
        );
    }

    #[test]
    fn canonical_tool_start_updates_placeholder_name_after_synthetic_start() {
        let mut pending = HashMap::new();
        let first = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "",
            "{\"command\":\"date\"}",
        );
        assert!(matches!(first, super::SinkToolEvent::Start { .. }));

        let canonical = super::normalize_tool_start_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date\"}",
        );
        assert_eq!(
            canonical,
            Some(super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: String::new(),
            })
        );
    }

    #[test]
    fn tool_delta_fragments_pass_through_verbatim() {
        let mut pending = HashMap::new();

        let first = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"",
        );
        assert!(matches!(first, super::SinkToolEvent::Start { .. }));

        let second =
            super::normalize_tool_delta_for_sink(&mut pending, "tool-1", "terminal", "date\"}");
        assert_eq!(
            second,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "date\"}".to_string(),
            }
        );

        // A fragment that happens to equal the buffer so far is still an
        // increment and must not be swallowed.
        let mut pending = HashMap::new();
        super::normalize_tool_delta_for_sink(&mut pending, "t", "x", "ab");
        let dup = super::normalize_tool_delta_for_sink(&mut pending, "t", "x", "ab");
        assert_eq!(
            dup,
            super::SinkToolEvent::Delta {
                id: "t".to_string(),
                name: "x".to_string(),
                arguments_fragment: "ab".to_string(),
            }
        );
    }

    #[test]
    fn canonical_tool_start_is_suppressed_after_synthetic_start() {
        let mut pending = HashMap::new();
        let first = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date\"}",
        );
        assert!(matches!(first, super::SinkToolEvent::Start { .. }));

        let canonical = super::normalize_tool_start_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date\"}",
        );
        assert_eq!(canonical, None);
    }

    #[test]
    fn canonical_tool_start_emits_when_no_delta_arrived_first() {
        let mut pending = HashMap::new();

        let start = super::normalize_tool_start_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date\"}",
        );
        assert_eq!(
            start,
            Some(super::SinkToolEvent::Start {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments: "{\"command\":\"date\"}".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn check_tasks_update_reads_tasks_file_and_dedupes_unchanged_content() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let paths = crate::session::SessionPaths::new(dir.path().to_path_buf());
        let tasks_path = paths.tasks_path();
        let mut last_hash = None;

        // A missing file yields no update.
        assert!(
            super::check_tasks_update(&paths, &mut last_hash)
                .await
                .is_none()
        );

        // A model-side write produces the parsed checklist once.
        async_fs::write(&tasks_path, "- [x] done\n- [ ] pending\n")
            .await
            .expect("tasks.md should be written");
        let checklist = super::check_tasks_update(&paths, &mut last_hash)
            .await
            .expect("a written tasks.md should produce a checklist");
        assert_eq!(checklist.total(), 2);
        assert_eq!(checklist.completed(), 1);

        // The same content is deduplicated by hash.
        assert!(
            super::check_tasks_update(&paths, &mut last_hash)
                .await
                .is_none()
        );

        // A further model-side write produces an updated checklist.
        async_fs::write(&tasks_path, "- [ ] new task\n")
            .await
            .expect("tasks.md should be rewritten");
        let checklist = super::check_tasks_update(&paths, &mut last_hash)
            .await
            .expect("a changed tasks.md should produce a new checklist");
        assert_eq!(checklist.total(), 1);
        assert_eq!(checklist.items[0].text, "new task");

        // A document without checklist items produces no update.
        async_fs::write(&tasks_path, "just prose\n")
            .await
            .expect("tasks.md should be rewritten");
        assert!(
            super::check_tasks_update(&paths, &mut last_hash)
                .await
                .is_none()
        );
    }

    /// What a run told its clients about waiting and discarding text.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Note {
        Hold(crate::client::HoldReason),
        Retract,
    }

    /// Reports holds and retractions on `notes` when given one, and never
    /// disconnects.
    #[derive(Default)]
    struct TestSink {
        notes: Option<async_channel::Sender<Note>>,
        /// How many times the run showed the text `WAKE_TEXT` as an agent event.
        wake_texts: usize,
    }

    /// The text of the background event a test resume turn opens with.
    const WAKE_TEXT: &str = "background command finished";

    impl TestSink {
        fn noting() -> (Self, async_channel::Receiver<Note>) {
            let (tx, rx) = async_channel::unbounded();
            (
                Self {
                    notes: Some(tx),
                    ..Self::default()
                },
                rx,
            )
        }

        fn note(&self, note: Note) {
            if let Some(notes) = &self.notes {
                notes.try_send(note).expect("the notes receiver is alive");
            }
        }
    }

    impl super::ChatSink for TestSink {
        fn on_agent_event(
            &mut self,
            event: Result<&aither_agent::AgentEvent, &aither_agent::AgentError>,
        ) {
            if matches!(event, Ok(aither_agent::AgentEvent::Text(text)) if text == WAKE_TEXT) {
                self.wake_texts += 1;
            }
        }
        fn on_start(&mut self, _s: &str, _a: &str) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_text(&mut self, _c: &str) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_reasoning(&mut self, _c: &str) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_tool_delta(
            &mut self,
            _i: &str,
            _n: &str,
            _f: &str,
            _s: crate::daemon::tool_summary::ToolSummary,
        ) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_tool_start(
            &mut self,
            _i: &str,
            _n: &str,
            _a: &str,
            _s: crate::daemon::tool_summary::ToolSummary,
        ) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_tool_end(
            &mut self,
            _i: &str,
            _n: &str,
            _a: Option<&str>,
            _r: &ToolResult,
            _s: crate::daemon::tool_summary::ToolSummary,
        ) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_todo_update(
            &mut self,
            _c: &crate::todo_parser::TodoChecklist,
        ) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_speed(&mut self, _t: f64) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
        fn on_hold(
            &mut self,
            reason: crate::client::HoldReason,
            _m: &str,
        ) -> impl Future<Output = bool> + Send {
            self.note(Note::Hold(reason));
            std::future::ready(true)
        }
        fn on_retract(&mut self) -> impl Future<Output = bool> + Send {
            self.note(Note::Retract);
            std::future::ready(true)
        }
        fn on_error(
            &mut self,
            _c: ApiErrorCode,
            _m: &str,
            _r: bool,
        ) -> impl Future<Output = bool> + Send {
            std::future::ready(true)
        }
    }

    /// A scripted OpenAI-compatible stub: answers every request with the
    /// queued responses and records the request bodies it receives.
    struct StubProvider {
        url: String,
        bodies: async_channel::Receiver<String>,
    }

    fn sse_chunk(chunk: &str) -> String {
        format!("data: {chunk}\n\n")
    }

    fn sse_text(text: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": text}}]
        });
        let done = serde_json::json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        });
        format!(
            "{}{}{}",
            sse_chunk(&chunk.to_string()),
            sse_chunk(&done.to_string()),
            sse_chunk("[DONE]")
        )
    }

    fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}}]
        });
        let done = serde_json::json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        });
        format!(
            "{}{}{}",
            sse_chunk(&chunk.to_string()),
            sse_chunk(&done.to_string()),
            sse_chunk("[DONE]")
        )
    }

    /// Spawns the stub; `responses` are consumed per /chat/completions POST,
    /// the last one repeating. Request bodies arrive on `bodies`.
    /// Requests the aither `OpenAI` client sends for one failing call: the first
    /// plus its three own retries.
    const OPENAI_CLIENT_ATTEMPTS: usize = 4;

    /// The `n`-th request body `stub` received (1-based).
    async fn last_body(stub: &StubProvider, n: usize) -> String {
        let mut body = String::new();
        for _ in 0..n {
            body = tokio::time::timeout(std::time::Duration::from_secs(60), stub.bodies.recv())
                .await
                .expect("the provider received a request")
                .expect("request body");
        }
        body
    }

    /// Waits until `session_id`'s turn has ended and the session takes a new
    /// one.
    async fn wait_until_idle(state: &DaemonState, session_id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let runtime = state
                    .runtime_service
                    .get(session_id)
                    .await
                    .expect("runtime");
                if runtime.admits_new_request() && state.parked_agents.contains(session_id) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the turn ends");
    }

    /// The next request body of `stub` that carries `marker` (lowercase):
    /// auxiliary requests of other turns are skipped.
    async fn body_containing(stub: &StubProvider, marker: &str) -> String {
        loop {
            let body = last_body(stub, 1).await.to_lowercase();
            if body.contains(marker) {
                return body;
            }
        }
    }

    async fn stub_provider(responses: Vec<String>) -> StubProvider {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        let (body_tx, bodies) = async_channel::unbounded::<String>();
        let responses = std::sync::Arc::new(std::sync::Mutex::new(responses));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body_tx = body_tx.clone();
                let responses = responses.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    // Read until the headers are complete, then until the
                    // body is complete (Content-Length or chunked).
                    let body = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break Vec::new();
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(head_end) =
                            buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
                        else {
                            continue;
                        };
                        let headers = String::from_utf8_lossy(&buf[..head_end]);
                        if headers.starts_with("GET /v1/models") {
                            let _ = socket
                                .write_all(
                                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                )
                                .await;
                            return;
                        }
                        if headers.contains("transfer-encoding: chunked") {
                            // Complete once the terminal 0-length chunk is in.
                            if buf[head_end..].windows(5).any(|w| w == b"0\r\n\r\n") {
                                let raw = &buf[head_end..];
                                let mut body = Vec::new();
                                let mut rest = raw;
                                while let Some(size_end) =
                                    rest.windows(2).position(|w| w == b"\r\n")
                                {
                                    let size = usize::from_str_radix(
                                        std::str::from_utf8(&rest[..size_end])
                                            .unwrap_or("0")
                                            .trim(),
                                        16,
                                    )
                                    .unwrap_or(0);
                                    if size == 0 {
                                        break;
                                    }
                                    let start = size_end + 2;
                                    body.extend_from_slice(&rest[start..start + size]);
                                    rest = &rest[start + size + 2..];
                                }
                                break body;
                            }
                            continue;
                        }
                        let length = headers
                            .split("\r\n")
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if buf.len() >= head_end + length {
                            break buf[head_end..head_end + length].to_vec();
                        }
                    };
                    if !body.is_empty() {
                        let _ = body_tx.try_send(String::from_utf8_lossy(&body).to_string());
                    }
                    let response = {
                        let mut queued = responses.lock().expect("stub queue");
                        if queued.len() > 1 {
                            queued.remove(0)
                        } else {
                            queued[0].clone()
                        }
                    };
                    // The connection dies before any response.
                    if response == "drop" {
                        return;
                    }
                    // The response starts streaming and then goes silent,
                    // like a socket that sleep killed.
                    if let Some(partial) = response.strip_prefix("stall:") {
                        let wire = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{partial}",
                            partial.len() + 1_000_000
                        );
                        let _ = socket.write_all(wire.as_bytes()).await;
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                        return;
                    }
                    let (status, body_out) = match response.as_str() {
                        "500" => ("HTTP/1.1 500 Internal Server Error", String::new()),
                        body => ("HTTP/1.1 200 OK", body.to_string()),
                    };
                    let wire = format!(
                        "{status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_out}",
                        body_out.len()
                    );
                    let _ = socket.write_all(wire.as_bytes()).await;
                });
            }
        });
        StubProvider {
            url: format!("http://{addr}/v1"),
            bodies,
        }
    }

    /// A daemon state whose session model fails over from `first` to
    /// `second`, both scripted `custom` providers.
    async fn failover_state(
        may_dir: &std::path::Path,
        first: &StubProvider,
        second: &StubProvider,
    ) -> std::sync::Arc<DaemonState> {
        failover_state_on(
            aither_connectivity::Connectivity::manual(true).0,
            may_dir,
            first,
            second,
        )
        .await
    }

    /// [`failover_state`] on a given connectivity source.
    async fn failover_state_on(
        connectivity: aither_connectivity::Connectivity,
        may_dir: &std::path::Path,
        first: &StubProvider,
        second: &StubProvider,
    ) -> std::sync::Arc<DaemonState> {
        use crate::config::{ProviderInstance, ProviderType};
        let mut config = MayConfig::default();
        for (id, stub) in [("a", first), ("b", second)] {
            let mut instance = ProviderInstance::new(ProviderType::Custom);
            instance.api_key = Some("test-key".to_string());
            instance.base_url = Some(stub.url.clone());
            config.provider.insert(id.to_string(), instance);
        }
        let state = std::sync::Arc::new(
            DaemonState::for_test_with_connectivity(may_dir, config, connectivity)
                .await
                .expect("test daemon state"),
        );
        state
            .db()
            .create_session("session", Some("test"), Some("a::gpt-4o"))
            .await
            .expect("session");
        state
    }

    /// A first attempt whose provider fails retryably retries with the same
    /// context: the user message reaches the retry exactly once and history
    /// holds it once.
    #[tokio::test]
    async fn a_failed_first_attempt_retries_without_repeating_the_user_message() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        // One failure more than the OpenAI client retries on its own, so the
        // turn retries; the cache is warm, so it stays on provider A.
        let mut script = vec!["500".to_string(); OPENAI_CLIENT_ATTEMPTS];
        script.push(sse_text("done"));
        let provider_a = stub_provider(script).await;
        let provider_b = stub_provider(Vec::new()).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;

        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "hello".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let mut sink = TestSink::default();
        let result = run_agent_loop(
            &state,
            &req,
            &mut sink,
            &guard,
            true,
            true,
            &TurnKind::Prompt,
        )
        .await;
        assert!(result.is_ok(), "the retry must succeed: {result:?}");

        let body = last_body(&provider_a, OPENAI_CLIENT_ATTEMPTS + 1).await;
        let messages: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&body)
            .expect("request json")["messages"]
            .as_array()
            .expect("messages array")
            .clone();
        let user_messages = messages
            .iter()
            .filter(|message| {
                message["role"] == "user" && message["content"].to_string().contains("hello")
            })
            .count();
        assert_eq!(
            user_messages, 1,
            "the retry carries the user message exactly once"
        );

        let history = crate::session::load_history_from_db(&state.db(), "session")
            .await
            .expect("history");
        let stored = history
            .messages()
            .iter()
            .filter(|m| matches!(m.role, super::MessageRole::User) && m.content == "hello")
            .count();
        assert_eq!(stored, 1, "history holds the user message once");
    }

    /// A retryable failure after a tool executed never reruns it: the turn
    /// continues from the committed transcript.
    #[tokio::test]
    async fn a_failure_after_a_tool_call_continues_without_replaying_it() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let may_dir = tempfile::tempdir().expect("may dir");
        // Session-scoped `files/` is the writable side of the workspace.
        let session_files = may_dir.path().join("sessions/session/files");
        let marker = session_files.join("count");
        let command = format!(
            "mkdir -p {} && echo x >> {}",
            session_files.display(),
            marker.display()
        );
        let arguments = serde_json::json!({
            "script": command,
            "description": "writes a marker line",
            "timeout": 10
        })
        .to_string();
        let mut script = vec![sse_tool_call("call_1", "terminal", &arguments)];
        script.extend(vec!["500".to_string(); OPENAI_CLIENT_ATTEMPTS]);
        script.push(sse_text("done"));
        let provider_a = stub_provider(script).await;
        let provider_b = stub_provider(Vec::new()).await;
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;

        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "run it".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let mut sink = TestSink::default();
        let result = run_agent_loop(
            &state,
            &req,
            &mut sink,
            &guard,
            true,
            true,
            &TurnKind::Prompt,
        )
        .await;
        assert!(
            result.is_ok(),
            "the resume must finish the turn: {result:?}"
        );

        let runs = std::fs::read_to_string(&marker)
            .expect("the tool wrote a marker")
            .lines()
            .count();
        assert_eq!(runs, 1, "the tool executed exactly once");

        let history = crate::session::load_history_from_db(&state.db(), "session")
            .await
            .expect("history");
        assert!(
            history
                .messages()
                .iter()
                .any(|m| matches!(m.role, super::MessageRole::Tool)),
            "the partial turn was persisted"
        );
    }

    /// Queues `contents` as pending messages of "session".
    async fn queue_messages(state: &std::sync::Arc<DaemonState>, contents: &[&str]) {
        for content in contents {
            let req = ChatRequest {
                session_id: "session".to_string(),
                content: (*content).to_string(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                delivery_mode: crate::session::PendingUserMessageMode::Queue,
                pending_entry_id: None,
            };
            state
                .runtime_service
                .enqueue(
                    "session",
                    super::pending_user_message_from_request(req),
                    crate::session::PendingUserMessageMode::Queue,
                )
                .await
                .expect("enqueue");
        }
    }

    /// Every turn clears the idle-recap timer when it starts, whatever path
    /// started it.
    #[tokio::test]
    async fn a_turn_unarms_the_recap_timer_when_it_starts() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider_a = stub_provider(vec![sse_text("one"), sse_text("two")]).await;
        let provider_b = stub_provider(Vec::new()).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;

        run_prompt_turn(&state, &mut TestSink::default())
            .await
            .expect("first turn");
        super::recap::arm(&state, "session");
        assert!(state.recap_schedules.is_armed("session"), "idle session");

        run_prompt_turn(&state, &mut TestSink::default())
            .await
            .expect("second turn");
        assert!(!state.recap_schedules.is_armed("session"));
    }

    /// A queue of two turns draining leaves the session idle with its recap
    /// timer armed, as a published turn does.
    #[tokio::test]
    async fn a_drained_queue_arms_the_recap_timer() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider_a = stub_provider(vec![sse_text("one"), sse_text("two")]).await;
        let provider_b = stub_provider(Vec::new()).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;
        queue_messages(&state, &["first", "second"]).await;

        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        super::dispatch_follow_ups(state.clone(), guard).await;

        assert_eq!(stored_assistant_texts(&state).await, ["one", "two"]);
        assert!(
            state.recap_schedules.is_armed("session"),
            "the drained queue left the session idle"
        );
    }

    /// A message enqueued on an idle session starts a turn that ends with the
    /// session idle and its recap timer armed.
    #[tokio::test]
    async fn an_enqueue_on_an_idle_session_arms_the_recap_timer() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider_a = stub_provider(vec![sse_text("answer")]).await;
        let provider_b = stub_provider(Vec::new()).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;

        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "hello".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::Queue,
            pending_entry_id: None,
        };
        super::enqueue_chat(
            skyzen::utils::Json(req),
            skyzen::utils::State(state.clone()),
        )
        .await
        .expect("enqueue");
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !state.recap_schedules.is_armed("session") {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the enqueued turn leaves the session idle with its timer armed");
        assert_eq!(stored_assistant_texts(&state).await, ["answer"]);
    }

    /// A resumed turn whose provider call fails and is retried on the same
    /// provider continues with the agent it took, and clients see its
    /// background events once.
    #[tokio::test]
    async fn a_resume_turn_retries_with_the_agent_it_took() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let mut script = vec![sse_text("first")];
        script.extend(vec!["500".to_string(); OPENAI_CLIENT_ATTEMPTS]);
        script.push(sse_text("resumed"));
        let provider_a = stub_provider(script).await;
        let provider_b = stub_provider(Vec::new()).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;

        // A first turn leaves the session's idle agent behind.
        let mut sink = TestSink::default();
        run_prompt_turn(&state, &mut sink)
            .await
            .expect("the first turn completes");
        assert!(state.parked_agents.contains("session"));

        let req = ChatRequest {
            session_id: "session".to_string(),
            content: String::new(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let wake = vec![aither_agent::TimedEvent {
            at: jiff::Timestamp::now(),
            event: aither_agent::AgentEvent::Text(WAKE_TEXT.to_string()),
        }];
        let mut sink = TestSink::default();
        let result = run_agent_loop(
            &state,
            &req,
            &mut sink,
            &guard,
            false,
            true,
            &TurnKind::Resume(wake),
        )
        .await;
        let result = result.expect("the retried resume completes");
        assert_eq!(result.full_text, "resumed");
        assert_eq!(sink.wake_texts, 1, "the background events are shown once");
    }

    /// A background command that finishes after the turn parked its agent but
    /// before the turn's run released the session still resumes the session
    /// once the release happens.
    #[tokio::test]
    async fn a_job_finishing_before_the_release_resumes_the_session_after_it() {
        // The test's two waits on the command's side. A command's `timeout`
        // runs from its start, so it spans the turn and then the wait for
        // `done`; the command is bounded by their sum and outlives the test
        // however slow the runner is, because a test that waits longer has
        // already failed.
        const TURN_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
        const DONE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let may_dir = tempfile::tempdir().expect("may dir");
        // The session's `files/` is what the sandboxed command can write and
        // read.
        let session_files = may_dir.path().join("sessions/session/files");
        std::fs::create_dir_all(&session_files).expect("session files");
        let gate = session_files.join("go");
        let done = session_files.join("done");
        let arguments = serde_json::json!({
            "script": format!(
                "while [ ! -e {} ]; do sleep 0.05; done; echo finished; touch {}",
                gate.display(),
                done.display()
            ),
            "description": "finishes after the turn ends",
            "background": true,
            "timeout": (TURN_WAIT + DONE_WAIT).as_secs()
        })
        .to_string();
        let provider_a = stub_provider(vec![
            sse_tool_call("call_1", "terminal", &arguments),
            sse_text("started"),
            sse_text("resumed"),
        ])
        .await;
        let provider_b = stub_provider(Vec::new()).await;
        let state = failover_state(may_dir.path(), &provider_a, &provider_b).await;

        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "start it".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        // The run keeps the session while its agent is already parked.
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let mut sink = TestSink::default();
        tokio::time::timeout(
            TURN_WAIT,
            run_agent_loop(
                &state,
                &req,
                &mut sink,
                &guard,
                true,
                true,
                &TurnKind::Prompt,
            ),
        )
        .await
        .expect("the turn finishes within its wait")
        .expect("the turn completes");
        assert!(state.parked_agents.contains("session"));

        // The command ends only now, while the run still holds the session;
        // the idle agent's worker is observed through its job registry.
        // The registry cannot show it: the agent's own background wait
        // records completions, and that wait starts only after the release.
        // The command reports through a file instead.
        std::fs::write(&gate, "").expect("open the gate");
        tokio::time::timeout(DONE_WAIT, async {
            while !done.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the command finishes");
        // The process exits right after writing the file.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            provider_a.bodies.try_recv().is_ok() && provider_a.bodies.try_recv().is_ok(),
            "the turn made two requests"
        );
        assert!(
            provider_a.bodies.try_recv().is_err(),
            "nothing resumed the session while it was held"
        );

        guard
            .finish_idle(crate::session::RunOutcome::Completed)
            .await
            .expect("release the run");
        let body =
            tokio::time::timeout(std::time::Duration::from_secs(20), provider_a.bodies.recv())
                .await
                .expect("the release resumes the session")
                .expect("request body");
        assert!(body.contains("finished"), "the resume sees the result");
    }

    /// Lowering a project's trust in `workspace.toml` retires the session's
    /// idle agent: the next turn builds from the new level instead of
    /// continuing with the grants captured at build.
    #[tokio::test]
    async fn a_trust_change_retires_the_idle_agent() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![sse_text("first")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let state = failover_state(may_dir.path(), &provider, &provider).await;
        let checkout = cwd.path().canonicalize().expect("canonical checkout");
        state
            .db()
            .set_session_cwd("session", &checkout.to_string_lossy())
            .await
            .expect("session cwd");
        let config_path =
            crate::session::MayHomePaths::new(may_dir.path().to_path_buf()).workspace_config();
        let set_level = |level| {
            crate::trust::persist(&config_path, &checkout, None, level).expect("persist trust");
        };
        let writable_checkout = |agent: &crate::agent::MayAgent| {
            agent
                .terminal_grants()
                .snapshot()
                .writable
                .iter()
                .any(|path| path == &checkout)
        };

        // The first turn runs at the local level and leaves its agent idle.
        set_level(crate::trust::TrustLevel::Local);
        let mut sink = TestSink::default();
        run_prompt_turn(&state, &mut sink)
            .await
            .expect("the first turn completes");
        assert!(state.parked_agents.contains("session"));
        let grants = state
            .runtime_service
            .terminal_grants("session")
            .expect("the built agent's grants");
        assert!(
            grants.snapshot().writable.iter().any(|p| p == &checkout),
            "local trust makes the checkout writable"
        );

        // Trust lowered while the agent idles: the next turn's preparation
        // builds a fresh agent without the checkout grant.
        set_level(crate::trust::TrustLevel::LocalBranch);
        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "again".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let prepared = super::prepare_agent_for_request(
            &state,
            &req,
            guard.fence(),
            true,
            true,
            super::TurnAttempt {
                provider_attempt: 0,
                turn_boundary: None,
                retained_agent: None,
            },
            &TurnKind::Prompt,
        )
        .await
        .unwrap_or_else(|error| panic!("the next turn prepares: {error}"));
        assert!(
            !writable_checkout(&prepared.agent),
            "the rebuilt agent no longer writes the checkout"
        );
        assert!(
            !state.parked_agents.contains("session"),
            "the old idle agent was retired, not reused"
        );
    }

    /// The three things the sleep alert requires of the session `/sleep` ran in.
    fn assert_greeting_requirements(body: &str) {
        for required in [
            "wish them good night",
            "what you are working on now",
            "what you will keep pushing forward",
            "through ask_user now, all in one go",
            "need a few answers first",
        ] {
            assert!(body.contains(required), "the alert asks: {required}");
        }
    }

    /// `/sleep` on a session whose turn is running succeeds and is taken by
    /// the session's next request, carrying the same requirements.
    #[tokio::test]
    async fn sleep_on_a_busy_session_reaches_its_next_request() {
        use crate::daemon::sleep::{SleepRequest, start_sleep};
        use skyzen::utils::{Json, State};

        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![sse_text("Good night")]).await;
        let unused = stub_provider(vec![sse_text("unused")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider, &unused).await;

        // A turn holds the session.
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let runtime = state.runtime_service.get("session").await.expect("runtime");
        assert!(!runtime.admits_new_request(), "the session is busy");

        let Json(status) = start_sleep(
            Json(SleepRequest {
                session_id: "session".to_string(),
            }),
            State(state.clone()),
        )
        .await
        .expect("/sleep succeeds on a busy session");
        assert!(status.asleep);
        let runtime = state.runtime_service.get("session").await.expect("runtime");
        assert!(runtime.has_pending_messages(), "/sleep waits for the turn");

        // The running turn ends and the session takes the pending message.
        tokio::spawn(super::dispatch_follow_ups(state.clone(), guard));
        let body = body_containing(&provider, "/sleep").await;
        assert_greeting_requirements(&body);
    }

    /// `/sleep` starts a turn in the session it ran in, whose request carries
    /// the sleep alert with the three things the agent must do; another
    /// session is told only that the user sleeps, once, at its next turn,
    /// and the wake reaches both.
    #[tokio::test]
    async fn sleep_starts_a_turn_whose_alert_asks_for_the_good_night() {
        use crate::daemon::sleep::{SleepRequest, end_sleep, start_sleep};
        use crate::ui_event_bus::UiEventTopics;
        use skyzen::utils::{Json, State};

        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![sse_text("Good night")]).await;
        let unused = stub_provider(vec![sse_text("unused")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider, &unused).await;
        state
            .db()
            .create_session("other", Some("other"), Some("a::gpt-4o"))
            .await
            .expect("second session");
        let mut events = state.ui_event_bus.subscribe();

        let Json(status) = start_sleep(
            Json(SleepRequest {
                session_id: "session".to_string(),
            }),
            State(state.clone()),
        )
        .await
        .expect("/sleep starts");
        assert!(status.asleep);
        assert_eq!(status.origin.as_deref(), Some("session"));
        assert!(
            events
                .recv()
                .await
                .expect("a sleep event")
                .contains(UiEventTopics::SLEEP),
            "clients hear of the state on the event stream"
        );

        // The turn started by the command carries the alert.
        let body = last_body(&provider, 1).await.to_lowercase();
        assert_greeting_requirements(&body);
        assert!(body.contains("/sleep"), "the user's command opens the turn");

        // Another session hears of it at its next turn, without the greeting.
        super::start_prompt_turn(
            &state,
            ChatRequest {
                session_id: "other".to_string(),
                content: "hello".to_string(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                delivery_mode: crate::session::PendingUserMessageMode::default(),
                pending_entry_id: None,
            },
        )
        .await
        .expect("the other session's turn starts");
        let other = body_containing(&provider, "hello").await;
        assert!(other.contains("the user is going to sleep"));
        assert!(!other.contains("wish them good night"));

        // Nobody reads suggestions while the user sleeps.
        assert!(!super::suggest::suggestions_allowed(&state));

        // Waking (`/sleep off`) reaches the sessions that were told.
        let Json(status) = end_sleep(State(state.clone())).await.expect("/sleep off");
        assert!(!status.asleep);
        assert!(super::suggest::suggestions_allowed(&state));
        wait_until_idle(&state, "other").await;
        super::start_prompt_turn(
            &state,
            ChatRequest {
                session_id: "other".to_string(),
                content: "good morning".to_string(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                delivery_mode: crate::session::PendingUserMessageMode::default(),
                pending_entry_id: None,
            },
        )
        .await
        .expect("the next turn starts");
        let woke = body_containing(&provider, "good morning").await;
        assert!(woke.contains("the user is awake again"), "{woke}");

        // The health reminder is raised for the user and reaches no agent.
        let now = jiff::Timestamp::now();
        state.db().record_sleep(now).await.expect("sleep log");
        assert!(
            state
                .sleep
                .remind_if_due(now, &jiff::tz::TimeZone::UTC)
                .await
                .expect("reminder")
        );
        wait_until_idle(&state, "other").await;
        super::start_prompt_turn(
            &state,
            ChatRequest {
                session_id: "other".to_string(),
                content: "third".to_string(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                delivery_mode: crate::session::PendingUserMessageMode::default(),
                pending_entry_id: None,
            },
        )
        .await
        .expect("the third turn starts");
        let third = body_containing(&provider, "third").await;
        assert!(!third.contains("get some rest"));
        assert!(!third.contains("usually go to sleep"));
        assert!(!third.contains("time to rest"));
    }

    /// A queued entry that carries only hidden context (an agent message)
    /// is committed by the run that delivers it: the claim reset at daemon
    /// start does not deliver it again and the queue reports it gone.
    #[tokio::test]
    async fn a_delivered_hidden_context_entry_is_not_redelivered_after_restart() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![sse_text("unused")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider, &provider).await;
        let db = state.db();
        db.set_pending_immediate_message(
            "session",
            crate::session::PendingUserMessage {
                id: None,
                state: crate::session::PendingMessageState::default(),
                error: None,
                content: String::new(),
                attachments: Vec::new(),
                hidden_context_xml: Some(
                    crate::XmlBlock::parse("<agent_message>ping</agent_message>")
                        .expect("hidden context"),
                ),
                created_at: jiff::Timestamp::now().to_string(),
                delivery_mode: crate::session::PendingUserMessageMode::Queue,
            },
        )
        .await
        .expect("enqueue");
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let next = db
            .claim_next_pending_message(guard.fence(), "session")
            .await
            .expect("claim")
            .expect("the entry waits in the queue");
        let req = ChatRequest {
            session_id: "session".to_string(),
            content: next.content,
            attachments: Vec::new(),
            hidden_context_xml: next.hidden_context_xml,
            delivery_mode: next.delivery_mode,
            pending_entry_id: next.id,
        };
        super::prepare_agent_for_request(
            &state,
            &req,
            guard.fence(),
            true,
            true,
            super::TurnAttempt {
                provider_attempt: 0,
                turn_boundary: None,
                retained_agent: None,
            },
            &TurnKind::Prompt,
        )
        .await
        .unwrap_or_else(|error| panic!("the entry's turn prepares: {error}"));
        guard
            .finish_idle(crate::session::RunOutcome::Completed)
            .await
            .expect("release the run");

        assert!(
            state
                .runtime_service
                .get("session")
                .await
                .expect("runtime")
                .queued_count
                == 0,
            "a delivered entry leaves the queue"
        );
        db.recover_session_runtimes_after_restart()
            .await
            .expect("daemon start resets claims");
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        assert!(
            db.claim_next_pending_message(guard.fence(), "session")
                .await
                .expect("claim")
                .is_none(),
            "the delivered entry is not delivered again"
        );
    }

    /// Runs one prompt turn of "session" to completion on `sink`.
    async fn run_prompt_turn(
        state: &std::sync::Arc<DaemonState>,
        sink: &mut TestSink,
    ) -> Result<super::AgentRunResult, super::AgentLoopError> {
        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "hello".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let result = run_agent_loop(state, &req, sink, &guard, true, true, &TurnKind::Prompt).await;
        guard
            .finish_idle(crate::session::RunOutcome::Completed)
            .await
            .expect("release the run");
        result
    }

    /// The assistant answers the history holds, in order.
    async fn stored_assistant_texts(state: &DaemonState) -> Vec<String> {
        crate::session::load_history_from_db(&state.db(), "session")
            .await
            .expect("history")
            .messages()
            .iter()
            .filter(|m| matches!(m.role, super::MessageRole::Assistant))
            .map(|m| m.content.clone())
            .collect()
    }

    /// A network failure while the machine is offline neither fails over nor
    /// ends the turn: it announces a network hold, waits for the network, and
    /// completes on the same provider with nothing of the outage in history.
    #[tokio::test]
    async fn a_network_failure_offline_waits_for_the_network_on_the_same_provider() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let mut script = vec!["drop".to_string(); OPENAI_CLIENT_ATTEMPTS];
        script.push(sse_text("back online"));
        let provider_a = stub_provider(script).await;
        let provider_b = stub_provider(vec![sse_text("wrong provider")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let (connectivity, controller) = aither_connectivity::Connectivity::manual(false);
        let state = failover_state_on(connectivity, may_dir.path(), &provider_a, &provider_b).await;

        let (mut sink, notes) = TestSink::noting();
        // The hold comes only after the provider client's own backoff sleeps
        // and the turn's setup, both of which stretch with host load: the
        // test waits for the note itself and fails at once if the turn ends
        // without ever holding. The bound only catches a hang, far above the
        // seconds this takes on a loaded host.
        let turn = run_prompt_turn(&state, &mut sink);
        tokio::pin!(turn);
        let note = tokio::time::timeout(std::time::Duration::from_secs(300), async {
            tokio::select! {
                note = notes.recv() => note.expect("notes"),
                result = &mut turn => panic!("the turn ended without holding for the network: {result:?}"),
            }
        })
        .await
        .expect("the turn holds for the network within five minutes");
        assert_eq!(note, Note::Hold(crate::client::HoldReason::Network));
        // Holding means the turn is parked: nothing else happens until the
        // network returns.
        assert!(notes.try_recv().is_err());
        controller.set_online(true);
        let result = turn.await;
        assert!(result.is_ok(), "the turn must complete: {result:?}");

        assert_eq!(
            stored_assistant_texts(&state).await,
            vec!["back online".to_string()],
            "the outage leaves nothing in history"
        );
        assert!(
            provider_b.bodies.try_recv().is_err(),
            "the turn never failed over: every provider shares the network"
        );
    }

    /// A wake aborts the response in flight: its partial text is retracted
    /// and the prompt resubmits to the same provider.
    #[tokio::test]
    async fn a_wake_mid_stream_aborts_and_resubmits() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let partial = sse_chunk(
            &serde_json::json!({
                "choices": [{"index": 0, "delta": {"content": "half an ans"}}]
            })
            .to_string(),
        );
        let provider_a =
            stub_provider(vec![format!("stall:{partial}"), sse_text("whole answer")]).await;
        let provider_b = stub_provider(vec![sse_text("wrong provider")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let (connectivity, controller) = aither_connectivity::Connectivity::manual(true);
        let state = failover_state_on(connectivity, may_dir.path(), &provider_a, &provider_b).await;

        let (mut sink, notes) = TestSink::noting();
        let machine_wakes = async {
            // The request reached the provider, which then went silent.
            last_body(&provider_a, 1).await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            controller.sleep();
            controller.wake();
        };
        let (result, ()) = tokio::join!(run_prompt_turn(&state, &mut sink), machine_wakes);
        assert!(result.is_ok(), "the turn must complete: {result:?}");

        let mut seen = Vec::new();
        while let Ok(note) = notes.try_recv() {
            seen.push(note);
        }
        assert_eq!(seen.first(), Some(&Note::Retract), "notes: {seen:?}");
        assert!(
            matches!(
                seen.get(1),
                Some(Note::Hold(crate::client::HoldReason::Retry { .. }))
            ),
            "the resubmission waits its backoff: {seen:?}"
        );
        assert_eq!(
            stored_assistant_texts(&state).await,
            vec!["whole answer".to_string()],
            "the abandoned partial text never reaches history"
        );
        assert!(provider_b.bodies.try_recv().is_err(), "no failover");
    }

    /// A turn persisted through the checkpoint hook restores with its answer
    /// exactly once: the transcript's `applied_seq` boundary keeps whatever the
    /// checkpoint already covers from being pushed again.
    #[tokio::test]
    async fn a_restored_turn_contains_the_answer_exactly_once() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![sse_text("the answer")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider, &provider).await;
        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "hello".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let mut sink = TestSink::default();
        run_agent_loop(
            &state,
            &req,
            &mut sink,
            &guard,
            true,
            true,
            &TurnKind::Prompt,
        )
        .await
        .expect("the turn must succeed");

        // Rebuild the agent from storage exactly as the next turn would.
        let config = state.config_snapshot();
        let ctx = resolve_session_context(&state, &req, guard.fence(), false, true, None)
            .await
            .expect("context resolves");
        assert!(
            ctx.context_snapshot.is_some(),
            "the turn's checkpoint must have persisted"
        );
        let mut agent = build_session_agent(&state, &config, &ctx, guard.fence(), 0)
            .await
            .expect("agent builds from storage");
        let checkpoint = agent.export_checkpoint().await.expect("checkpoint");
        let answers = checkpoint
            .context
            .items()
            .iter()
            .filter(|item| format!("{:?}", item.message()).contains("the answer"))
            .count();
        assert_eq!(answers, 1, "the answer appears in context exactly once");
    }

    /// The ids a finished turn announces are the ids REST returns for the
    /// messages the turn stored.
    #[tokio::test]
    async fn a_turns_done_event_names_its_messages_as_rest_does() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![sse_text("答えは四十二。回答は42です。")]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider, &provider).await;
        let mut sink = TestSink::default();
        let result = run_prompt_turn(&state, &mut sink)
            .await
            .expect("the turn completes");

        let done = super::done_event_from_run_result(1, "turn".into(), "session".into(), result);
        let rows = state.db().get_messages("session").await.expect("rows load");
        let api = crate::daemon::message_mapping::map_db_messages(&rows).expect("rows map");
        let rest_user = api
            .iter()
            .find(|message| message.role == "user")
            .expect("the user message is stored");
        let rest_assistants: Vec<&str> = api
            .iter()
            .filter(|message| message.role == "assistant")
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(done.user_message_id.as_deref(), Some(rest_user.id.as_str()));
        assert_eq!(done.message_ids, rest_assistants);
        assert_eq!(done.assistant.id, rest_assistants[0]);
    }

    /// Seeding from UI history produces transcript seqs the agent itself
    /// assigned — the stored entries and the agent's own checkpoint agree.
    #[tokio::test]
    async fn seeded_transcript_seqs_are_the_agents_own() {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let provider = stub_provider(vec![]).await;
        let may_dir = tempfile::tempdir().expect("may dir");
        let state = failover_state(may_dir.path(), &provider, &provider).await;
        let guard = state
            .runtime_service
            .runs()
            .acquire("session")
            .await
            .expect("acquire run");
        let fence = guard.fence();
        let db = state.db();
        db.append_message(
            "session",
            crate::session::MessageRole::User,
            "earlier question",
            None,
            crate::db::MessageMeta::default(),
        )
        .await
        .expect("seed user row");
        db.append_message(
            "session",
            crate::session::MessageRole::Assistant,
            "earlier answer",
            None,
            crate::db::MessageMeta::default(),
        )
        .await
        .expect("seed assistant row");
        let req = ChatRequest {
            session_id: "session".to_string(),
            content: "next".to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        };
        let ctx = resolve_session_context(&state, &req, fence, false, true, None)
            .await
            .expect("context resolves");
        let config = state.config_snapshot();
        let mut agent = build_session_agent(&state, &config, &ctx, fence, 0)
            .await
            .expect("agent builds");
        let checkpoint = agent.export_checkpoint().await.expect("checkpoint");
        let entries = db
            .transcript_entries_after("session", -1)
            .await
            .expect("transcript");
        assert_eq!(
            entries.len(),
            checkpoint.context.items().len(),
            "every agent-committed item is a transcript entry"
        );
        for (entry, item) in entries.iter().zip(checkpoint.context.items()) {
            assert_eq!(
                entry.seq,
                i64::try_from(item.seq()).expect("seq"),
                "transcript seqs are aither's own seqs"
            );
        }
    }
}
