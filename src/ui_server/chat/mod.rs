mod tool_dispatch;

use self::tool_dispatch::{
    handle_browser_request, handle_notify_request, handle_register_skills_request,
    handle_register_subagents_request, handle_task_request,
};

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::Result as SkyResult;
use skyzen::responder::Sse;
use skyzen::utils::{Json, State};

use aither_agent::{BrowserBackend, StaticCdpBrowser};
use aither_core::llm::{Message as LlmMessage, Role as LlmRole, ToolResult, Usage};
use aither_models::lookup as lookup_model_info;
use aither_sandbox::{
    BollardContainerExec, ContainerExecHandle, ContainerLaunchSpec, Content, OutputEntry,
    TERMINAL_STDIN_BLOCKED_NOTICE, TerminalResult,
};

use crate::agent::MayAgent;
use crate::agent::browser_tool::BrowserToolError;
use crate::ask_user;
use crate::browser_service::persist_browser_page_capture;
use crate::notification::{NotificationPayload, NotificationService};
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
use super::error::{internal_error, json_ok_response, typed_error_response};
use super::error_chain::error_chain_text;
use super::helpers::{
    RETRY_BACKOFF_MS, append_history_message, elapsed_ms_saturating, load_session_paths,
    send_event, truncate_preview,
};
use super::model_resolution::resolve_default_model;
use super::state::{UiServerState, UiToolQueues};
use super::terminal_parsing::{
    TerminalTaskStatus, parse_terminal_result_payload, update_terminal_snapshot_from_tool_event,
};
use super::title_generation;
use super::types::{
    ApiErrorCode, ApiMessage, ChatRequest, ChatResponse, PendingMessageKind,
    RemovePendingMessageRequest, RequestLogEntry, RequestStats, SessionRuntimeResponse,
    SseChunkEvent, SseDoneEvent, SseErrorEvent, SseStartEvent, SseTodoUpdateEvent, SseToolEndEvent,
    SseToolStartEvent, TerminalStateSnapshot, ToolEventRecord,
};

fn stopped_terminal_tool_result() -> ToolResult {
    ToolResult::json(&TerminalResult {
        stdout: OutputEntry::Empty,
        stderr: None,
        exit_code: 130,
        task_id: None,
        status: Some("stopped".to_string()),
    })
    .expect("stopped terminal tool result must serialize")
}

// ── ChatSink trait ──────────────────────────────────────────

/// Abstraction over how chat events are delivered to the caller.
/// `SseSink` streams events to the browser; `CollectSink` is a no-op
/// (data is accumulated in the shared loop and returned as a result).
trait ChatSink {
    async fn on_start(&mut self, session_id: &str, assistant_id: &str) -> bool;
    async fn on_text(&mut self, chunk: &str) -> bool;
    async fn on_reasoning(&mut self, chunk: &str) -> bool;
    async fn on_tool_start(&mut self, id: &str, name: &str, arguments: &str) -> bool;
    async fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &ToolResult,
    ) -> bool;
    async fn on_todo_update(&mut self, event: &SseTodoUpdateEvent) -> bool;
    async fn on_error(&mut self, message: &str) -> bool;
}

struct SseSink(skyzen::responder::sse::Sender);

impl ChatSink for SseSink {
    async fn on_start(&mut self, session_id: &str, assistant_id: &str) -> bool {
        send_event(
            &self.0,
            "start",
            &SseStartEvent {
                session_id,
                assistant_id,
            },
        )
        .await
    }

    async fn on_text(&mut self, chunk: &str) -> bool {
        send_event(&self.0, "text", &SseChunkEvent { chunk }).await
    }

    async fn on_reasoning(&mut self, chunk: &str) -> bool {
        send_event(&self.0, "reasoning", &SseChunkEvent { chunk }).await
    }

    async fn on_tool_start(&mut self, id: &str, name: &str, arguments: &str) -> bool {
        send_event(
            &self.0,
            "tool_start",
            &SseToolStartEvent {
                id,
                name,
                arguments,
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
    ) -> bool {
        send_event(
            &self.0,
            "tool_end",
            &SseToolEndEvent {
                id,
                name,
                arguments,
                result,
            },
        )
        .await
    }

    async fn on_todo_update(&mut self, event: &SseTodoUpdateEvent) -> bool {
        send_event(&self.0, "todo_update", event).await
    }

    async fn on_error(&mut self, message: &str) -> bool {
        send_event(&self.0, "error", &SseErrorEvent { message }).await
    }
}

/// No-op sink for non-streaming collect mode.
struct CollectSink;

impl ChatSink for CollectSink {
    async fn on_start(&mut self, _: &str, _: &str) -> bool {
        true
    }
    async fn on_text(&mut self, _: &str) -> bool {
        true
    }
    async fn on_reasoning(&mut self, _: &str) -> bool {
        true
    }
    async fn on_tool_start(&mut self, _: &str, _: &str, _: &str) -> bool {
        true
    }
    async fn on_tool_end(&mut self, _: &str, _: &str, _: Option<&str>, _: &ToolResult) -> bool {
        true
    }
    async fn on_todo_update(&mut self, _: &SseTodoUpdateEvent) -> bool {
        true
    }
    async fn on_error(&mut self, _: &str) -> bool {
        true
    }
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
    async fn try_new(state: Arc<UiServerState>, session_id: impl Into<String>) -> Option<Self> {
        let session_id = session_id.into();
        let claimed = state
            .db()
            .claim_session_run_lease(session_id.as_str())
            .await
            .ok()?;
        if !claimed {
            return None;
        }
        let Some(receiver) = state.run_cancellation_registry.try_open(session_id.clone()) else {
            let _ = state
                .db()
                .release_session_run_lease(session_id.as_str())
                .await;
            return None;
        };
        Some(Self {
            state,
            session_id,
            receiver,
        })
    }

    const fn receiver(&self) -> &async_channel::Receiver<()> {
        &self.receiver
    }

    async fn close(&self) {
        let _ = self
            .state
            .db()
            .release_session_run_lease(self.session_id.as_str())
            .await;
        self.state.run_cancellation_registry.close(&self.session_id);
    }
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
        .await
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
        let Some(run_registration) =
            SessionRunRegistration::try_new(state.clone(), session_id.clone()).await
        else {
            return;
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
                run_chat_collect_with_receiver(state.clone(), request, true, true, run_registration.receiver()).await
            {
                tracing::warn!(session_id = %session_id, error = %error, "follow-up run failed");
                break;
            }
        }

        run_registration.close().await;
        let _ = clear_running_state_if_needed(&state, &session_id).await;
    });
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

#[derive(Serialize)]
struct BrowserTakeoverAlertXml {
    #[serde(rename = "@type")]
    alert_type: String,
    event: String,
    instruction: String,
}

#[derive(Serialize)]
struct ResumeWaitingTurnXml<'a> {
    #[serde(rename = "@type")]
    alert_type: &'a str,
    request_kind: ResumeRequestKind,
    resolution: ResumeResolution,
    instruction: &'a str,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResumeRequestKind {
    Permission,
    AskUser,
    Workspace,
}

#[derive(Clone, Copy, Serialize)]
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

pub(super) async fn chat(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let tokio_handle = state.tokio_handle.clone();
    let result = tokio_handle
        .spawn(async move { run_chat_collect(state, req).await })
        .await
        .map_err(|_| internal_error("chat task panicked before producing a response"))?;
    match result {
        Ok(response) => json_ok_response(&response),
        Err(ChatCollectError::Preparation(error)) => Ok(typed_error_response(
            error.status(),
            error.api_code(),
            error,
        )),
        Err(ChatCollectError::Sky(error)) => Err(error),
    }
}

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

    let Some(run_registration) =
        SessionRunRegistration::try_new(state.clone(), req.session_id.clone()).await
    else {
        return Err(active_session_conflict_error());
    };
    let (sender, sse) = Sse::channel();

    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        if let Err(err) =
            stream_chat_events(state.clone(), req, sender.clone(), run_registration).await
        {
            send_event(
                &sender,
                "error",
                &SseErrorEvent {
                    message: &err.to_string(),
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
            let result = handle_task_request(&task_state, request.args.clone()).await;
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
async fn run_agent_loop(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    sink: &mut impl ChatSink,
    cancel_rx: &async_channel::Receiver<()>,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
) -> Result<AgentRunResult, AgentLoopError> {
    let mut attempt = 0usize;
    let mut max_retries = 0usize;
    let mut turn_hidden_context_xml: Option<String> = None;

    loop {
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
                let _ = sink.on_error(&error.to_string()).await;
                return Err(error.into());
            }
        };
        if attempt == 0 {
            max_retries = provider_candidate_count.saturating_sub(1);
        }
        if let Some(hidden_context_xml) = hidden_context_xml {
            turn_hidden_context_xml = Some(hidden_context_xml);
        }
        let model_context_window = agent.model_context_window().or_else(|| {
            lookup_model_info(&current_model).and_then(aither_models::ModelEntry::max_input_tokens)
        });

        let assistant_uuid = uuid::Uuid::new_v4().to_string();
        let assistant_id = format!("assistant-{assistant_uuid}");
        let started_at = Instant::now();
        let mut usage = Usage::default();
        let mut segment_text = String::new(); // current text segment, flushed on tool calls
        let mut full_text = String::new(); // accumulated full assistant text
        let mut reasoning_text = String::new();
        let mut terminal_snapshot = TerminalStateSnapshot::default();
        let mut saw_tool_activity = false;
        let mut tool_arguments_by_id: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_todo_hash: Option<u64> = None;
        let tasks_md_path = agent.sandbox_dir().join("tasks.md");

        if !sink.on_start(&req.session_id, &assistant_id).await {
            return Err(AgentLoopError::ClientDisconnectedBeforeStreamStart);
        }

        let mut terminal_error: Option<aither_agent::AgentError> = None;
        let mut cancellation_reason: Option<&'static str> = None;
        let mut browser_takeover_paused = false;
        {
            agent.sync_runtime_context().await;
            let mut stream = std::pin::pin!(agent.run_with_hidden_context(
                &prompt,
                turn_hidden_context_xml.as_deref(),
                attachments,
            ));
            loop {
                let next_event = tokio::select! {
                    biased;
                    cancel = cancel_rx.recv() => {
                        if let Ok(()) = cancel {
                            cancellation_reason = Some("run cancelled by user");
                            None
                        } else {
                            cancellation_reason = Some("run cancellation channel closed unexpectedly");
                            None
                        }
                    }
                    next = stream.next() => next,
                };
                let Some(event) = next_event else {
                    break;
                };
                match event {
                    Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                        reasoning_text.push_str(&text);
                        if !sink.on_reasoning(&text).await {
                            cancellation_reason =
                                Some("client disconnected during reasoning stream");
                            break;
                        }
                    }
                    Ok(aither_agent::AgentEvent::Text(text)) => {
                        segment_text.push_str(&text);
                        full_text.push_str(&text);
                        if !sink.on_text(&text).await {
                            cancellation_reason = Some("client disconnected during text stream");
                            break;
                        }
                    }
                    Ok(aither_agent::AgentEvent::Usage(u)) => {
                        usage = u;
                    }
                    Ok(aither_agent::AgentEvent::ToolCallStart {
                        id,
                        name,
                        arguments,
                    }) => {
                        saw_tool_activity = true;
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
                        // Flush accumulated text as a history message before the tool call.
                        if !segment_text.trim().is_empty() {
                            append_history_message(
                                state,
                                &session_id,
                                ChatMessage::assistant(segment_text.clone()),
                            )
                            .await?;
                            segment_text.clear();
                        }

                        let record = ToolEventRecord::Start {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        };
                        if let Ok(serialized) = serde_json::to_string(&record) {
                            append_history_message(
                                state,
                                &session_id,
                                ChatMessage::tool(id.clone(), serialized),
                            )
                            .await?;
                        }
                        update_terminal_snapshot_from_tool_event(&mut terminal_snapshot, &record);
                        tool_arguments_by_id.insert(id.clone(), arguments.clone());
                        if !sink.on_tool_start(&id, &name, &arguments).await {
                            cancellation_reason = Some("client disconnected during tool stream");
                            break;
                        }
                    }
                    Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
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
                        if let Ok(serialized) = serde_json::to_string(&record) {
                            append_history_message(
                                state,
                                &session_id,
                                ChatMessage::tool(id.clone(), serialized),
                            )
                            .await?;
                        }
                        update_terminal_snapshot_from_tool_event(&mut terminal_snapshot, &record);
                        let arguments = tool_arguments_by_id.remove(&id);
                        if !sink
                            .on_tool_end(&id, &name, arguments.as_deref(), &result)
                            .await
                        {
                            cancellation_reason = Some("client disconnected during tool stream");
                            break;
                        }

                        // Check if tasks.md changed after tool execution.
                        if let Some(event) =
                            check_tasks_update(&tasks_md_path, &mut last_todo_hash).await
                            && !sink.on_todo_update(&event).await
                        {
                            cancellation_reason = Some("client disconnected during todo update");
                            break;
                        }
                    }
                    Ok(aither_agent::AgentEvent::BackgroundTaskStarted {
                        task_id,
                        output_preview,
                        output_file,
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

            if !segment_text.trim().is_empty() {
                append_history_message(
                    state,
                    &session_id,
                    ChatMessage::assistant(segment_text.clone()),
                )
                .await?;
            }

            clear_terminal_runtime_state(state, session_id.as_str()).await?;

            let has_assistant_text = !full_text.trim().is_empty();
            let has_terminal_output = !terminal_snapshot.executions.is_empty();
            let has_tool_activity = has_terminal_output || saw_tool_activity;
            if !has_assistant_text && has_tool_activity {
                append_history_message(state, &session_id, ChatMessage::assistant(String::new()))
                    .await?;
            }

            record_request_log(
                state,
                log_base.failure(elapsed_ms_saturating(&started_at), TAKEOVER_PAUSED_MESSAGE),
            )
            .await;
            schedule_browser_takeover_resume(state, session_id.as_str()).await?;

            return Err(AgentLoopError::BrowserTakeoverPaused(AgentRunResult {
                assistant_id,
                full_text,
                reasoning_text,
                terminal_snapshot,
                usage,
                provider_id,
                model_context_window,
                elapsed_ms: elapsed_ms_saturating(&started_at),
            }));
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
            let _ = sink.on_error(&err_msg).await;
            return Err(err.into());
        }

        if let Some(reason) = cancellation_reason {
            if reason == "run cancelled by user" {
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
                    if let Ok(serialized) = serde_json::to_string(&record) {
                        append_history_message(
                            state,
                            &session_id,
                            ChatMessage::tool(call_id, serialized),
                        )
                        .await?;
                    }
                    update_terminal_snapshot_from_tool_event(&mut terminal_snapshot, &record);
                }
            }
            if !segment_text.trim().is_empty() {
                append_history_message(
                    state,
                    &session_id,
                    ChatMessage::assistant(segment_text.clone()),
                )
                .await?;
            }
            record_request_log(
                state,
                log_base.failure(elapsed_ms_saturating(&started_at), reason),
            )
            .await;
            return Err(AgentLoopError::Cancelled(reason));
        }

        // Flush remaining text segment to history.
        if !segment_text.trim().is_empty() {
            append_history_message(
                state,
                &session_id,
                ChatMessage::assistant(segment_text.clone()),
            )
            .await?;
        }

        clear_terminal_runtime_state(state, session_id.as_str()).await?;

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
            let _ = sink.on_error(err).await;
            return Err(AgentLoopError::EmptyAssistantResponse);
        }
        if !has_assistant_text && has_tool_activity {
            append_history_message(state, &session_id, ChatMessage::assistant(String::new()))
                .await?;
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
}

async fn stream_chat_events(
    state: Arc<UiServerState>,
    req: ChatRequest,
    sender: skyzen::responder::sse::Sender,
    run_registration: SessionRunRegistration,
) -> Result<(), crate::db::ChatDbError> {
    set_session_runtime_state(&state, &req.session_id, SessionRuntimeState::Running).await?;
    let mut sink = SseSink(sender.clone());
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
                session_id: response_session_id.clone(),
                assistant: api_message_from_run_result(result),
            };
            send_event(&sender, "done", &done_event).await;
            run_registration.close().await;
            clear_running_state_if_needed(&state, &response_session_id).await?;
            spawn_follow_up_runs(state.clone(), response_session_id.clone());
            Ok(())
        }
        Err(AgentLoopError::BrowserTakeoverPaused(result)) => {
            let done_event = SseDoneEvent {
                session_id: response_session_id.clone(),
                assistant: api_message_from_run_result(result),
            };
            send_event(&sender, "paused", &done_event).await;
            run_registration.close().await;
            clear_running_state_if_needed(&state, &response_session_id).await?;
            Ok(())
        }
        // Errors are already sent to the sink via on_error in run_agent_loop.
        Err(_) => {
            run_registration.close().await;
            clear_running_state_if_needed(&state, &response_session_id).await?;
            Ok(())
        }
    }
}

async fn run_chat_collect_with_persistence(
    state: Arc<UiServerState>,
    req: ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
) -> Result<ChatResponse, ChatCollectError> {
    let Some(run_registration) =
        SessionRunRegistration::try_new(state.clone(), req.session_id.clone()).await
    else {
        return Err(ChatCollectError::Sky(active_session_conflict_error()));
    };
    let response = run_chat_collect_with_receiver(
        state.clone(),
        req.clone(),
        persist_user_message,
        use_persisted_context_snapshot,
        run_registration.receiver(),
    )
    .await;
    run_registration.close().await;
    match response {
        Ok(response) => {
            spawn_follow_up_runs(state.clone(), req.session_id.clone());
            Ok(response)
        }
        Err(error) => Err(error),
    }
}

async fn run_chat_collect_with_receiver(
    state: Arc<UiServerState>,
    req: ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    cancel_rx: &async_channel::Receiver<()>,
) -> Result<ChatResponse, ChatCollectError> {
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
    match result {
        Ok(result) => {
            clear_running_state_if_needed(&state, &req.session_id)
                .await
                .map_err(internal_error)
                .map_err(ChatCollectError::from)?;

            Ok(ChatResponse {
                session_id: req.session_id,
                assistant: api_message_from_run_result(result),
            })
        }
        Err(AgentLoopError::BrowserTakeoverPaused(result)) => {
            clear_running_state_if_needed(&state, &req.session_id)
                .await
                .map_err(internal_error)
                .map_err(ChatCollectError::from)?;
            Ok(ChatResponse {
                session_id: req.session_id,
                assistant: api_message_from_run_result(result),
            })
        }
        Err(error) => {
            clear_running_state_if_needed(&state, &req.session_id)
                .await
                .map_err(internal_error)
                .map_err(ChatCollectError::from)?;
            match error {
                AgentLoopError::Preparation(error) => Err(ChatCollectError::Preparation(error)),
                other => Err(ChatCollectError::Sky(internal_error(other))),
            }
        }
    }
}

async fn run_chat_collect(
    state: Arc<UiServerState>,
    req: ChatRequest,
) -> Result<ChatResponse, ChatCollectError> {
    run_chat_collect_with_persistence(state, req, true, true).await
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
            .map(url::Url::to_string)
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
    prompt_attachments: &[url::Url],
    assembled_messages: &[LlmMessage],
) -> Result<(), crate::db::ChatDbError> {
    let prompt_attachments = prompt_attachments
        .iter()
        .map(url::Url::to_string)
        .collect::<Vec<_>>();
    let assembled_messages = assembled_messages
        .iter()
        .map(map_context_debug_message)
        .collect::<Vec<_>>();
    let timestamp = jiff::Timestamp::now().to_string();

    let db = state.db();
    db.upsert_context_debug_snapshot(
        session_id.as_str(),
        &timestamp,
        model,
        provider_id,
        prompt,
        &prompt_attachments,
        &assembled_messages,
    )
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
}

struct PreparedAgent {
    session_id: SessionId,
    agent: MayAgent,
    current_model: String,
    provider_id: String,
    profile_id: Option<String>,
    hidden_context_xml: Option<String>,
    prompt: String,
    attachments: Vec<url::Url>,
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
    #[error("invalid container profile configuration: {0}")]
    InvalidContainerProfileConfiguration(#[source] crate::container::ContainerError),
    #[error("invalid container mount configuration: {0}")]
    InvalidContainerMountConfiguration(#[source] crate::container::ContainerError),
    #[error("container image build failed: {0}")]
    EnsureContainerImage(#[source] crate::container::ContainerError),
    #[error("container start failed: {0}")]
    StartContainer(#[source] crate::container::ContainerError),
    #[error("remote profile model is missing")]
    MissingRemoteProfileModel,
    #[error("{0}")]
    ResolveRemoteServer(String),
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
    fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::BrowserTakeoverActive => ApiErrorCode::BrowserTakeoverActive,
            Self::SessionNotFound(_) => ApiErrorCode::SessionNotFound,
            Self::MissingModelConfiguration => ApiErrorCode::ModelNotConfigured,
            Self::NoProviderAvailable { .. } => ApiErrorCode::ProviderUnavailable,
            Self::InvalidFileMentionPath(_) => ApiErrorCode::BadRequest,
            Self::MissingContainerProfileModel
            | Self::MissingRemoteProfileModel
            | Self::ResolveRemoteServer(_)
            | Self::ParseProfileEnvironment(_)
            | Self::InvalidContainerImageTag(_)
            | Self::InvalidContainerProfileConfiguration(_)
            | Self::InvalidContainerMountConfiguration(_) => {
                ApiErrorCode::ProfileConfigurationInvalid
            }
            Self::ResolveContainerRuntime(_) => ApiErrorCode::ContainerRuntimeUnavailable,
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

    fn status(&self) -> skyzen::StatusCode {
        match self {
            Self::BrowserTakeoverActive => skyzen::StatusCode::CONFLICT,
            Self::SessionNotFound(_) => skyzen::StatusCode::NOT_FOUND,
            Self::MissingModelConfiguration
            | Self::NoProviderAvailable { .. }
            | Self::InvalidFileMentionPath(_)
            | Self::MissingContainerProfileModel
            | Self::MissingRemoteProfileModel
            | Self::ResolveRemoteServer(_)
            | Self::ParseProfileEnvironment(_)
            | Self::InvalidContainerImageTag(_)
            | Self::InvalidContainerProfileConfiguration(_)
            | Self::InvalidContainerMountConfiguration(_)
            | Self::ResolveContainerRuntime(_)
            | Self::EnsureContainerImage(_)
            | Self::StartContainer(_)
            | Self::LoadLocation(_) => skyzen::StatusCode::BAD_REQUEST,
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
            | Self::HistoryReplay(_) => skyzen::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ChatMarkupError {
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
    BrowserTakeoverPaused(AgentRunResult),
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
    if persist_user_message {
        if take_over_alert_pending(state, &session_id).await {
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
            db.update_session(session_id.as_str(), Some(title.as_str()), None)
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
    })
}

fn session_paths_for_session(
    state: &UiServerState,
    profile_id: Option<&str>,
    session_id: &SessionId,
) -> SessionPaths {
    let home_paths = MayHomePaths::new(state.may_dir.clone());
    match profile_id {
        Some(profile_id) => home_paths
            .profile_home(profile_id)
            .session_paths(session_id.as_str()),
        None => home_paths.session_paths(session_id.as_str()),
    }
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
            let default_image_tag = crate::container::profile::default_image_tag(
                &state.may_dir,
                profile,
                container_preferences.clone(),
            )
            .await
            .map_err(UiChatPreparationError::InvalidContainerImageTag)?;

            let image_spec = crate::container::profile::image_spec_for_profile(
                &state.may_dir,
                profile,
                &default_image_tag,
                container_preferences.clone(),
            )
            .map_err(UiChatPreparationError::InvalidContainerProfileConfiguration)?;
            let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences)
                .map_err(UiChatPreparationError::InvalidContainerMountConfiguration)?;

            crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
                .await
                .map_err(UiChatPreparationError::EnsureContainerImage)?;

            let container_name = crate::container::profile::container_name_for_profile_session(
                profile,
                &ctx.session_id,
            );
            let host_gateway = container_manager.runtime().host_gateway();

            let mut extra_env = Vec::new();
            let browser_backend: Option<Arc<dyn BrowserBackend>> = if let Some(bm) =
                state.browser_manager()
            {
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
                    Some(Arc::new(StaticCdpBrowser::new(endpoint)))
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

            let container_ipc_host = host_gateway.to_string();
            let container_exec =
                ContainerExecHandle::new(std::sync::Arc::new(BollardContainerExec::new(
                    std::sync::Arc::new(container_manager.runtime().client.clone()),
                )));

            crate::agent::AgentEnvironment::Container {
                container_id,
                container_ipc_host,
                container_exec,
                browser_backend,
            }
        }
        Some(crate::profile::ProfileEnvironment::Remote { .. }) => {
            let profile = ctx
                .profile_model
                .as_ref()
                .ok_or(UiChatPreparationError::MissingRemoteProfileModel)?;
            let config = state.config_snapshot();
            let ssh_server =
                crate::profile::remote::resolve_remote_profile_server(profile, &config.ssh_servers)
                    .map_err(UiChatPreparationError::ResolveRemoteServer)?;
            crate::agent::AgentEnvironment::Remote { ssh_server }
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

async fn prepare_agent_for_request(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
    provider_attempt: usize,
) -> Result<PreparedAgent, UiChatPreparationError> {
    let ctx = resolve_session_context(
        state,
        req,
        persist_user_message,
        use_persisted_context_snapshot,
    )
    .await?;
    let (environment, location) = setup_agent_environment(state, &ctx).await?;
    let brokers = build_tool_brokers(state, &ctx.session_id);

    let config = state.config_snapshot();
    let mut agent = MayAgent::from_config_with_model_and_permissions(
        &config,
        ctx.sandbox_dir,
        state.may_dir.clone(),
        state.cwd.clone(),
        ctx.session_id.clone(),
        brokers.workspace,
        brokers.ask_user,
        &ctx.current_model,
        None,
        ctx.profile_id.clone(),
        Some(
            state
                .permission_approver
                .for_session(ctx.session_id.as_str().to_string()),
        ),
        ctx.profile_persona,
        ctx.profile_memories,
        ctx.plan_state,
        location,
        Some(brokers.task),
        Some(brokers.notify),
        state.browser_manager().map(|_| brokers.browser),
        Some(brokers.register_skills),
        Some(brokers.register_subagents),
        environment,
        provider_attempt,
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
        match crate::session::history_delta_since_snapshot(
            &ctx.history_messages,
            &db_ids,
            snapshot.last_message_db_id,
        ) {
            Some(delta) => {
                agent
                    .restore_checkpoint(snapshot.checkpoint.clone())
                    .map_err(UiChatPreparationError::RestoreCheckpoint)?;
                for message in delta {
                    agent.push_message(message);
                }
            }
            None => {
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
        }
    } else {
        for message in ctx.history_messages {
            agent.push_message(message);
        }
    }

    let provider_id = agent.provider_id().to_string();

    let (prompt, attachments) = build_prompt_and_attachments(state, req, &ctx.current_model)
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
}

/// Read tasks.md and return an SSE event if its content changed since the last check.
async fn check_tasks_update(
    path: &std::path::Path,
    last_hash: &mut Option<u64>,
) -> Option<SseTodoUpdateEvent> {
    use std::hash::{Hash, Hasher};

    let content = async_fs::read_to_string(path).await.ok()?;
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
    Some(SseTodoUpdateEvent::from(&checklist))
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

    #[test]
    fn history_delta_since_snapshot_returns_suffix_after_anchor_id() {
        let history = vec![
            LlmMessage::system("memory"),
            LlmMessage::user("first"),
            LlmMessage::assistant("second"),
        ];
        let db_ids = vec![10, 20, 30];

        let delta =
            crate::session::history_delta_since_snapshot(&history, &db_ids, Some(20))
                .expect("anchor should be valid");
        assert_eq!(delta, vec![LlmMessage::assistant("second")]);
    }

    #[test]
    fn history_delta_since_snapshot_returns_none_for_stale_anchor() {
        let history = vec![LlmMessage::user("only")];
        let db_ids = vec![10];
        // Anchor 999 does not exist among message IDs
        let result =
            crate::session::history_delta_since_snapshot(&history, &db_ids, Some(999));
        assert!(result.is_none(), "stale anchor should return None");
    }

    #[test]
    fn history_delta_since_snapshot_empty_anchor_replays_all() {
        let history = vec![LlmMessage::user("a"), LlmMessage::assistant("b")];
        let db_ids = vec![1, 2];
        let delta =
            crate::session::history_delta_since_snapshot(&history, &db_ids, None)
                .expect("None anchor replays all");
        assert_eq!(delta.len(), 2);
    }

    #[test]
    fn parse_persisted_context_snapshot_reads_last_message_db_id() {
        let snapshot = crate::session::parse_persisted_context_snapshot(
            crate::db::StoredSessionContextSnapshot {
                checkpoint: aither_agent::AgentCheckpoint {
                    context: aither_agent::ContextCheckpoint::new(None, Vec::new()),
                    todo_items: Vec::new(),
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
}
