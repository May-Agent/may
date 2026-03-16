use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::Result as SkyResult;
use skyzen::responder::Sse;
use skyzen::utils::{Json, State};

use aither_agent::{BrowserBackend, StaticCdpBrowser};
use aither_core::llm::{Message as LlmMessage, Role as LlmRole, Usage};
use aither_models::lookup as lookup_model_info;
use aither_sandbox::{
    BashResult, BollardContainerExec, CONTAINER_STDIN_BLOCKED_NOTICE, ContainerExecHandle,
    ContainerLaunchSpec, Content, OutputEntry,
};

use crate::agent::MayAgent;
use crate::ask_user;
use crate::notification::NotificationPayload;
use crate::notification_service::NotificationService;
use crate::session::{
    ChatMessage, ChatMessageStats, MayHomePaths, MessageRole, PendingUserMessage,
    PersistedAttachment, SessionId, SessionRuntimeState, SessionStore,
};
use crate::task_trigger::EVENT_CHAT_TURN_COMPLETED;

use super::attachments::build_prompt_and_attachments;
use super::bash_parsing::update_bash_snapshot_from_tool_event;
use super::error::internal_error;
use super::helpers::{
    RETRY_BACKOFF_MS, append_history_message, elapsed_ms_saturating, load_session_paths,
    send_event, truncate_preview,
};
use super::model_resolution::resolve_default_model;
use super::state::{UiServerState, UiToolQueues};
use super::title_generation;
use super::types::{
    ApiMessage, BashStateSnapshot, ChatRequest, ChatResponse, ContextDebugMessage,
    PendingMessageKind, RemovePendingMessageRequest, RequestLogEntry, RequestStats,
    SessionRuntimeResponse, SseChunkEvent, SseDoneEvent, SseErrorEvent, SseStartEvent,
    SseTodoUpdateEvent, SseToolEndEvent, SseToolStartEvent, ToolEventRecord,
};

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
        result: &Result<String, String>,
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
        result: &Result<String, String>,
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
    async fn on_tool_end(
        &mut self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &Result<String, String>,
    ) -> bool {
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
struct AgentRunResult {
    assistant_id: String,
    full_text: String,
    reasoning_text: String,
    bash_snapshot: BashStateSnapshot,
    usage: Usage,
    provider_id: String,
    model_context_window: Option<u32>,
    elapsed_ms: u64,
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

    fn receiver(&self) -> &async_channel::Receiver<()> {
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

fn join_text(parts: &[&str]) -> String {
    let capacity = parts.iter().map(|part| part.len()).sum();
    let mut text = String::with_capacity(capacity);
    for part in parts {
        text.push_str(part);
    }
    text
}

fn prefixed_text(prefix: &str, suffix: &str) -> String {
    let mut text = String::with_capacity(prefix.len() + suffix.len());
    text.push_str(prefix);
    text.push_str(suffix);
    text
}

fn path_error_text(prefix: &str, path: &std::path::Path, error: &impl std::fmt::Display) -> String {
    let path_text = path.display().to_string();
    let error_text = error.to_string();
    join_text(&[prefix, path_text.as_str(), ": ", error_text.as_str()])
}

fn path_message(prefix: &str, path: &std::path::Path) -> String {
    let path_text = path.display().to_string();
    join_text(&[prefix, path_text.as_str()])
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
) -> anyhow::Result<()> {
    state
        .runtime_service
        .set_state(session_id, runtime_state)
        .await
        .map_err(anyhow::Error::from)
}

async fn clear_running_state_if_needed(
    state: &Arc<UiServerState>,
    session_id: &str,
) -> anyhow::Result<()> {
    state
        .runtime_service
        .clear_running_if_needed(session_id)
        .await
        .map_err(anyhow::Error::from)
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
        .is_some_and(|text| text.starts_with(CONTAINER_STDIN_BLOCKED_NOTICE))
}

fn result_waits_for_terminal_input(result: &Result<String, String>) -> bool {
    let Ok(serialized) = result else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<BashResult>(serialized) else {
        return false;
    };
    parsed.status.as_deref() == Some("running")
        && output_contains_terminal_wait_notice(&parsed.stdout)
}

fn spawn_follow_up_runs(state: Arc<UiServerState>, session_id: String) {
    tokio::spawn(async move {
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
                set_session_runtime_state(&state, &session_id, SessionRuntimeState::Running).await
            {
                tracing::warn!(session_id = %session_id, error = %error, "failed to mark follow-up run as running");
                break;
            }

            if let Err(error) = run_chat_collect(state.clone(), request).await {
                tracing::warn!(session_id = %session_id, error = %error, "follow-up run failed");
                break;
            }
        }

        run_registration.close().await;
        let _ = clear_running_state_if_needed(&state, &session_id).await;
    });
}

pub(super) async fn resume_pending_follow_up_runs(state: Arc<UiServerState>) -> anyhow::Result<()> {
    let session_ids = state
        .db()
        .sessions_requiring_pending_dispatch()
        .await
        .map_err(anyhow::Error::from)?;
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
    request_kind: &'a str,
    resolution: &'a str,
    instruction: &'a str,
}

fn serialize_xml<T: Serialize>(tag: &str, value: &T) -> anyhow::Result<String> {
    crate::xml_fragment::serialize_tagged_xml(tag, value).map_err(anyhow::Error::from)
}

fn browser_takeover_alert_xml() -> anyhow::Result<String> {
    serialize_xml(
        "system_alert",
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
    request_kind: &'static str,
    resolution: &'static str,
) -> anyhow::Result<crate::XmlFragment> {
    let xml = serialize_xml(
        "system_alert",
        &ResumeWaitingTurnXml {
            alert_type: "resume_waiting_turn",
            request_kind,
            resolution,
            instruction: "The previous run was interrupted while waiting for this response. Reassess the latest state, use the recorded response, and continue from the most sensible next action.",
        },
    )?;
    crate::XmlFragment::parse(xml).map_err(anyhow::Error::from)
}

pub(super) async fn schedule_waiting_turn_resume(
    state: &Arc<UiServerState>,
    session_id: &str,
    request_kind: &'static str,
    resolution: &'static str,
) -> anyhow::Result<crate::session::SessionRuntimeSummary> {
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
        .map_err(anyhow::Error::from)?;
    spawn_follow_up_runs(state.clone(), session_id.to_string());
    Ok(runtime)
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
    state.notify_ui_events();
}

pub(super) async fn chat(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ChatResponse>> {
    let response = run_chat_collect(state, req).await?;
    Ok(Json(response))
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
        return Err(skyzen::Error::msg(join_text(&[
            "Session ",
            session_id.as_str(),
            " not found",
        ]))
        .set_status(skyzen::StatusCode::NOT_FOUND));
    }

    let persisted_cancel = state
        .db()
        .request_session_run_cancel(&session_id)
        .await
        .map_err(internal_error)?;
    let cancelled = state.run_cancellation_registry.cancel(&session_id) || persisted_cancel;
    tracing::info!(session_id = %session_id, cancelled, "received session stop request");

    let runtime = state
        .runtime_service
        .get(&session_id)
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

    tokio::spawn(async move {
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
        register_skills,
        register_subagents,
    } = queues;

    let task_state = state.clone();
    tokio::spawn(async move {
        while let Some(request) = task.next().await {
            let result = handle_task_request(&task_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let notification_service = state.notification_service.clone();
    tokio::spawn(async move {
        while let Some(request) = notify.next().await {
            let result =
                handle_notify_request(notification_service.clone(), request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let register_skills_state = state.clone();
    tokio::spawn(async move {
        while let Some(request) = register_skills.next().await {
            let result =
                handle_register_skills_request(&register_skills_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    tokio::spawn(async move {
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
) -> anyhow::Result<AgentRunResult> {
    let mut attempt = 0usize;
    let mut max_retries = 0usize;

    loop {
        let PreparedAgent {
            session_id,
            mut agent,
            current_model,
            provider_id,
            profile_id,
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
                return Err(error);
            }
        };
        if attempt == 0 {
            max_retries = provider_candidate_count.saturating_sub(1);
        }
        let model_context_window = agent
            .model_context_window()
            .or_else(|| lookup_model_info(&current_model).and_then(|m| m.max_input_tokens()));

        let assistant_uuid = uuid::Uuid::new_v4().to_string();
        let assistant_id = prefixed_text("assistant-", assistant_uuid.as_str());
        let started_at = Instant::now();
        let mut usage = Usage::default();
        let mut segment_text = String::new(); // current text segment, flushed on tool calls
        let mut full_text = String::new(); // accumulated full assistant text
        let mut reasoning_text = String::new();
        let mut bash_snapshot = BashStateSnapshot::default();
        let mut saw_tool_activity = false;
        let mut tool_arguments_by_id: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_todo_hash: Option<u64> = None;
        let tasks_md_path = agent.sandbox_dir().join("tasks.md");

        if !sink.on_start(&req.session_id, &assistant_id).await {
            return Err(anyhow::anyhow!("client disconnected before stream start"));
        }

        let mut terminal_error: Option<aither_agent::AgentError> = None;
        let mut cancellation_reason: Option<&'static str> = None;
        {
            agent.sync_runtime_context().await;
            let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
            loop {
                let next_event = tokio::select! {
                    biased;
                    cancel = cancel_rx.recv() => {
                        match cancel {
                            Ok(()) => {
                                cancellation_reason = Some("run cancelled by user");
                                None
                            }
                            Err(_) => {
                                cancellation_reason = Some("run cancellation channel closed unexpectedly");
                                None
                            }
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
                            "bash" | "input_terminal" | "kill_terminal" | "read_terminal_delta"
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
                        update_bash_snapshot_from_tool_event(&mut bash_snapshot, &record);
                        tool_arguments_by_id.insert(id.clone(), arguments.clone());
                        if !sink.on_tool_start(&id, &name, &arguments).await {
                            cancellation_reason = Some("client disconnected during tool stream");
                            break;
                        }
                    }
                    Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
                        saw_tool_activity = true;
                        if name == "bash" && result_waits_for_terminal_input(&result) {
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
                        update_bash_snapshot_from_tool_event(&mut bash_snapshot, &record);
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
                    Ok(aither_agent::AgentEvent::Error(err)) | Err(err) => {
                        terminal_error = Some(err);
                        break;
                    }
                    Ok(_) => {}
                }

                if is_takeover_active_for_session(state, &session_id).await {
                    terminal_error = Some(aither_agent::AgentError::Config(
                        "Browser takeover is active for this session; model execution paused"
                            .to_string(),
                    ));
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
            return Err(anyhow::anyhow!(err));
        }

        if let Some(reason) = cancellation_reason {
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
            return Err(anyhow::anyhow!(reason));
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

        let has_assistant_text = !full_text.trim().is_empty();
        let has_bash_output = !bash_snapshot.executions.is_empty();
        let has_tool_activity = has_bash_output || saw_tool_activity;
        if !has_assistant_text && !has_tool_activity {
            let err = "Model returned an empty assistant response";
            record_request_log(
                state,
                log_base.failure(elapsed_ms_saturating(&started_at), err),
            )
            .await;
            let _ = sink.on_error(err).await;
            return Err(anyhow::anyhow!(err));
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
            bash_snapshot,
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
) -> anyhow::Result<()> {
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
                assistant: ApiMessage {
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
                    bash_executions: result.bash_snapshot.executions,
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
                },
            };
            send_event(&sender, "done", &done_event).await;
            spawn_follow_up_runs(state.clone(), response_session_id.clone());
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
) -> SkyResult<ChatResponse> {
    let Some(run_registration) =
        SessionRunRegistration::try_new(state.clone(), req.session_id.clone()).await
    else {
        return Err(active_session_conflict_error());
    };
    set_session_runtime_state(&state, &req.session_id, SessionRuntimeState::Running)
        .await
        .map_err(internal_error)?;
    let mut sink = CollectSink;
    let result = run_agent_loop(
        &state,
        &req,
        &mut sink,
        run_registration.receiver(),
        persist_user_message,
        use_persisted_context_snapshot,
    )
    .await;
    match result {
        Ok(result) => {
            spawn_follow_up_runs(state.clone(), req.session_id.clone());
            run_registration.close().await;
            clear_running_state_if_needed(&state, &req.session_id)
                .await
                .map_err(internal_error)?;

            Ok(ChatResponse {
                session_id: req.session_id,
                assistant: ApiMessage {
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
                    bash_executions: Vec::new(),
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
                },
            })
        }
        Err(error) => {
            run_registration.close().await;
            clear_running_state_if_needed(&state, &req.session_id)
                .await
                .map_err(internal_error)?;
            Err(internal_error(error))
        }
    }
}

async fn run_chat_collect(state: Arc<UiServerState>, req: ChatRequest) -> SkyResult<ChatResponse> {
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
) -> anyhow::Result<()> {
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
        .map_err(anyhow::Error::from)
}

fn map_llm_role(role: LlmRole) -> &'static str {
    match role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
        LlmRole::Tool => "tool",
    }
}

fn map_context_debug_message(message: &LlmMessage) -> ContextDebugMessage {
    ContextDebugMessage {
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
) -> anyhow::Result<()> {
    let prompt_attachments = prompt_attachments
        .iter()
        .map(url::Url::to_string)
        .collect::<Vec<_>>();
    let assembled_messages = assembled_messages
        .iter()
        .map(map_context_debug_message)
        .collect::<Vec<_>>();
    let prompt_attachments_json = serde_json::to_string(&prompt_attachments)?;
    let assembled_messages_json = serde_json::to_string(&assembled_messages)?;
    let timestamp = jiff::Timestamp::now().to_string();

    let db = state.db();
    db.upsert_context_debug_snapshot(
        session_id.as_str(),
        &timestamp,
        model,
        provider_id,
        prompt,
        &prompt_attachments_json,
        &assembled_messages_json,
    )
    .await
    .map_err(anyhow::Error::from)
}

fn enqueue_event_triggered_tasks(
    state: &Arc<UiServerState>,
    session_id: &SessionId,
    profile_id: Option<&str>,
) {
    let state = state.clone();
    let session_id = session_id.as_str().to_string();
    let profile_id = profile_id.map(ToOwned::to_owned);

    tokio::spawn(async move {
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
    incremental_messages: Vec<LlmMessage>,
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
    prompt: String,
    attachments: Vec<url::Url>,
    provider_candidate_count: usize,
}

struct ToolBrokers {
    workspace: crate::workspace_request::WorkspaceRequestBroker,
    ask_user: ask_user::AskUserBroker,
    task: crate::agent::task_tool::TaskBroker,
    notify: crate::agent::notify_tool::NotifyBroker,
    register_skills: crate::agent::register_tool::RegisterBroker,
    register_subagents: crate::agent::register_tool::RegisterBroker,
}

// ── Agent preparation: decomposed steps ───────────────────

async fn resolve_session_context(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
    use_persisted_context_snapshot: bool,
) -> anyhow::Result<SessionContext> {
    let session_id = SessionId::from_string(req.session_id.clone());

    if is_takeover_active_for_session(state, &session_id).await {
        return Err(anyhow::anyhow!(
            "Browser takeover is active for this session; wait until user returns control"
        ));
    }

    let mut manager = state.sessions().await;
    manager
        .load_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;

    let session_model = manager.current().and_then(|s| s.model.clone());
    let cfg = state.config.read().await;
    let current_model = resolve_default_model(&cfg, session_model)
        .ok_or_else(|| anyhow::anyhow!("No model configured"))?;
    let provider_candidate_count = crate::agent::provider_candidate_count(&cfg, &current_model);
    if provider_candidate_count == 0 {
        return Err(anyhow::anyhow!(
            "No provider available for model '{}'",
            current_model
        ));
    }
    drop(cfg);

    let profile_model =
        if let Some(profile_id) = manager.current().and_then(|s| s.profile_id.clone()) {
            manager
                .db()
                .get_profile(&profile_id)
                .await
                .map_err(anyhow::Error::from)?
        } else {
            None
        };
    let profile_persona = profile_model.as_ref().and_then(|p| p.persona.clone());
    let profile_id = profile_model.as_ref().map(|p| p.id.clone());
    let profile_environment = profile_model
        .as_ref()
        .map(|profile| {
            crate::profile::ProfileInfo::from_db_model(profile.clone()).map(|info| info.environment)
        })
        .transpose()
        .map_err(anyhow::Error::from)?;

    let profile_memories = if let Some(ref profile) = profile_model {
        let memories = crate::memory::ProfileMemories::load(manager.db(), &profile.id)
            .await
            .map_err(anyhow::Error::from)?;
        if memories.is_empty() {
            None
        } else {
            Some(memories)
        }
    } else {
        None
    };
    let runtime = manager
        .db()
        .get_session_runtime(session_id.as_str())
        .await
        .map_err(anyhow::Error::from)?
        .unwrap_or_default();

    let history_messages = manager
        .current()
        .map(|s| s.history.to_aither_messages())
        .unwrap_or_default();
    let context_snapshot = if use_persisted_context_snapshot {
        manager
            .db()
            .get_session_context_snapshot(session_id.as_str())
            .await
            .map_err(anyhow::Error::from)?
            .map(crate::session::parse_persisted_context_snapshot)
            .transpose()?
    } else {
        None
    };

    let mut incremental_messages = Vec::new();
    if persist_user_message {
        if take_over_alert_pending(state, &session_id).await {
            let takeover_alert = browser_takeover_alert_xml()?;
            manager
                .append_message(ChatMessage::hidden_context(takeover_alert.clone()))
                .await
                .map_err(anyhow::Error::from)?;
            incremental_messages.push(LlmMessage::system(takeover_alert));
        }

        if let Some(hidden_context_xml) = req
            .hidden_context_xml
            .as_deref()
            .map(str::trim)
            .filter(|xml| !xml.is_empty())
        {
            let hidden_context_already_present = manager.current().is_some_and(|session| {
                session.history.messages().iter().any(|msg| {
                    matches!(msg.role, MessageRole::HiddenContext)
                        && msg.content == hidden_context_xml
                })
            });
            if !hidden_context_already_present {
                manager
                    .append_message(ChatMessage::hidden_context(hidden_context_xml.to_string()))
                    .await
                    .map_err(anyhow::Error::from)?;
                incremental_messages.push(LlmMessage::system(hidden_context_xml.to_string()));
            }
        }

        let needs_title = manager.current().is_some_and(|s| s.title.is_none());
        if needs_title {
            let title = truncate_preview(&req.content);
            manager
                .set_current_title(title)
                .await
                .map_err(anyhow::Error::from)?;
            state.notify_ui_events();
        }
        manager
            .append_message(ChatMessage::user(req.content.clone()))
            .await
            .map_err(anyhow::Error::from)?;
    }

    let sandbox_dir = manager
        .current()
        .map(|s| s.sandbox_dir.clone())
        .ok_or_else(|| anyhow::anyhow!("No active session"))?;

    Ok(SessionContext {
        session_id,
        current_model,
        provider_candidate_count,
        sandbox_dir,
        history_messages,
        context_snapshot,
        incremental_messages,
        profile_persona,
        profile_memories,
        profile_model,
        profile_environment,
        profile_id,
        plan_state: runtime.plan_state,
    })
}

async fn setup_agent_environment(
    state: &Arc<UiServerState>,
    ctx: &SessionContext,
) -> anyhow::Result<(
    crate::agent::AgentEnvironment,
    Option<crate::location::LocationInfo>,
)> {
    let environment = match ctx.profile_environment.as_ref() {
        Some(crate::profile::ProfileEnvironment::Container { .. }) => {
            let profile = ctx
                .profile_model
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("container profile model is missing"))?;
            let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
            store
                .reconcile_container_mount_points(
                    ctx.session_id.as_str(),
                    ctx.profile_id.as_deref(),
                )
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Failed to reconcile container session layout: {error}")
                })?;
            let container_manager =
                state
                    .resolve_container_manager(false)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "No container runtime available for container profile: {error}"
                        )
                    })?;
            let default_image_tag =
                crate::container::profile::default_image_tag(&state.may_dir, profile)
                    .map_err(|e| anyhow::anyhow!("Invalid container profile image tag: {e}"))?;

            let image_spec = crate::container::profile::image_spec_for_profile(
                &state.may_dir,
                profile,
                &default_image_tag,
            )
            .map_err(|e| anyhow::anyhow!("Invalid container profile configuration: {e}"))?;
            let extra_mounts = crate::container::profile::mounts_for_profile(profile)
                .map_err(|e| anyhow::anyhow!("Invalid container mount configuration: {e}"))?;

            crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
                .await
                .map_err(|e| anyhow::anyhow!("Container image build failed: {e}"))?;

            let container_name = crate::container::profile::container_name_for_profile_session(
                profile,
                &ctx.session_id,
            );
            let host_gateway = container_manager.runtime().host_gateway();

            let mut extra_env = Vec::new();
            let browser_backend: Option<Arc<dyn BrowserBackend>> = if let Some(ref bm) =
                state.browser_manager
            {
                if let Err(e) = bm.start().await {
                    tracing::warn!(error = %e, "Failed to auto-start browser for container session");
                    None
                } else {
                    let config = state.config.read().await;
                    let port = config.playwright.port.to_string();
                    let endpoint = join_text(&["http://", host_gateway, ":", port.as_str()]);
                    extra_env.push(prefixed_text("BROWSER_CDP_ENDPOINT=", endpoint.as_str()));
                    Some(Arc::new(StaticCdpBrowser::new(endpoint)))
                }
            } else {
                None
            };

            let ipc_socket_path = ctx.sandbox_dir.join("leash-ipc.sock");
            let mut launch_spec = ContainerLaunchSpec::new(
                container_name,
                image_spec.tag.clone(),
                ctx.sandbox_dir.clone(),
            )
            .with_ipc_socket(ipc_socket_path);
            for mount in extra_mounts {
                launch_spec = launch_spec.with_mount(mount);
            }
            for env in extra_env {
                launch_spec = launch_spec.with_env(env);
            }

            let container_id = container_manager
                .ensure_running(&launch_spec)
                .await
                .map_err(|e| anyhow::anyhow!("Container start failed: {e}"))?;

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
                .ok_or_else(|| anyhow::anyhow!("remote profile model is missing"))?;
            let config = state.config.read().await;
            let ssh_server =
                crate::remote_profile::resolve_remote_profile_server(profile, &config.ssh_servers)
                    .map_err(|error| anyhow::anyhow!(error))?;
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
                .map_err(|e| anyhow::anyhow!("Location fetch failed: {e}"))?,
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
    tokio::spawn(async move {
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
    tokio::spawn(async move {
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
) -> anyhow::Result<PreparedAgent> {
    let ctx = resolve_session_context(
        state,
        req,
        persist_user_message,
        use_persisted_context_snapshot,
    )
    .await?;
    let (environment, location) = setup_agent_environment(state, &ctx).await?;
    let brokers = build_tool_brokers(state, &ctx.session_id);

    let config = state.config.read().await;
    let mut agent = MayAgent::from_config_with_model_and_permissions(
        &config,
        ctx.sandbox_dir,
        state.may_dir.clone(),
        state.cwd.clone(),
        ctx.session_id.clone(),
        brokers.workspace,
        brokers.ask_user,
        &ctx.current_model,
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
        Some(brokers.register_skills),
        Some(brokers.register_subagents),
        environment,
        provider_attempt,
    )
    .await
    .map_err(anyhow::Error::from)?;

    if let Some(snapshot) = ctx.context_snapshot.as_ref() {
        agent
            .restore_checkpoint(snapshot.checkpoint.clone())
            .map_err(anyhow::Error::from)?;
        for message in crate::session::history_delta_since_snapshot(
            &ctx.history_messages,
            snapshot.history_message_count,
        )? {
            agent.push_message(message);
        }
        for message in ctx.incremental_messages {
            agent.push_message(message);
        }
    } else {
        for message in ctx.history_messages {
            agent.push_message(message);
        }
    }

    let provider_id = agent.provider_id().to_string();

    let (prompt, attachments) = build_prompt_and_attachments(state, req, &ctx.current_model)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let preview_messages = agent
        .preview_request_messages(&prompt, attachments.iter().cloned())
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
    .await?;

    Ok(PreparedAgent {
        session_id: ctx.session_id,
        agent,
        current_model: ctx.current_model,
        provider_id,
        profile_id: ctx.profile_id,
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

async fn handle_task_request(
    state: &Arc<UiServerState>,
    request: crate::agent::task_tool::TaskCreateRequest,
) -> Result<crate::agent::task_tool::TaskCreateResult, String> {
    let result = state
        .task_runtime
        .create_task_from_tool_request(request)
        .await;
    if result.is_ok() {
        state.notify_ui_events();
    }
    result
}

async fn handle_notify_request(
    notification_service: Arc<NotificationService>,
    args: crate::agent::notify_tool::NotifyArgs,
) -> Result<crate::agent::notify_tool::NotifyResult, String> {
    let payload = NotificationPayload {
        notification_id: None,
        title: args.title,
        body: args.body,
        session_id: args.session_id,
        callback_context_xml: args.callback_context_xml,
        callback_message: args.callback_message,
    };
    let sent = notification_service
        .send_payload(payload)
        .await
        .map_err(|error| {
            let error_text = error.to_string();
            join_text(&["failed to send notification payload: ", error_text.as_str()])
        })?;

    Ok(crate::agent::notify_tool::NotifyResult { sent })
}

async fn handle_register_skills_request(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
) -> Result<crate::agent::register_tool::RegisterResult, String> {
    handle_register_request(
        state,
        request,
        "skill",
        "skills",
        |skills_dir, source| async move {
            let manager = crate::skill::SkillManager::from_skills_dir(skills_dir);
            let info = manager
                .register_from_local_path(&source)
                .await
                .map_err(|error| {
                    path_error_text("failed to register skill from ", &source, &error)
                })?;
            Ok(vec![info.dir_name])
        },
    )
    .await
}

async fn handle_register_subagents_request(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
) -> Result<crate::agent::register_tool::RegisterResult, String> {
    handle_register_request(
        state,
        request,
        "subagent definitions",
        "subagents",
        |subagents_dir, source| async move {
            let manager = crate::subagent::SubagentManager::from_subagents_dir(subagents_dir);
            let infos = manager
                .register_from_local_path(&source)
                .await
                .map_err(|error| {
                    path_error_text("failed to register subagents from ", &source, &error)
                })?;
            Ok(infos.into_iter().map(|item| item.file_name).collect())
        },
    )
    .await
}

async fn handle_register_request<F, Fut>(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
    source_label: &str,
    target_dir_name: &str,
    register: F,
) -> Result<crate::agent::register_tool::RegisterResult, String>
where
    F: FnOnce(std::path::PathBuf, std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
{
    let source = resolve_registration_source_path(state, &request.path).await?;
    let session_root = resolve_session_root(state, request.session_id.as_str()).await?;
    let target_dir = session_root.join(target_dir_name);
    let approval_message = build_registration_approval_message(
        request.session_id.as_str(),
        &source,
        source_label,
        target_dir_name,
    );
    request_registration_approval(state, request.session_id.as_str(), &approval_message).await?;
    Ok(crate::agent::register_tool::RegisterResult {
        registered: register(target_dir, source).await?,
    })
}

fn build_registration_approval_message(
    session_id: &str,
    source: &std::path::Path,
    source_label: &str,
    target_dir_name: &str,
) -> String {
    let source_display = source.display().to_string();
    join_text(&[
        "Session ",
        session_id,
        " requests registering ",
        source_label,
        " from ",
        source_display.as_str(),
        " into this session's ",
        target_dir_name,
        ". Approve?",
    ])
}

async fn resolve_session_root(
    state: &UiServerState,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let session_root = load_session_paths(state, session_id)
        .await
        .map_err(|error| error.to_string())?
        .root()
        .to_path_buf();
    tokio::fs::create_dir_all(&session_root)
        .await
        .map_err(|error| {
            path_error_text("failed to create session root ", &session_root, &error)
        })?;
    let session_root_canonical = tokio::fs::canonicalize(&session_root)
        .await
        .map_err(|error| path_error_text("failed to resolve session ", &session_root, &error))?;
    Ok(session_root_canonical)
}

async fn resolve_registration_source_path(
    state: &UiServerState,
    raw_path: &str,
) -> Result<std::path::PathBuf, String> {
    let path = raw_path.trim();
    if path.is_empty() {
        return Err("registration path must not be empty".to_string());
    }

    let candidate = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        state.cwd.join(path)
    };

    let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
        path_error_text("failed to resolve registration path ", &candidate, &error)
    })?;
    let project_root = tokio::fs::canonicalize(&state.cwd)
        .await
        .map_err(|error| path_error_text("failed to resolve project root ", &state.cwd, &error))?;

    if !canonical.starts_with(&project_root) {
        return Err(path_message(
            "registration path must be inside project root ",
            &project_root,
        ));
    }

    Ok(canonical)
}

async fn request_registration_approval(
    state: &UiServerState,
    session_id: &str,
    question: &str,
) -> Result<(), String> {
    let (tx, rx) = async_channel::bounded(1);
    let request = ask_user::AskUserRequest::new(
        ask_user::AskUserRequestPayload {
            session_id: session_id.to_string(),
            request: ask_user::AskUserArgs {
                question: None,
                option: Vec::new(),
                multi_select: false,
                questions: vec![ask_user::Question {
                    section: "registration".to_string(),
                    question: question.to_string(),
                    option: vec!["approve".to_string(), "deny".to_string()],
                    multi_select: false,
                }],
            },
        },
        tx,
    );
    state
        .ask_user_approver
        .enqueue(request, Some(session_id.to_string()))
        .await;
    let answers = rx.recv().await.map_err(|error| {
        let error_text = error.to_string();
        join_text(&["failed to receive user approval: ", error_text.as_str()])
    })?;

    let approved = answers.into_iter().any(|answer| match answer.answer {
        ask_user::AnswerValue::Selected(values) => values
            .iter()
            .any(|value| value.eq_ignore_ascii_case("approve")),
        ask_user::AnswerValue::Custom(value) => value.eq_ignore_ascii_case("approve"),
        ask_user::AnswerValue::Skipped => false,
    });

    if !approved {
        return Err("user denied registration request".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use aither_core::llm::Message as LlmMessage;

    #[test]
    fn history_delta_since_snapshot_returns_suffix_after_anchor() {
        let history = vec![
            LlmMessage::system("memory"),
            LlmMessage::user("first"),
            LlmMessage::assistant("second"),
        ];

        let delta = crate::session::history_delta_since_snapshot(&history, 2)
            .expect("anchor must be valid");
        assert_eq!(delta, vec![LlmMessage::assistant("second")]);
    }

    #[test]
    fn history_delta_since_snapshot_rejects_out_of_bounds_anchor() {
        let history = vec![LlmMessage::user("only")];
        let error = crate::session::history_delta_since_snapshot(&history, 2)
            .expect_err("invalid anchor must fail");
        assert!(
            error
                .to_string()
                .contains("persisted context snapshot anchor exceeds available history")
        );
    }

    #[test]
    fn parse_persisted_context_snapshot_reads_history_anchor() {
        let checkpoint = serde_json::to_string(&aither_agent::AgentCheckpoint {
            context_json: "{\"recent\":[]}".to_string(),
            todo_items: Vec::new(),
            active_skill_names: Vec::new(),
            history_anchor: Some(7),
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
        })
        .expect("checkpoint should serialize");
        let snapshot = crate::session::parse_persisted_context_snapshot(
            crate::db::entity::session_context_snapshots::Model {
                session_id: "session-1".to_string(),
                context_json: checkpoint,
                assembled_messages_json: None,
                metadata_json: None,
                updated_at: "2026-03-14T00:00:00Z".to_string(),
            },
        )
        .expect("snapshot metadata should parse");

        assert_eq!(snapshot.history_message_count, 7);
        assert_eq!(snapshot.checkpoint.context_json, "{\"recent\":[]}");
    }
}
