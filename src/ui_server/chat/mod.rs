mod tool_dispatch;
mod turn_persister;

use self::tool_dispatch::{
    handle_browser_request, handle_notify_request, handle_register_skills_request,
    handle_register_subagents_request, handle_task_request,
};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::Result as SkyResult;
use skyzen::responder::Sse;
use skyzen::utils::{Json, State};

use aither_core::llm::{
    Attachment as LlmAttachment, Message as LlmMessage, Role as LlmRole, ToolResult, Usage,
};
use aither_models::lookup as lookup_model_info;
use aither_sandbox::{
    AnyContainerExec, BollardContainerExec, ContainerLaunchSpec, Content, OutputEntry,
    TERMINAL_STDIN_BLOCKED_NOTICE, TerminalResult,
};

use crate::agent::MayAgent;
use crate::ask_user;
use crate::prompt_mentions;
use crate::session::{
    ChatHistory, ChatMessage, ChatMessageStats, MayHomePaths, MessageRole, PendingUserMessage,
    PersistedAttachment, SessionId, SessionPaths, SessionRuntimeState, SessionStore,
    TerminalRuntimeState,
};
use crate::task_trigger::EVENT_CHAT_TURN_COMPLETED;
use crate::ui_event_bus::UiEventTopics;

use super::attachments::build_prompt_and_attachments;
use super::browser::ensure_browser_runtime_started;
use super::error::internal_error;
use super::error_chain::error_chain_text;
use super::helpers::{RETRY_BACKOFF_MS, elapsed_ms_saturating, send_event, truncate_preview};
use super::model_resolution::resolve_default_model;
use super::state::{UiServerState, UiToolQueues};
use super::terminal_parsing::{
    TerminalTaskStatus, parse_terminal_result_payload, update_terminal_snapshot_from_tool_event,
};
use super::title_generation;
use super::types::{
    ApiErrorCode, ApiMessage, ChatRequest, PendingMessageKind, RemovePendingMessageRequest,
    RequestLogEntry, RequestStats, SessionRuntimeResponse, SseChunkEvent, SseDoneEvent,
    SseErrorEvent, SsePingEvent, SseSpeedEvent, SseStartEvent, SseTodoUpdateEvent,
    SseToolDeltaEvent, SseToolEndEvent, SseToolStartEvent, TerminalStateSnapshot, ToolEventRecord,
    ToolVariant,
};
use crate::decode_meter::DecodeMeter;

fn stopped_terminal_tool_result() -> ToolResult {
    ToolResult::json(&TerminalResult {
        stdout: OutputEntry::Empty,
        stderr: None,
        exit_code: 130,
        task_id: None,
        status: Some("stopped".to_string()),
        background_reason: None,
    })
    .expect("stopped terminal tool result must serialize")
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
        let record = ToolEventRecord::End {
            id: call_id.clone(),
            name: "terminal".to_string(),
            result: stopped_terminal_tool_result(),
        };
        let serialized = serde_json::to_string(&record)
            .map_err(|error| crate::db::ChatDbError::Storage(error.to_string()))?;
        turn_persister.push_tool_event(call_id, serialized);
        update_terminal_snapshot_from_tool_event(terminal_snapshot, &record);
    }
    Ok(())
}

// ── ChatSink trait ──────────────────────────────────────────

/// Abstraction over how chat events are delivered to the caller.
///
/// The stream protocol carries ordering metadata — `seq` (monotonic per
/// turn, shared with pings) and `turn_id` (UUID-per-retry) — so the
/// frontend can detect gaps and reconcile with history.
///
/// `SseSink` streams events to the browser with a running seq counter
/// and a heartbeat task. `CollectSink` is a no-op used by internal
/// follow-up dispatch when no client is attached.
trait ChatSink {
    fn on_start(
        &mut self,
        session_id: &str,
        assistant_id: &str,
    ) -> impl Future<Output = bool> + Send;
    fn on_text(&mut self, chunk: &str) -> impl Future<Output = bool> + Send;
    fn on_reasoning(&mut self, chunk: &str) -> impl Future<Output = bool> + Send;
    fn on_tool_delta(
        &mut self,
        id: &str,
        name: &str,
        arguments_fragment: &str,
        summary: crate::ui_server::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send;
    fn on_tool_start(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        summary: crate::ui_server::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send;
    fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &ToolResult,
        summary: crate::ui_server::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send;
    fn on_todo_update(
        &mut self,
        checklist: &crate::todo_parser::TodoChecklist,
    ) -> impl Future<Output = bool> + Send;
    fn on_speed(&mut self, tokens_per_second: f64) -> impl Future<Output = bool> + Send;
    fn on_error(
        &mut self,
        code: ApiErrorCode,
        message: &str,
        retryable: bool,
    ) -> impl Future<Output = bool> + Send;
}

/// Shared seq counter — `AtomicU64` so the background heartbeat task
/// can allocate its own `seq` values without racing the main loop.
type SeqCounter = std::sync::Arc<std::sync::atomic::AtomicU64>;

struct SseSink {
    sender: skyzen::responder::sse::Sender,
    seq: SeqCounter,
    turn_id: String,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl SseSink {
    fn new(sender: skyzen::responder::sse::Sender) -> Self {
        Self {
            sender,
            seq: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            turn_id: String::new(),
            heartbeat: None,
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn spawn_heartbeat(&mut self) {
        if self.heartbeat.is_some() {
            return;
        }
        let sender = self.sender.clone();
        let seq = self.seq.clone();
        let turn_id = self.turn_id.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick — we just sent the `start` event.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let event = SsePingEvent {
                    seq: seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    turn_id: turn_id.as_str(),
                };
                if !send_event(&sender, "ping", &event).await {
                    break;
                }
            }
        });
        self.heartbeat = Some(handle);
    }
}

impl Drop for SseSink {
    fn drop(&mut self) {
        if let Some(handle) = self.heartbeat.take() {
            handle.abort();
        }
    }
}

impl ChatSink for SseSink {
    async fn on_start(&mut self, session_id: &str, assistant_id: &str) -> bool {
        self.turn_id.clear();
        self.turn_id.push_str(assistant_id);
        let event = SseStartEvent {
            seq: self.next_seq(),
            turn_id: &self.turn_id,
            session_id,
            assistant_id,
        };
        let ok = send_event(&self.sender, "start", &event).await;
        if ok {
            self.spawn_heartbeat();
        }
        ok
    }

    async fn on_text(&mut self, chunk: &str) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
            "text",
            &SseChunkEvent {
                seq,
                turn_id: &self.turn_id,
                chunk,
            },
        )
        .await
    }

    async fn on_reasoning(&mut self, chunk: &str) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
            "reasoning",
            &SseChunkEvent {
                seq,
                turn_id: &self.turn_id,
                chunk,
            },
        )
        .await
    }

    async fn on_tool_delta(
        &mut self,
        id: &str,
        name: &str,
        arguments_fragment: &str,
        summary: crate::ui_server::tool_summary::ToolSummary,
    ) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
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
        )
        .await
    }

    async fn on_tool_start(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        summary: crate::ui_server::tool_summary::ToolSummary,
    ) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
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
        )
        .await
    }

    async fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &ToolResult,
        summary: crate::ui_server::tool_summary::ToolSummary,
    ) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
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
        )
        .await
    }

    async fn on_todo_update(&mut self, checklist: &crate::todo_parser::TodoChecklist) -> bool {
        let seq = self.next_seq();
        let event = SseTodoUpdateEvent::from_checklist(checklist, seq, self.turn_id.clone());
        send_event(&self.sender, "todo_update", &event).await
    }

    async fn on_speed(&mut self, tokens_per_second: f64) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
            "speed",
            &SseSpeedEvent {
                seq,
                turn_id: &self.turn_id,
                tokens_per_second,
            },
        )
        .await
    }

    async fn on_error(&mut self, code: ApiErrorCode, message: &str, retryable: bool) -> bool {
        let seq = self.next_seq();
        send_event(
            &self.sender,
            "error",
            &SseErrorEvent {
                seq,
                turn_id: &self.turn_id,
                code,
                message,
                retryable,
            },
        )
        .await
    }
}

/// No-op sink for internal follow-up runs where no client is attached.
/// Events are still persisted via the main loop; the sink only suppresses
/// network emission.
struct CollectSink;

impl ChatSink for CollectSink {
    fn on_start(&mut self, _: &str, _: &str) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_text(&mut self, _: &str) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_reasoning(&mut self, _: &str) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_tool_delta(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        _: crate::ui_server::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_tool_start(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        _: crate::ui_server::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_tool_end(
        &mut self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &ToolResult,
        _: crate::ui_server::tool_summary::ToolSummary,
    ) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_todo_update(
        &mut self,
        _: &crate::todo_parser::TodoChecklist,
    ) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_speed(&mut self, _: f64) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
    fn on_error(&mut self, _: ApiErrorCode, _: &str, _: bool) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }
}

/// Interval between `speed` events so fast providers don't flood the stream.
const SPEED_EMIT_INTERVAL: Duration = Duration::from_millis(300);

/// Feed streamed output bytes into `meter` and emit a `speed` event when the
/// interval has elapsed. Returns the sink liveness flag, like the `on_*` calls.
async fn note_output_and_emit_speed(
    sink: &mut impl ChatSink,
    meter: &mut DecodeMeter,
    last_emit: &mut Option<Instant>,
    bytes: usize,
) -> bool {
    meter.note_output(bytes);
    if last_emit.is_some_and(|t| t.elapsed() < SPEED_EMIT_INTERVAL) {
        return true;
    }
    let Some(rate) = meter.rate() else {
        return true;
    };
    *last_emit = Some(Instant::now());
    sink.on_speed(rate).await
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
) -> crate::ui_server::tool_summary::ToolSummary {
    let raw = pending_streams
        .get(id)
        .map(|p| p.buffered_arguments.as_str())
        .or(fallback)
        .unwrap_or("");
    crate::ui_server::tool_summary::project_tool_summary(raw, ToolVariant::infer(name))
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
    let arguments_fragment =
        merge_tool_arguments(&mut pending.buffered_arguments, arguments_fragment);

    if pending.emitted_start_to_sink {
        return SinkToolEvent::Delta {
            id: id.to_string(),
            name: pending.name.clone(),
            arguments_fragment,
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
    assistant_id: String,
    full_text: String,
    reasoning_text: String,
    terminal_snapshot: TerminalStateSnapshot,
    usage: Usage,
    provider_id: String,
    model_context_window: Option<u32>,
    elapsed_ms: u64,
}

fn api_message_from_run_result(result: AgentRunResult) -> ApiMessage {
    ApiMessage {
        id: result.assistant_id,
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
    }
}

struct SessionRunRegistration {
    state: Arc<UiServerState>,
    session_id: String,
    receiver: async_channel::Receiver<()>,
}

impl SessionRunRegistration {
    /// Claim the session's run, or `Ok(None)` when another run holds it.
    ///
    /// A storage failure is an error, never a refusal: reporting it as "the
    /// session is already active" would hide the failure behind a conflict.
    async fn try_new(
        state: Arc<UiServerState>,
        session_id: impl Into<String>,
    ) -> Result<Option<Self>, crate::db::ChatDbError> {
        let session_id = session_id.into();
        if !state
            .db()
            .claim_session_run_lease(session_id.as_str())
            .await?
        {
            return Ok(None);
        }
        let Some(receiver) = state.run_cancellation_registry.try_open(&session_id) else {
            state
                .db()
                .release_session_run_lease(session_id.as_str())
                .await?;
            return Ok(None);
        };
        Ok(Some(Self {
            state,
            session_id,
            receiver,
        }))
    }

    const fn receiver(&self) -> &async_channel::Receiver<()> {
        &self.receiver
    }

    async fn close(&self) -> Result<(), crate::db::ChatDbError> {
        let released = self
            .state
            .db()
            .release_session_run_lease(self.session_id.as_str())
            .await;
        self.state.run_cancellation_registry.close(&self.session_id);
        released
    }
}

/// The run registration a client turn proceeds with, or the response that
/// refuses it: a conflict when another run holds the session, an internal
/// error when claiming the run failed.
fn registration_or_refusal<T>(claim: Result<Option<T>, crate::db::ChatDbError>) -> SkyResult<T> {
    claim
        .map_err(internal_error)?
        .ok_or_else(active_session_conflict_error)
}

impl Drop for SessionRunRegistration {
    fn drop(&mut self) {
        self.state.run_cancellation_registry.close(&self.session_id);
    }
}

fn active_session_conflict_error() -> skyzen::Error {
    skyzen::Error::msg("session is already active; send this message via steer or queue instead")
        .set_status(skyzen::StatusCode::CONFLICT)
}

const STOP_WAIT_POLL_INTERVAL_MS: u64 = 120;

async fn wait_for_session_stop_settlement(
    state: &UiServerState,
    session_id: &str,
) -> Result<crate::session::SessionRuntimeSummary, crate::db::ChatDbError> {
    loop {
        let runtime = state.runtime_service.get(session_id).await?;
        let lease_active = state.db().session_run_lease_active(session_id).await?;

        if !lease_active && !runtime.interactions.is_active() {
            if runtime.state.is_active() {
                state
                    .runtime_service
                    .clear_running_if_needed(session_id)
                    .await?;
                return state.runtime_service.get(session_id).await;
            }
            return Ok(runtime);
        }

        async_io::Timer::after(Duration::from_millis(STOP_WAIT_POLL_INTERVAL_MS)).await;
    }
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

fn api_attachments_from_persisted(
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

async fn set_session_runtime_state(
    state: &Arc<UiServerState>,
    session_id: &str,
    runtime_state: SessionRuntimeState,
) -> Result<(), crate::db::ChatDbError> {
    state
        .runtime_service
        .set_state(session_id, runtime_state)
        .await
}

async fn clear_running_state_if_needed(
    state: &Arc<UiServerState>,
    session_id: &str,
) -> Result<(), crate::db::ChatDbError> {
    state
        .runtime_service
        .clear_running_if_needed(session_id)
        .await?;
    // A chat turn persists its buffered messages before reaching this boundary.
    // Refresh the profile snapshot only after the turn has settled so every UI
    // client can invalidate its active-session history without racing the
    // per-turn chat stream.
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(())
}

async fn set_terminal_runtime_state(
    state: &Arc<UiServerState>,
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
    state: &Arc<UiServerState>,
    session_id: &str,
) -> Result<(), crate::db::ChatDbError> {
    state
        .runtime_service
        .clear_terminal_runtime(session_id)
        .await
        .map(|_| ())
}

fn output_contains_terminal_wait_notice(output: &OutputEntry) -> bool {
    let content = match output {
        OutputEntry::Inline { content } | OutputEntry::Loaded { content, .. } => Some(content),
        OutputEntry::Stored { content, .. } => content.as_ref(),
        OutputEntry::Empty => None,
    };
    content
        .and_then(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .is_some_and(|text| text.starts_with(TERMINAL_STDIN_BLOCKED_NOTICE))
}

fn result_waits_for_terminal_input(result: &ToolResult) -> bool {
    if result.is_error() {
        return false;
    }
    let Some(parsed) = parse_terminal_result_payload(result) else {
        return false;
    };
    parsed.status == Some(TerminalTaskStatus::Running)
        && output_contains_terminal_wait_notice(&parsed.stdout)
}

pub(super) fn spawn_follow_up_runs(state: Arc<UiServerState>, session_id: String) {
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        let run_registration =
            match SessionRunRegistration::try_new(state.clone(), session_id.clone()).await {
                Ok(Some(run_registration)) => run_registration,
                // The run holding the session dispatches pending work when it ends.
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(session_id = %session_id, error = %error, "failed to claim session run for follow-up dispatch");
                    return;
                }
            };

        loop {
            let next = match state
                .runtime_service
                .take_next_dispatchable(&session_id)
                .await
            {
                Ok(next) => next,
                Err(error) => {
                    tracing::warn!(session_id = %session_id, error = %error, "failed to load next pending message");
                    break;
                }
            };
            let Some(next) = next else {
                break;
            };

            let request = ChatRequest {
                session_id: session_id.clone(),
                content: next.content,
                attachments: api_attachments_from_persisted(next.attachments),
                hidden_context_xml: next.hidden_context_xml,
                delivery_mode: next.delivery_mode,
            };
            if let Err(error) =
                Box::pin(run_chat_collect_with_receiver(
                    state.clone(),
                    request,
                    true,
                    true,
                    run_registration.receiver(),
                ))
                .await
            {
                tracing::warn!(session_id = %session_id, error = %error, "follow-up run failed");
                break;
            }
        }

        if let Err(error) = run_registration.close().await {
            tracing::error!(session_id = %session_id, error = %error, "failed to release session run after follow-up dispatch");
        }
        if let Err(error) = clear_running_state_if_needed(&state, &session_id).await {
            tracing::error!(session_id = %session_id, error = %error, "failed to clear running state after follow-up dispatch");
        }
    });
}

async fn spawn_follow_up_runs_if_dispatchable(
    state: &Arc<UiServerState>,
    session_id: &str,
) -> Result<(), crate::db::ChatDbError> {
    let runtime = state.runtime_service.get(session_id).await?;
    if runtime.can_dispatch_follow_up() {
        spawn_follow_up_runs(state.clone(), session_id.to_string());
    }
    Ok(())
}

pub(super) async fn resume_pending_follow_up_runs(
    state: Arc<UiServerState>,
) -> Result<(), crate::db::ChatDbError> {
    let session_ids = state.db().sessions_requiring_pending_dispatch().await?;
    for session_id in session_ids {
        spawn_follow_up_runs(state.clone(), session_id);
    }
    Ok(())
}

#[derive(Serialize, skyzen::ToSchema)]
struct BrowserTakeoverAlertXml {
    #[serde(rename = "@type")]
    alert_type: String,
    event: String,
    instruction: String,
}

#[derive(Serialize, skyzen::ToSchema)]
struct ResumeWaitingTurnXml<'a> {
    #[serde(rename = "@type")]
    alert_type: &'a str,
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
    instruction: &'a str,
}

#[derive(Clone, Copy, Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResumeRequestKind {
    Permission,
    AskUser,
    Workspace,
}

#[derive(Clone, Copy, Serialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResumeResolution {
    Approved,
    Denied,
    Answered,
}

fn serialize_xml<T: Serialize>(tag: &str, value: &T) -> Result<String, ChatMarkupError> {
    crate::xml::serialize_xml_block(tag, value).map_err(ChatMarkupError::Serialize)
}

fn browser_takeover_alert_xml() -> Result<String, ChatMarkupError> {
    serialize_xml(
        "system-reminder",
        &BrowserTakeoverAlertXml {
            alert_type: "browser_takeover".to_string(),
            event: "browser_control_returned".to_string(),
            instruction:
                "User manually controlled the browser and just returned control. You must re-check current page state before continuing automation."
                    .to_string(),
        },
    )
}

fn resume_waiting_turn_xml(
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
) -> Result<crate::XmlBlock, ChatMarkupError> {
    let xml = serialize_xml(
        "system-reminder",
        &ResumeWaitingTurnXml {
            alert_type: "resume_waiting_turn",
            request_kind,
            resolution,
            instruction: "The previous run was interrupted while waiting for this response. Reassess the latest state, use the recorded response, and continue from the most sensible next action.",
        },
    )?;
    crate::XmlBlock::parse(xml).map_err(ChatMarkupError::Parse)
}

pub(super) async fn schedule_waiting_turn_resume(
    state: &Arc<UiServerState>,
    session_id: &str,
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
) -> Result<crate::session::SessionRuntimeSummary, ScheduleWaitingTurnError> {
    let pending = PendingUserMessage {
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: Some(resume_waiting_turn_xml(request_kind, resolution)?),
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: crate::session::PendingUserMessageMode::Steer,
    };
    let runtime = state
        .runtime_service
        .enqueue(
            session_id,
            pending,
            crate::session::PendingUserMessageMode::Steer,
        )
        .await
        .map_err(ScheduleWaitingTurnError::Enqueue)?;
    spawn_follow_up_runs(state.clone(), session_id.to_string());
    Ok(runtime)
}

async fn schedule_browser_takeover_resume(
    state: &Arc<UiServerState>,
    session_id: &str,
) -> Result<crate::session::SessionRuntimeSummary, crate::db::ChatDbError> {
    let pending = PendingUserMessage {
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

async fn record_request_log(state: &Arc<UiServerState>, entry: RequestLogEntry) {
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
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let pending = pending_user_message_from_request(req.clone());
    let runtime = state
        .runtime_service
        .enqueue(&req.session_id, pending, req.delivery_mode)
        .await
        .map_err(internal_error)?;
    if runtime.can_dispatch_follow_up() {
        spawn_follow_up_runs(state.clone(), req.session_id.clone());
    }

    Ok(Json(runtime.into()))
}

pub(super) async fn remove_pending_message(
    params: skyzen::routing::Params,
    Json(req): Json<RemovePendingMessageRequest>,
    State(state): State<Arc<UiServerState>>,
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
    State(state): State<Arc<UiServerState>>,
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

    let persisted_cancel = state
        .db()
        .request_session_run_cancel(&session_id)
        .await
        .map_err(internal_error)?;
    let cancelled = state.run_cancellation_registry.cancel(&session_id) || persisted_cancel;
    tracing::info!(session_id = %session_id, cancelled, "received session stop request");

    let runtime = wait_for_session_stop_settlement(&state, &session_id)
        .await
        .map_err(internal_error)?;
    Ok(Json(runtime.into()))
}

pub(super) async fn chat_stream(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Sse> {
    let runtime = state
        .runtime_service
        .get(&req.session_id)
        .await
        .map_err(internal_error)?;
    if runtime.is_active() {
        return Err(active_session_conflict_error());
    }

    let run_registration = registration_or_refusal(
        SessionRunRegistration::try_new(state.clone(), req.session_id.clone()).await,
    )?;
    let (sender, sse) = Sse::channel();

    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        if let Err(err) = Box::pin(stream_chat_events(
            state.clone(),
            req,
            sender.clone(),
            run_registration,
        ))
        .await
        {
            // Envelope-shaped error for an outer-level failure that bypassed
            // the sink's own error pathway. `seq = 0` is reserved for events
            // with no turn context (heartbeat analogue).
            send_event(
                &sender,
                "error",
                &SseErrorEvent {
                    seq: 0,
                    turn_id: "",
                    code: ApiErrorCode::InternalError,
                    message: &err.to_string(),
                    retryable: false,
                },
            )
            .await;
        }
    });

    Ok(sse)
}

pub(super) fn spawn_shared_tool_dispatchers(state: Arc<UiServerState>, queues: UiToolQueues) {
    let UiToolQueues {
        task,
        notify,
        browser,
        register_skills,
        register_subagents,
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

    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Some(request) = register_subagents.next().await {
            let result = handle_register_subagents_request(&state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });
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
    state: &'a Arc<UiServerState>,
    req: &'a ChatRequest,
    sink: &'a mut impl ChatSink,
    cancel_rx: &'a async_channel::Receiver<()>,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
) -> Pin<Box<impl Future<Output = Result<AgentRunResult, AgentLoopError>> + 'a>> {
    Box::pin(async move {
        let mut attempt = 0usize;
        let mut max_retries = 0usize;
        let mut turn_hidden_context_xml: Option<String> = None;

        loop {
            let assistant_uuid = uuid::Uuid::new_v4().to_string();
            let assistant_id = format!("assistant-{assistant_uuid}");
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
            } = match prepare_agent_for_request(
                state,
                req,
                persist_user_message && attempt == 0,
                use_persisted_context_snapshot,
                attempt,
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = sink
                        .on_error(error.api_code(), &error.to_string(), false)
                        .await;
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
            if attempt == 0 {
                max_retries = provider_candidate_count.saturating_sub(1);
            }
            if let Some(hidden_context_xml) = hidden_context_xml {
                turn_hidden_context_xml = Some(hidden_context_xml);
            }
            let model_context_window = agent.model_context_window().or_else(|| {
                lookup_model_info(&current_model)
                    .and_then(aither_models::ModelEntry::max_input_tokens)
            });

            let mut usage = Usage::default();
            let mut decode_meter = DecodeMeter::default();
            let mut last_speed_emit: Option<Instant> = None;
            let mut segment_text = String::new(); // current text segment, flushed on tool calls
            let mut full_text = String::new(); // accumulated full assistant text
            let mut reasoning_text = String::new();
            let mut terminal_snapshot = TerminalStateSnapshot::default();
            let mut saw_tool_activity = false;
            let mut tool_arguments_by_id: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut pending_tool_streams: std::collections::HashMap<String, PendingToolStream> =
                std::collections::HashMap::new();
            let mut last_todo_hash: Option<u64> = None;
            let tasks_db = state.db();
            let tasks_session_id = req.session_id.clone();
            let mut turn_persister = turn_persister::TurnPersister::new(session_id.as_str());

            let mut terminal_error: Option<aither_agent::AgentError> = None;
            let mut cancellation_reason: Option<&'static str> = None;
            let mut browser_takeover_paused = false;
            let mut first_agent_event_seen = false;
            let mut first_visible_tool_seen = false;
            let mut first_text_seen = false;
            {
                agent.sync_runtime_context().await;
                let mut stream = agent.run_with_hidden_context(
                    &prompt,
                    turn_hidden_context_xml.as_deref(),
                    attachments,
                );
                loop {
                    let next_event = tokio::select! {
                        biased;
                        cancel = cancel_rx.recv() => {
                            if cancel == Ok(()) {
                                cancellation_reason = Some("run cancelled by user");
                            } else {
                                cancellation_reason = Some("run cancellation channel closed unexpectedly");
                            }
                            None
                        }
                        next = stream.next() => next,
                    };
                    let Some(event) = next_event else {
                        break;
                    };
                    match event {
                        Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                            log_first_agent_event(
                                &mut first_agent_event_seen,
                                &started_at,
                                session_id.as_str(),
                                assistant_id.as_str(),
                                "reasoning",
                            );
                            reasoning_text.push_str(&text);
                            if !sink.on_reasoning(&text).await {
                                cancellation_reason =
                                    Some("client disconnected during reasoning stream");
                                break;
                            }
                            if !note_output_and_emit_speed(
                                sink,
                                &mut decode_meter,
                                &mut last_speed_emit,
                                text.len(),
                            )
                            .await
                            {
                                cancellation_reason =
                                    Some("client disconnected during reasoning stream");
                                break;
                            }
                        }
                        Ok(aither_agent::AgentEvent::Text(text)) => {
                            log_first_agent_event(
                                &mut first_agent_event_seen,
                                &started_at,
                                session_id.as_str(),
                                assistant_id.as_str(),
                                "text",
                            );
                            log_first_text_event(
                                &mut first_text_seen,
                                &started_at,
                                session_id.as_str(),
                                assistant_id.as_str(),
                            );
                            segment_text.push_str(&text);
                            full_text.push_str(&text);
                            if !sink.on_text(&text).await {
                                cancellation_reason =
                                    Some("client disconnected during text stream");
                                break;
                            }
                            if !note_output_and_emit_speed(
                                sink,
                                &mut decode_meter,
                                &mut last_speed_emit,
                                text.len(),
                            )
                            .await
                            {
                                cancellation_reason =
                                    Some("client disconnected during text stream");
                                break;
                            }
                        }
                        Ok(aither_agent::AgentEvent::Usage(u)) => {
                            decode_meter.note_usage(u.completion_tokens);
                            usage = u;
                        }
                        Ok(aither_agent::AgentEvent::ToolCallDelta {
                            id,
                            name,
                            arguments_fragment,
                        }) => {
                            log_first_agent_event(
                                &mut first_agent_event_seen,
                                &started_at,
                                session_id.as_str(),
                                assistant_id.as_str(),
                                "tool_delta",
                            );
                            tracing::trace!(
                                target: "may::chat::tool_delta",
                                id = %id,
                                name = %name,
                                fragment = %arguments_fragment,
                                "incoming ToolCallDelta from aither"
                            );
                            if !note_output_and_emit_speed(
                                sink,
                                &mut decode_meter,
                                &mut last_speed_emit,
                                arguments_fragment.len(),
                            )
                            .await
                            {
                                cancellation_reason =
                                    Some("client disconnected during tool stream");
                                break;
                            }
                            match normalize_tool_delta_for_sink(
                                &mut pending_tool_streams,
                                &id,
                                &name,
                                &arguments_fragment,
                            ) {
                                SinkToolEvent::Start {
                                    id,
                                    name,
                                    arguments,
                                } => {
                                    log_first_visible_tool_event(
                                        &mut first_visible_tool_seen,
                                        &started_at,
                                        session_id.as_str(),
                                        assistant_id.as_str(),
                                        id.as_str(),
                                        name.as_str(),
                                        "synthetic_start_from_delta",
                                    );
                                    let summary = project_tool_summary_for_sink(
                                        &pending_tool_streams,
                                        &id,
                                        &name,
                                        Some(arguments.as_str()),
                                    );
                                    if !sink.on_tool_start(&id, &name, &arguments, summary).await {
                                        cancellation_reason =
                                            Some("client disconnected during tool stream");
                                        break;
                                    }
                                }
                                SinkToolEvent::Delta {
                                    id,
                                    name,
                                    arguments_fragment,
                                } => {
                                    let summary = project_tool_summary_for_sink(
                                        &pending_tool_streams,
                                        &id,
                                        &name,
                                        None,
                                    );
                                    if !sink
                                        .on_tool_delta(&id, &name, &arguments_fragment, summary)
                                        .await
                                    {
                                        cancellation_reason =
                                            Some("client disconnected during tool stream");
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(aither_agent::AgentEvent::ToolCallStart {
                            id,
                            name,
                            arguments,
                        }) => {
                            log_first_agent_event(
                                &mut first_agent_event_seen,
                                &started_at,
                                session_id.as_str(),
                                assistant_id.as_str(),
                                "tool_start",
                            );
                            saw_tool_activity = true;
                            decode_meter.note_boundary();
                            if matches!(
                                name.as_str(),
                                "terminal" | "terminal_input" | "terminal_kill" | "terminal_read"
                            ) {
                                set_session_runtime_state(
                                    state,
                                    session_id.as_str(),
                                    SessionRuntimeState::Running,
                                )
                                .await?;
                            }
                            // Buffer accumulated text segment before the tool call.
                            if !segment_text.trim().is_empty() {
                                turn_persister.push_assistant(segment_text.clone());
                                segment_text.clear();
                            }

                            let record = ToolEventRecord::Start {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            };
                            let serialized = serde_json::to_string(&record)
                                .map_err(|e| crate::db::ChatDbError::Storage(e.to_string()))?;
                            turn_persister.push_tool_event(id.clone(), serialized);
                            update_terminal_snapshot_from_tool_event(
                                &mut terminal_snapshot,
                                &record,
                            );
                            tool_arguments_by_id.insert(id.clone(), arguments.clone());
                            if let Some(SinkToolEvent::Start {
                                id,
                                name,
                                arguments,
                            }) = normalize_tool_start_for_sink(
                                &mut pending_tool_streams,
                                &id,
                                &name,
                                &arguments,
                            ) {
                                log_first_visible_tool_event(
                                    &mut first_visible_tool_seen,
                                    &started_at,
                                    session_id.as_str(),
                                    assistant_id.as_str(),
                                    id.as_str(),
                                    name.as_str(),
                                    "canonical_start",
                                );
                                let summary = project_tool_summary_for_sink(
                                    &pending_tool_streams,
                                    &id,
                                    &name,
                                    Some(arguments.as_str()),
                                );
                                if !sink.on_tool_start(&id, &name, &arguments, summary).await {
                                    cancellation_reason =
                                        Some("client disconnected during tool stream");
                                    break;
                                }
                            }
                        }
                        Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
                            log_first_agent_event(
                                &mut first_agent_event_seen,
                                &started_at,
                                session_id.as_str(),
                                assistant_id.as_str(),
                                "tool_end",
                            );
                            saw_tool_activity = true;
                            if name == "terminal" && result_waits_for_terminal_input(&result) {
                                set_session_runtime_state(
                                    state,
                                    session_id.as_str(),
                                    SessionRuntimeState::WaitingTerminalInput,
                                )
                                .await?;
                            }
                            let record = ToolEventRecord::End {
                                id: id.clone(),
                                name: name.clone(),
                                result: result.clone(),
                            };
                            let serialized = serde_json::to_string(&record)
                                .map_err(|e| crate::db::ChatDbError::Storage(e.to_string()))?;
                            turn_persister.push_tool_event(id.clone(), serialized);
                            update_terminal_snapshot_from_tool_event(
                                &mut terminal_snapshot,
                                &record,
                            );
                            let arguments = tool_arguments_by_id.remove(&id);
                            let summary = project_tool_summary_for_sink(
                                &pending_tool_streams,
                                &id,
                                &name,
                                arguments.as_deref(),
                            );
                            pending_tool_streams.remove(&id);
                            if !sink
                                .on_tool_end(&id, &name, arguments.as_deref(), &result, summary)
                                .await
                            {
                                cancellation_reason =
                                    Some("client disconnected during tool stream");
                                break;
                            }

                            // Check if tasks.md changed after tool execution.
                            if let Some(checklist) = check_tasks_update(
                                &tasks_db,
                                &tasks_session_id,
                                &mut last_todo_hash,
                            )
                            .await
                                && !sink.on_todo_update(&checklist).await
                            {
                                cancellation_reason =
                                    Some("client disconnected during todo update");
                                break;
                            }
                        }
                        Ok(aither_agent::AgentEvent::BackgroundTaskStarted {
                            task_id,
                            output_preview,
                            output_file,
                            reason,
                        }) => {
                            saw_tool_activity = true;
                            set_terminal_runtime_state(
                                state,
                                session_id.as_str(),
                                TerminalRuntimeState {
                                    task_id: Some(task_id),
                                    output_file: Some(output_file),
                                    output_preview: Some(output_preview),
                                    input_notice: None,
                                    background_reason: Some(reason),
                                },
                            )
                            .await?;
                        }
                        Ok(aither_agent::AgentEvent::BackgroundTaskCompleted { .. }) => {
                            saw_tool_activity = true;
                            clear_terminal_runtime_state(state, session_id.as_str()).await?;
                            set_session_runtime_state(
                                state,
                                session_id.as_str(),
                                SessionRuntimeState::Running,
                            )
                            .await?;
                        }
                        Ok(aither_agent::AgentEvent::TerminalInputNeeded { task_id, notice }) => {
                            saw_tool_activity = true;
                            set_session_runtime_state(
                                state,
                                session_id.as_str(),
                                SessionRuntimeState::WaitingTerminalInput,
                            )
                            .await?;
                            let current_terminal = state
                                .runtime_service
                                .get(session_id.as_str())
                                .await?
                                .interactions
                                .terminal;
                            set_terminal_runtime_state(
                                state,
                                session_id.as_str(),
                                current_terminal.with_input_notice(task_id, notice),
                            )
                            .await?;
                        }
                        Ok(aither_agent::AgentEvent::Error(err)) | Err(err) => {
                            terminal_error = Some(err);
                            break;
                        }
                        Ok(_) => {}
                    }

                    if is_takeover_active_for_session(state, &session_id).await {
                        browser_takeover_paused = true;
                        break;
                    }
                }
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

            let log_base = RequestLogEntry::new(
                session_id.as_str(),
                &current_model,
                &provider_id,
                &usage,
                model_context_window,
            );

            if browser_takeover_paused {
                const TAKEOVER_PAUSED_MESSAGE: &str =
                    "Browser takeover is active for this session; model execution paused";

                persist_stopped_terminal_calls(&mut terminal_snapshot, &mut turn_persister)?;
                if !segment_text.trim().is_empty() {
                    turn_persister.push_assistant(segment_text.clone());
                }

                let has_assistant_text = !full_text.trim().is_empty();
                let has_tool_activity =
                    !terminal_snapshot.executions.is_empty() || saw_tool_activity;
                if !has_assistant_text && has_tool_activity {
                    turn_persister.push_assistant(String::new());
                }

                // Flush the entire turn atomically
                turn_persister.flush(&state.db()).await?;
                clear_terminal_runtime_state(state, session_id.as_str()).await?;

                record_request_log(
                    state,
                    log_base.failure(elapsed_ms_saturating(&started_at), TAKEOVER_PAUSED_MESSAGE),
                )
                .await;
                schedule_browser_takeover_resume(state, session_id.as_str()).await?;

                return Err(AgentLoopError::BrowserTakeoverPaused(Box::new(
                    AgentRunResult {
                        assistant_id,
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
                if err.is_retryable() && attempt < max_retries && full_text.trim().is_empty() {
                    tokio::time::sleep(Duration::from_millis(
                        RETRY_BACKOFF_MS * (attempt as u64 + 1),
                    ))
                    .await;
                    attempt += 1;
                    continue;
                }

                let err_msg = err.to_string();
                record_request_log(
                    state,
                    log_base.failure(elapsed_ms_saturating(&started_at), &err_msg),
                )
                .await;
                let _ = sink
                    .on_error(ApiErrorCode::InternalError, &err_msg, false)
                    .await;
                return Err(err.into());
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
                    turn_persister.push_assistant(segment_text.clone());
                }
                // Flush the entire turn atomically before returning
                turn_persister.flush(&state.db()).await?;
                record_request_log(
                    state,
                    log_base.failure(elapsed_ms_saturating(&started_at), reason),
                )
                .await;
                return Err(AgentLoopError::Cancelled(reason));
            }

            // Buffer remaining text segment.
            if !segment_text.trim().is_empty() {
                turn_persister.push_assistant(segment_text.clone());
            }

            let has_assistant_text = !full_text.trim().is_empty();
            let has_terminal_output = !terminal_snapshot.executions.is_empty();
            let has_tool_activity = has_terminal_output || saw_tool_activity;
            if !has_assistant_text && !has_tool_activity {
                let err = "Model returned an empty assistant response";
                record_request_log(
                    state,
                    log_base.failure(elapsed_ms_saturating(&started_at), err),
                )
                .await;
                let _ = sink.on_error(ApiErrorCode::InternalError, err, false).await;
                return Err(AgentLoopError::EmptyAssistantResponse);
            }
            if !has_assistant_text && has_tool_activity {
                turn_persister.push_assistant(String::new());
            }

            // Flush the entire turn atomically
            turn_persister.flush(&state.db()).await?;
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
            record_request_log(state, log_base.success(elapsed_ms)).await;

            // Post-completion hooks
            schedule_profile_memory_processing(state, profile_id.as_deref()).await;
            title_generation::enqueue(state, &session_id, req.content.clone(), full_text.clone());
            enqueue_event_triggered_tasks(state, &session_id, profile_id.as_deref());

            return Ok(AgentRunResult {
                assistant_id,
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

async fn stream_chat_events(
    state: Arc<UiServerState>,
    req: ChatRequest,
    sender: skyzen::responder::sse::Sender,
    run_registration: SessionRunRegistration,
) -> Result<(), crate::db::ChatDbError> {
    set_session_runtime_state(&state, &req.session_id, SessionRuntimeState::Running).await?;
    let mut sink = SseSink::new(sender.clone());
    let result = run_agent_loop(
        &state,
        &req,
        &mut sink,
        run_registration.receiver(),
        true,
        true,
    )
    .await;
    let response_session_id = req.session_id.clone();
    match result {
        Ok(result) => {
            let done_event = SseDoneEvent {
                seq: sink.next_seq(),
                turn_id: sink.turn_id.clone(),
                session_id: response_session_id.clone(),
                assistant: api_message_from_run_result(result),
            };
            // Release the session before announcing the end of the turn, so
            // a client that sends its next message on `done` is never refused
            // as a concurrent run. For the same reason the follow-up
            // dispatcher starts only when there is pending work: it claims
            // the session's run, and claiming it with nothing to dispatch
            // would refuse that next message.
            run_registration.close().await?;
            clear_running_state_if_needed(&state, &response_session_id).await?;
            send_event(&sender, "done", &done_event).await;
            spawn_follow_up_runs_if_dispatchable(&state, &response_session_id).await?;
        }
        Err(AgentLoopError::BrowserTakeoverPaused(result)) => {
            let done_event = SseDoneEvent {
                seq: sink.next_seq(),
                turn_id: sink.turn_id.clone(),
                session_id: response_session_id.clone(),
                assistant: api_message_from_run_result(*result),
            };
            run_registration.close().await?;
            clear_running_state_if_needed(&state, &response_session_id).await?;
            send_event(&sender, "paused", &done_event).await;
            spawn_follow_up_runs_if_dispatchable(&state, &response_session_id).await?;
        }
        // Errors are already sent to the sink via on_error in run_agent_loop.
        Err(_) => {
            run_registration.close().await?;
            clear_running_state_if_needed(&state, &response_session_id).await?;
        }
    }
    Ok(())
}

/// Drive a chat turn without an attached SSE client — used by the follow-up
/// queue to dispatch server-side enqueued messages. All assistant text and
/// tool events are still persisted by the main loop; the sink only
/// suppresses network emission.
async fn run_chat_collect_with_receiver(
    state: Arc<UiServerState>,
    req: ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    cancel_rx: &async_channel::Receiver<()>,
) -> Result<(), ChatCollectError> {
    set_session_runtime_state(&state, &req.session_id, SessionRuntimeState::Running)
        .await
        .map_err(internal_error)
        .map_err(ChatCollectError::from)?;
    let mut sink = CollectSink;
    let result = run_agent_loop(
        &state,
        &req,
        &mut sink,
        cancel_rx,
        persist_user_message,
        use_persisted_context_snapshot,
    )
    .await;
    let takeover_paused = matches!(&result, Err(AgentLoopError::BrowserTakeoverPaused(_)));
    clear_running_state_if_needed(&state, &req.session_id)
        .await
        .map_err(internal_error)
        .map_err(ChatCollectError::from)?;
    if takeover_paused {
        spawn_follow_up_runs_if_dispatchable(&state, &req.session_id)
            .await
            .map_err(internal_error)
            .map_err(ChatCollectError::from)?;
    }
    match result {
        Ok(_) | Err(AgentLoopError::BrowserTakeoverPaused(_)) => Ok(()),
        Err(AgentLoopError::Preparation(error)) => Err(ChatCollectError::Preparation(error)),
        Err(other) => Err(ChatCollectError::Sky(internal_error(other))),
    }
}

async fn schedule_profile_memory_processing(state: &Arc<UiServerState>, profile_id: Option<&str>) {
    let Some(profile_id) = profile_id else {
        return;
    };
    let Some(processor) = state.memory_processor.as_ref() else {
        return;
    };
    processor.mark_profile_dirty(profile_id).await;
}

async fn persist_latest_assistant_stats(
    state: &Arc<UiServerState>,
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
    state: &Arc<UiServerState>,
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
    state: &Arc<UiServerState>,
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
    session_id: SessionId,
    current_model: String,
    provider_candidate_count: usize,
    sandbox_dir: std::path::PathBuf,
    history_messages: Vec<LlmMessage>,
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
}

struct ToolBrokers {
    workspace: crate::workspace_request::WorkspaceRequestBroker,
    ask_user: ask_user::AskUserBroker,
    task: crate::agent::task_tool::TaskBroker,
    notify: crate::agent::notify_tool::NotifyBroker,
    browser: crate::agent::browser_tool::BrowserBroker,
    register_skills: crate::agent::register_tool::RegisterBroker,
    register_subagents: crate::agent::register_tool::RegisterBroker,
}

#[derive(Debug, thiserror::Error)]
enum UiChatPreparationError {
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
    #[error("failed to serialize browser takeover alert: {0}")]
    BuildTakeoverAlert(#[source] ChatMarkupError),
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
    #[error("invalid container profile image tag: {0}")]
    InvalidContainerImageTag(#[source] crate::container::ContainerError),
    #[error("container image build failed: {0}")]
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
            Self::MissingContainerProfileModel
            | Self::ParseProfileEnvironment(_)
            | Self::InvalidContainerImageTag(_) => ApiErrorCode::ProfileConfigurationInvalid,
            Self::ResolveContainerRuntime(_) | Self::ResolveContainerIpcRoute(_) => {
                ApiErrorCode::ContainerRuntimeUnavailable
            }
            Self::EnsureContainerImage(_) => ApiErrorCode::ContainerBuildFailed,
            Self::StartContainer(_) => ApiErrorCode::ContainerStartFailed,
            Self::LoadLocation(_) => ApiErrorCode::LocationUnavailable,
            Self::LoadSession(_)
            | Self::LoadHistory(_)
            | Self::LoadProfile(_)
            | Self::LoadProfileMemories(_)
            | Self::LoadRuntime(_)
            | Self::LoadContextSnapshot(_)
            | Self::BuildTakeoverAlert(_)
            | Self::BuildSessionMentions(_)
            | Self::BuildFileMentions(_)
            | Self::UpdateSessionTitle(_)
            | Self::AppendUserMessage(_)
            | Self::PersistHiddenContext(_)
            | Self::ReconcileContainerLayout(_)
            | Self::BuildAgent(_)
            | Self::RestoreCheckpoint(_)
            | Self::BuildPrompt(_)
            | Self::PersistContextDebugSnapshot(_)
            | Self::HistoryReplay(_) => ApiErrorCode::InternalError,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ChatMarkupError {
    #[error("failed to serialize XML fragment: {0}")]
    Serialize(#[source] quick_xml::SeError),
    #[error("failed to parse XML fragment: {0}")]
    Parse(#[source] crate::XmlBlockError),
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ScheduleWaitingTurnError {
    #[error(transparent)]
    Markup(#[from] ChatMarkupError),
    #[error("failed to enqueue waiting-turn resume message: {0}")]
    Enqueue(#[source] crate::db::ChatDbError),
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
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
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
    let history = crate::session::load_history_from_db(&db, session_id.as_str())
        .await
        .map_err(UiChatPreparationError::LoadHistory)?;

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

    let history_messages = history
        .to_aither_messages()
        .map_err(UiChatPreparationError::HistoryReplay)?;
    let context_snapshot = if use_persisted_context_snapshot {
        db.get_session_context_snapshot(session_id.as_str())
            .await
            .map_err(UiChatPreparationError::LoadContextSnapshot)?
            .map(crate::session::parse_persisted_context_snapshot)
    } else {
        None
    };

    let mut hidden_context_parts = Vec::<String>::new();
    if persist_user_message && take_over_alert_pending(state, &session_id).await {
        push_hidden_context_part(
            &db,
            &session_id,
            &history,
            &mut hidden_context_parts,
            browser_takeover_alert_xml().map_err(UiChatPreparationError::BuildTakeoverAlert)?,
            persist_user_message,
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
            &session_id,
            &history,
            &mut hidden_context_parts,
            hidden_context_xml.to_string(),
            persist_user_message,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    if let Some(session_mentions_xml) =
        prompt_mentions::build_session_mentions_xml(&db, &req.content)
            .await
            .map_err(UiChatPreparationError::BuildSessionMentions)?
    {
        push_hidden_context_part(
            &db,
            &session_id,
            &history,
            &mut hidden_context_parts,
            session_mentions_xml,
            persist_user_message,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    let session_paths =
        session_paths_for_session(state, session.profile_id.as_deref(), &session_id);
    if let Some(file_mentions_xml) =
        prompt_mentions::build_file_mentions_xml(&session_paths.files_dir(), &req.content, 400)
            .await
            .map_err(|error| match error {
                prompt_mentions::PromptMentionError::AbsolutePath
                | prompt_mentions::PromptMentionError::ParentTraversal => {
                    UiChatPreparationError::InvalidFileMentionPath(error)
                }
                _ => UiChatPreparationError::BuildFileMentions(error),
            })?
    {
        push_hidden_context_part(
            &db,
            &session_id,
            &history,
            &mut hidden_context_parts,
            file_mentions_xml,
            persist_user_message,
        )
        .await
        .map_err(UiChatPreparationError::PersistHiddenContext)?;
    }

    if persist_user_message && request_has_visible_user_input(req) {
        let needs_title = session.title.is_none();
        if needs_title {
            let title = truncate_preview(&req.content);
            db.update_session(session_id.as_str(), Some(title.as_str()), None, None)
                .await
                .map_err(UiChatPreparationError::UpdateSessionTitle)?;
            state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
        }
        db.append_chat_message(session_id.as_str(), &ChatMessage::user(req.content.clone()))
            .await
            .map_err(UiChatPreparationError::AppendUserMessage)?;
    }

    let sandbox_dir = SessionStore::new(MayHomePaths::new(state.may_dir.clone()))
        .sandbox_dir(session_id.as_str(), session.profile_id.as_deref());

    let reasoning_effort = session
        .reasoning_effort
        .as_deref()
        .and_then(parse_reasoning_effort_token);

    Ok(SessionContext {
        session_id,
        current_model,
        provider_candidate_count,
        sandbox_dir,
        history_messages,
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
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        _ => None,
    }
}

fn session_paths_for_session(
    state: &UiServerState,
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
    session_id: &SessionId,
    history: &ChatHistory,
    hidden_context_parts: &mut Vec<String>,
    content: String,
    persist_user_message: bool,
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
        db.append_chat_message(
            session_id.as_str(),
            &ChatMessage::hidden_context(trimmed.to_string()),
        )
        .await?;
    }

    if !already_queued {
        hidden_context_parts.push(trimmed.to_string());
    }

    Ok(())
}

async fn setup_agent_environment(
    state: &Arc<UiServerState>,
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
            let default_image_tag = Box::pin(crate::container::profile::default_image_tag(
                &state.may_dir,
                profile,
                container_preferences.clone(),
            ))
            .await
            .map_err(UiChatPreparationError::InvalidContainerImageTag)?;

            let image_spec = crate::container::profile::image_spec_for_profile(
                &state.may_dir,
                profile,
                &default_image_tag,
                container_preferences.clone(),
            );
            let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences);

            crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
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
                image_spec.tag.clone(),
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
    state: &Arc<UiServerState>,
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
    state: &Arc<UiServerState>,
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

fn build_tool_brokers(state: &Arc<UiServerState>, session_id: &SessionId) -> ToolBrokers {
    ToolBrokers {
        workspace: spawn_workspace_forwarder(state, session_id),
        ask_user: spawn_ask_user_forwarder(state, session_id),
        task: state.tool_dispatchers.task.clone(),
        notify: state.tool_dispatchers.notify.clone(),
        browser: state.tool_dispatchers.browser.clone(),
        register_skills: state.tool_dispatchers.register_skills.clone(),
        register_subagents: state.tool_dispatchers.register_subagents.clone(),
    }
}

/// Resolves the session context and builds the agent for one chat turn.
///
/// Built on the heap for the reason given on [`run_agent_loop`].
fn prepare_agent_for_request<'a>(
    state: &'a Arc<UiServerState>,
    req: &'a ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    provider_attempt: usize,
) -> Pin<Box<impl Future<Output = Result<PreparedAgent, UiChatPreparationError>> + 'a>> {
    Box::pin(async move {
        let ctx = Box::pin(resolve_session_context(
            state,
            req,
            persist_user_message,
            use_persisted_context_snapshot,
        ))
        .await?;
        let (environment, location) = Box::pin(setup_agent_environment(state, &ctx)).await?;
        let brokers = build_tool_brokers(state, &ctx.session_id);

        let config = state.config_snapshot();
        let mut agent = MayAgent::from_config_with_model_and_permissions(
            &config,
            crate::agent::MayAgentSessionInput {
                sandbox_dir: ctx.sandbox_dir,
                may_dir: state.may_dir.clone(),
                cwd: state.cwd.clone(),
                session_id: ctx.session_id.clone(),
                workspace_request_broker: brokers.workspace,
                ask_user_broker: brokers.ask_user,
                db: state.db(),
                model: &ctx.current_model,
                reasoning_effort: ctx.reasoning_effort,
                profile_id: ctx.profile_id.clone(),
                permission_approver: Some(
                    state
                        .permission_approver
                        .for_session(ctx.session_id.as_str().to_string()),
                ),
                profile_persona: ctx.profile_persona,
                profile_memories: ctx.profile_memories,
                plan_state: ctx.plan_state,
                location,
                task_broker: Some(brokers.task),
                notify_broker: Some(brokers.notify),
                browser_broker: state.browser_manager().map(|_| brokers.browser),
                register_skills_broker: Some(brokers.register_skills),
                register_subagents_broker: Some(brokers.register_subagents),
                environment,
                tool_distribution: state.tool_distribution.clone(),
                tool_environments: state.tool_environments.clone(),
                provider_attempt,
            },
        )
        .await
        .map_err(UiChatPreparationError::BuildAgent)?;

        if let Some(snapshot) = ctx.context_snapshot.as_ref() {
            // Try to restore from checkpoint with ID-based delta.
            // If the snapshot anchor is stale, fall back to full history replay.
            let db_messages = state
                .db()
                .get_messages(ctx.session_id.as_str())
                .await
                .map_err(UiChatPreparationError::LoadSession)?;
            let db_ids: Vec<i64> = db_messages.iter().map(|m| i64::from(m.id)).collect();
            if let Some(delta) = crate::session::history_delta_since_snapshot(
                &ctx.history_messages,
                &db_ids,
                snapshot.last_message_db_id,
            ) {
                agent
                    .restore_checkpoint(snapshot.checkpoint.clone())
                    .map_err(UiChatPreparationError::RestoreCheckpoint)?;
                for message in delta {
                    agent.push_message(message);
                }
            } else {
                tracing::warn!(
                    session_id = %ctx.session_id.as_str(),
                    "context snapshot anchor is stale, falling back to full history replay"
                );
                // Delete the stale snapshot so future requests don't hit this path
                let _ = state
                    .db()
                    .delete_session_context_snapshot(ctx.session_id.as_str())
                    .await;
                for message in &ctx.history_messages {
                    agent.push_message(message.clone());
                }
            }
        } else {
            for message in ctx.history_messages {
                agent.push_message(message);
            }
        }

        let provider_id = agent.provider_id().to_string();

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
            .await;
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
        })
    })
}

/// Read tasks.md and return the parsed checklist if its content changed since the last check.
/// The sink is responsible for wrapping it in the SSE envelope with `seq/turn_id`.
async fn check_tasks_update(
    db: &crate::db::ChatDb,
    session_id: &str,
    last_hash: &mut Option<u64>,
) -> Option<crate::todo_parser::TodoChecklist> {
    use std::hash::{Hash, Hasher};

    let content = db.get_session_tasks(session_id).await.ok()??;
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

async fn is_takeover_active_for_session(state: &UiServerState, session_id: &SessionId) -> bool {
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

async fn take_over_alert_pending(state: &UiServerState, session_id: &SessionId) -> bool {
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
    use aither_core::llm::Message as LlmMessage;
    use skyzen::{HttpError, StatusCode};
    use std::collections::HashMap;

    #[test]
    fn run_claim_storage_failure_is_an_internal_error_not_a_conflict() {
        let refusal = super::registration_or_refusal::<()>(Err(crate::db::ChatDbError::Storage(
            "database is locked".to_string(),
        )))
        .expect_err("a failed claim must refuse the turn");
        let refusal = refusal.into_boxed_http_error();
        assert_eq!(refusal.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            refusal.to_string().contains("database is locked"),
            "the refusal should carry the storage failure: {refusal}"
        );
    }

    #[test]
    fn run_claim_held_by_another_run_is_a_conflict() {
        let refusal = super::registration_or_refusal::<()>(Ok(None))
            .expect_err("a held run must refuse the turn")
            .into_boxed_http_error();
        assert_eq!(refusal.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn history_delta_since_snapshot_returns_suffix_after_anchor_id() {
        let history = vec![
            LlmMessage::system("memory"),
            LlmMessage::user("first"),
            LlmMessage::assistant("second"),
        ];
        let db_ids = vec![10, 20, 30];

        let delta = crate::session::history_delta_since_snapshot(&history, &db_ids, Some(20))
            .expect("anchor should be valid");
        assert_eq!(delta, vec![LlmMessage::assistant("second")]);
    }

    #[test]
    fn history_delta_since_snapshot_returns_none_for_stale_anchor() {
        let history = vec![LlmMessage::user("only")];
        let db_ids = vec![10];
        // Anchor 999 does not exist among message IDs
        let result = crate::session::history_delta_since_snapshot(&history, &db_ids, Some(999));
        assert!(result.is_none(), "stale anchor should return None");
    }

    #[test]
    fn history_delta_since_snapshot_empty_anchor_replays_all() {
        let history = vec![LlmMessage::user("a"), LlmMessage::assistant("b")];
        let db_ids = vec![1, 2];
        let delta = crate::session::history_delta_since_snapshot(&history, &db_ids, None)
            .expect("None anchor replays all");
        assert_eq!(delta.len(), 2);
    }

    #[test]
    fn parse_persisted_context_snapshot_reads_last_message_db_id() {
        let snapshot = crate::session::parse_persisted_context_snapshot(
            crate::db::StoredSessionContextSnapshot {
                checkpoint: aither_agent::AgentCheckpoint {
                    context: aither_agent::ContextCheckpoint::new(None, Vec::new()),
                    tool_surface_hash: "abc123".to_string(),
                    context_window: aither_agent::ContextWindowSnapshot {
                        phase: aither_agent::ContextWindowPhase::Stable,
                        metrics: aither_agent::ContextWindowMetrics {
                            usage_fraction: 0.0,
                            selected_model_context_window: None,
                            fast_model_context_window: None,
                            effective_context_window: 0,
                            system_block_count: 0,
                            reminder_count: 0,
                            recent_message_count: 0,
                            recent_system_message_count: 0,
                            has_handoff: false,
                        },
                        messages: Vec::new(),
                    },
                    has_background_tasks: false,
                },
                last_message_db_id: Some(42),
                updated_at: "2026-03-14T00:00:00Z".to_string(),
            },
        );

        assert_eq!(snapshot.last_message_db_id, Some(42));
        assert!(snapshot.checkpoint.context.is_empty());
    }

    /// aither's `OpenAI` adapter emits `ToolCallDelta` where `arguments_fragment`
    /// is the CUMULATIVE argument string so far (see `aither/openai/src/client.rs`
    /// line 1099–1104). `normalize_tool_delta_for_sink` MUST decompose these
    /// cumulative snapshots into true incremental suffixes so the frontend can
    /// concatenate them into valid JSON. A regression here would drop the
    /// opening `{"` and break every tool summary.
    #[test]
    fn cumulative_tool_deltas_decompose_into_incremental_suffixes() {
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

        // First real cumulative snapshot carries `{"description`.
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

        // Next cumulative: previous prefix is extended — emit only the suffix.
        let e3 = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"description\":\"Get",
        );
        assert_eq!(
            e3,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "\":\"Get".to_string(),
            }
        );

        // Re-sending the same cumulative string is a no-op (empty fragment).
        let e4 = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"description\":\"Get",
        );
        assert_eq!(
            e4,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: String::new(),
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
    fn accumulated_argument_snapshots_are_emitted_as_suffixes() {
        let mut pending = HashMap::new();

        let first = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"",
        );
        assert!(matches!(first, super::SinkToolEvent::Start { .. }));

        let second = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date",
        );
        assert_eq!(
            second,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: "date".to_string(),
            }
        );

        let duplicate = super::normalize_tool_delta_for_sink(
            &mut pending,
            "tool-1",
            "terminal",
            "{\"command\":\"date",
        );
        assert_eq!(
            duplicate,
            super::SinkToolEvent::Delta {
                id: "tool-1".to_string(),
                name: "terminal".to_string(),
                arguments_fragment: String::new(),
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
}
