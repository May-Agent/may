use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use skyzen::Result as SkyResult;
use skyzen::responder::Sse;
use skyzen::utils::{Json, State};

use aither_agent::{BrowserBackend, StaticCdpBrowser};
use aither_core::llm::{Message as LlmMessage, Role as LlmRole, Usage};
use aither_models::lookup as lookup_model_info;
use aither_sandbox::{BollardContainerExec, ContainerExecHandle, ContainerLaunchSpec};

use crate::agent::MayAgent;
use crate::notification::NotificationPayload;
use crate::notification_service::NotificationService;
use crate::session::{
    ChatMessage, ChatMessageStats, MessageRole, PendingUserMessage, PersistedAttachment, SessionId,
    SessionRuntimeState,
};
use crate::task_trigger::EVENT_CHAT_TURN_COMPLETED;
use crate::{ask_user, workspace_request};

use super::attachments::build_prompt_and_attachments;
use super::bash_parsing::update_bash_snapshot_from_tool_event;
use super::error::internal_error;
use super::helpers::{
    RETRY_BACKOFF_MS, append_history_message, elapsed_ms_saturating, send_event, truncate_preview,
};
use super::model_resolution::resolve_default_model;
use super::state::UiServerState;
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
    async fn on_start(&mut self, session_id: &str, assistant_id: &str);
    async fn on_text(&mut self, chunk: &str);
    async fn on_reasoning(&mut self, chunk: &str);
    async fn on_tool_start(&mut self, id: &str, name: &str, arguments: &str);
    async fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &Result<String, String>,
    );
    async fn on_todo_update(&mut self, event: &SseTodoUpdateEvent);
    async fn on_error(&mut self, message: &str);
}

struct SseSink(skyzen::responder::sse::Sender);

impl ChatSink for SseSink {
    async fn on_start(&mut self, session_id: &str, assistant_id: &str) {
        send_event(
            &self.0,
            "start",
            &SseStartEvent {
                session_id,
                assistant_id,
            },
        )
        .await;
    }

    async fn on_text(&mut self, chunk: &str) {
        send_event(&self.0, "text", &SseChunkEvent { chunk }).await;
    }

    async fn on_reasoning(&mut self, chunk: &str) {
        send_event(&self.0, "reasoning", &SseChunkEvent { chunk }).await;
    }

    async fn on_tool_start(&mut self, id: &str, name: &str, arguments: &str) {
        send_event(
            &self.0,
            "tool_start",
            &SseToolStartEvent {
                id,
                name,
                arguments,
            },
        )
        .await;
    }

    async fn on_tool_end(
        &mut self,
        id: &str,
        name: &str,
        arguments: Option<&str>,
        result: &Result<String, String>,
    ) {
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
        .await;
    }

    async fn on_todo_update(&mut self, event: &SseTodoUpdateEvent) {
        send_event(&self.0, "todo_update", event).await;
    }

    async fn on_error(&mut self, message: &str) {
        send_event(&self.0, "error", &SseErrorEvent { message }).await;
    }
}

/// No-op sink for non-streaming collect mode.
struct CollectSink;

impl ChatSink for CollectSink {
    async fn on_start(&mut self, _: &str, _: &str) {}
    async fn on_text(&mut self, _: &str) {}
    async fn on_reasoning(&mut self, _: &str) {}
    async fn on_tool_start(&mut self, _: &str, _: &str, _: &str) {}
    async fn on_tool_end(&mut self, _: &str, _: &str, _: Option<&str>, _: &Result<String, String>) {
    }
    async fn on_todo_update(&mut self, _: &SseTodoUpdateEvent) {}
    async fn on_error(&mut self, _: &str) {}
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

fn spawn_follow_up_runs(state: Arc<UiServerState>, session_id: String) {
    tokio::spawn(async move {
        let inserted = {
            let mut active = state.active_streaming_sessions.lock().await;
            active.insert(session_id.clone())
        };
        if !inserted {
            return;
        }

        loop {
            let next = match state.runtime_service.take_next_pending(&session_id).await {
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

        {
            let mut active = state.active_streaming_sessions.lock().await;
            active.remove(session_id.as_str());
        }
        let _ = clear_running_state_if_needed(&state, &session_id).await;
    });
}

const BROWSER_TAKEOVER_ALERT_XML: &str = include_str!("../texts/browser_takeover_alert.xml");

async fn record_request_log(state: &Arc<UiServerState>, entry: RequestLogEntry) {
    {
        let mut logs = state.request_logs.lock().await;
        logs.push_front(entry);
        while logs.len() > 300 {
            logs.pop_back();
        }
    }
    state.notify_ui_events();
}

pub(super) async fn chat(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ChatResponse>> {
    set_session_runtime_state(&state, &req.session_id, SessionRuntimeState::Running)
        .await
        .map_err(internal_error)?;
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

pub(super) async fn chat_stream(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Sse> {
    let (sender, sse) = Sse::channel();

    tokio::spawn(async move {
        let session_id = req.session_id.clone();
        state
            .active_streaming_sessions
            .lock()
            .await
            .insert(session_id.clone());

        if let Err(err) = stream_chat_events(state.clone(), req, sender.clone()).await {
            send_event(
                &sender,
                "error",
                &SseErrorEvent {
                    message: &err.to_string(),
                },
            )
            .await;
        }

        state
            .active_streaming_sessions
            .lock()
            .await
            .remove(&session_id);
    });

    Ok(sse)
}

pub(super) fn spawn_register_subagents_worker(
    state: Arc<UiServerState>,
    queue: crate::agent::register_tool::RegisterSubagentsQueue,
) {
    tokio::spawn(async move {
        while let Some(request) = queue.next().await {
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
        } = match prepare_agent_for_request(state, req, attempt == 0, attempt).await {
            Ok(prepared) => prepared,
            Err(error) => {
                sink.on_error(&error.to_string()).await;
                return Err(error);
            }
        };
        if attempt == 0 {
            max_retries = provider_candidate_count.saturating_sub(1);
        }
        let model_context_window = agent
            .model_context_window()
            .or_else(|| lookup_model_info(&current_model).and_then(|m| m.max_input_tokens()));

        let assistant_id = format!("assistant-{}", uuid::Uuid::new_v4());
        let started_at = Instant::now();
        let mut usage = Usage::default();
        let mut segment_text = String::new(); // current text segment, flushed on tool calls
        let mut full_text = String::new(); // accumulated full assistant text
        let mut reasoning_text = String::new();
        let mut bash_snapshot = BashStateSnapshot::default();
        let mut tool_arguments_by_id: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_todo_hash: Option<u64> = None;
        let tasks_md_path = agent.sandbox_dir().join("tasks.md");

        sink.on_start(&req.session_id, &assistant_id).await;

        let mut terminal_error: Option<aither_agent::AgentError> = None;
        {
            let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
            while let Some(event) = stream.next().await {
                match event {
                    Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                        reasoning_text.push_str(&text);
                        sink.on_reasoning(&text).await;
                    }
                    Ok(aither_agent::AgentEvent::Text(text)) => {
                        segment_text.push_str(&text);
                        full_text.push_str(&text);
                        sink.on_text(&text).await;
                    }
                    Ok(aither_agent::AgentEvent::Usage(u)) => {
                        usage = u;
                    }
                    Ok(aither_agent::AgentEvent::ToolCallStart {
                        id,
                        name,
                        arguments,
                    }) => {
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
                        sink.on_tool_start(&id, &name, &arguments).await;
                    }
                    Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
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
                        sink.on_tool_end(&id, &name, arguments.as_deref(), &result)
                            .await;

                        // Check if tasks.md changed after tool execution.
                        if let Some(event) =
                            check_tasks_update(&tasks_md_path, &mut last_todo_hash).await
                        {
                            sink.on_todo_update(&event).await;
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
            sink.on_error(&err_msg).await;
            return Err(anyhow::anyhow!(err));
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
        if !has_assistant_text && !has_bash_output {
            let err = "Model returned an empty assistant response";
            record_request_log(
                state,
                log_base.failure(elapsed_ms_saturating(&started_at), err),
            )
            .await;
            sink.on_error(err).await;
            return Err(anyhow::anyhow!(err));
        }
        if !has_assistant_text && has_bash_output {
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
        persist_context_snapshot(state, &session_id, &mut agent).await?;

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
) -> anyhow::Result<()> {
    set_session_runtime_state(&state, &req.session_id, SessionRuntimeState::Running).await?;
    let mut sink = SseSink(sender.clone());
    let result = run_agent_loop(&state, &req, &mut sink).await;
    {
        let mut active = state.active_streaming_sessions.lock().await;
        active.remove(req.session_id.as_str());
    }
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
                        context_window: result.model_context_window,
                        elapsed_ms: Some(result.elapsed_ms),
                    }),
                },
            };
            send_event(&sender, "done", &done_event).await;
            spawn_follow_up_runs(state.clone(), response_session_id.clone());
            clear_running_state_if_needed(&state, &response_session_id).await?;
            Ok(())
        }
        // Errors are already sent to the sink via on_error in run_agent_loop.
        Err(_) => {
            clear_running_state_if_needed(&state, &response_session_id).await?;
            Ok(())
        }
    }
}

async fn run_chat_collect(state: Arc<UiServerState>, req: ChatRequest) -> SkyResult<ChatResponse> {
    let mut sink = CollectSink;
    let result = run_agent_loop(&state, &req, &mut sink)
        .await
        .map_err(|e| internal_error(e))?;

    spawn_follow_up_runs(state.clone(), req.session_id.clone());
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
                context_window: result.model_context_window,
                elapsed_ms: Some(result.elapsed_ms),
            }),
        },
    })
}

async fn persist_context_snapshot(
    state: &Arc<UiServerState>,
    session_id: &SessionId,
    agent: &mut MayAgent,
) -> anyhow::Result<()> {
    let context_json = agent.export_context_json().map_err(anyhow::Error::from)?;
    let window_snapshot = agent.snapshot_context_window().await;
    let assembled_messages_json = serde_json::to_string(&window_snapshot.messages)?;
    let metadata_json = serde_json::to_string(&window_snapshot.metrics)?;
    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };
    db.upsert_session_context_snapshot(
        session_id.as_str(),
        crate::db::SessionContextSnapshotUpsert {
            context_json: &context_json,
            assembled_messages_json: Some(&assembled_messages_json),
            metadata_json: Some(&metadata_json),
        },
    )
    .await
    .map_err(anyhow::Error::from)
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
    let mut manager = state.session_manager.lock().await;
    if manager.current().map(|s| &s.id) != Some(session_id) {
        manager
            .load_session(session_id)
            .await
            .map_err(anyhow::Error::from)?;
    }

    let total_tokens = usage.total_tokens.or_else(|| {
        usage
            .prompt_tokens
            .zip(usage.completion_tokens)
            .map(|(p, c)| p + c)
    });
    manager
        .set_latest_assistant_stats(ChatMessageStats {
            provider_id: Some(provider_id.to_string()),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens,
            context_window: model_context_window,
            elapsed_ms: Some(elapsed_ms),
        })
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

    let db = {
        let manager = state.session_manager.lock().await;
        manager.db().clone()
    };
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
    context_snapshot_json: Option<String>,
    incremental_messages: Vec<LlmMessage>,
    profile_persona: Option<String>,
    profile_memories: Option<crate::memory::ProfileMemories>,
    profile_model: Option<crate::db::entity::profiles::Model>,
    profile_environment: Option<crate::profile::ProfileEnvironment>,
    profile_id: Option<String>,
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
    workspace: workspace_request::WorkspaceRequestBroker,
    ask_user: ask_user::AskUserBroker,
    task: crate::agent::task_tool::TaskBroker,
    notify: crate::agent::notify_tool::NotifyBroker,
    register_skills: crate::agent::register_tool::RegisterSkillsBroker,
    register_subagents: crate::agent::register_tool::RegisterSubagentsBroker,
}

// ── Agent preparation: decomposed steps ───────────────────

async fn resolve_session_context(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
) -> anyhow::Result<SessionContext> {
    let session_id = SessionId::from_string(req.session_id.clone());

    if is_takeover_active_for_session(state, &session_id).await {
        return Err(anyhow::anyhow!(
            "Browser takeover is active for this session; wait until user returns control"
        ));
    }

    let mut manager = state.session_manager.lock().await;
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

    let mut incremental_messages = Vec::new();
    if persist_user_message {
        if take_over_alert_pending(state, &session_id).await {
            let takeover_alert = BROWSER_TAKEOVER_ALERT_XML.trim().to_string();
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

    let history_messages = manager
        .current()
        .map(|s| s.history.to_aither_messages())
        .unwrap_or_default();
    let history_messages = if persist_user_message {
        let mut history_messages = history_messages;
        let should_pop = history_messages.last().is_some_and(|message| {
            message.role() == LlmRole::User && message.content() == req.content
        });
        if should_pop {
            let _ = history_messages.pop();
        }
        history_messages
    } else {
        history_messages
    };
    let context_snapshot_json = manager
        .db()
        .get_session_context_snapshot(session_id.as_str())
        .await
        .map_err(anyhow::Error::from)?
        .map(|snapshot| snapshot.context_json);

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
        context_snapshot_json,
        incremental_messages,
        profile_persona,
        profile_memories,
        profile_model,
        profile_environment,
        profile_id,
    })
}

async fn setup_agent_environment(
    state: &Arc<UiServerState>,
    ctx: &SessionContext,
) -> anyhow::Result<(
    crate::agent::AgentEnvironment,
    Option<crate::location::LocationInfo>,
)> {
    let environment = if matches!(
        ctx.profile_environment,
        Some(crate::profile::ProfileEnvironment::Container { .. })
    ) {
        let profile = ctx
            .profile_model
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("container profile model is missing"))?;
        {
            let manager = state.session_manager.lock().await;
            manager
                .reconcile_container_layout_for_session(&ctx.session_id)
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Failed to reconcile container session layout: {error}")
                })?;
        }
        let container_manager = state
            .resolve_container_manager(false)
            .await
            .map_err(|error| {
                anyhow::anyhow!("No container runtime available for container profile: {error}")
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

        let container_name =
            crate::container::profile::container_name_for_profile_session(profile, &ctx.session_id);

        let mut extra_env = Vec::new();
        let browser_backend: Option<Arc<dyn BrowserBackend>> = if let Some(ref bm) =
            state.browser_manager
        {
            if let Err(e) = bm.start().await {
                tracing::warn!(error = %e, "Failed to auto-start browser for container session");
                None
            } else {
                let host_gateway = container_manager.runtime().host_gateway();
                let config = state.config.read().await;
                let endpoint = format!("http://{}:{}", host_gateway, config.playwright.port);
                extra_env.push(format!("BROWSER_CDP_ENDPOINT={endpoint}"));
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

        let container_exec = std::sync::Arc::new(ContainerExecHandle::new(std::sync::Arc::new(
            BollardContainerExec::new(std::sync::Arc::new(
                container_manager.runtime().client.clone(),
            )),
        )));

        crate::agent::AgentEnvironment::Container {
            container_id,
            container_exec,
            browser_backend,
        }
    } else {
        crate::agent::AgentEnvironment::Local
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

fn wire_tool_channels(state: &Arc<UiServerState>, session_id: &SessionId) -> ToolBrokers {
    let (workspace_broker, workspace_queue) = workspace_request::channel();
    let (ask_user_broker, ask_user_queue) = ask_user::channel();
    let (task_broker, task_queue) = crate::agent::task_tool::channel();
    let (notify_broker, notify_queue) = crate::agent::notify_tool::channel();
    let (register_skills_broker, register_skills_queue) =
        crate::agent::register_tool::skills_channel();
    let register_subagents_broker = state.register_subagents_broker.clone();

    tokio::spawn(async move {
        while let Some(request) = workspace_queue.next().await {
            let _ = request.respond(false);
        }
    });

    let ask_user_approver = state.ask_user_approver.clone();
    let ask_user_session_id = session_id.as_str().to_string();
    tokio::spawn(async move {
        while let Some(request) = ask_user_queue.next().await {
            ask_user_approver
                .enqueue(request, Some(ask_user_session_id.clone()))
                .await;
        }
    });

    let task_state = state.clone();
    tokio::spawn(async move {
        while let Some(request) = task_queue.next().await {
            let result = handle_task_request(&task_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let notification_service = state.notification_service.clone();
    tokio::spawn(async move {
        while let Some(request) = notify_queue.next().await {
            let result =
                handle_notify_request(notification_service.clone(), request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    let register_skills_state = state.clone();
    tokio::spawn(async move {
        while let Some(request) = register_skills_queue.next().await {
            let result =
                handle_register_skills_request(&register_skills_state, request.args.clone()).await;
            let _ = request.respond(result);
        }
    });

    ToolBrokers {
        workspace: workspace_broker,
        ask_user: ask_user_broker,
        task: task_broker,
        notify: notify_broker,
        register_skills: register_skills_broker,
        register_subagents: register_subagents_broker,
    }
}

async fn prepare_agent_for_request(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
    provider_attempt: usize,
) -> anyhow::Result<PreparedAgent> {
    let ctx = resolve_session_context(state, req, persist_user_message).await?;
    let (environment, location) = setup_agent_environment(state, &ctx).await?;
    let brokers = wire_tool_channels(state, &ctx.session_id);

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
        Some(
            state
                .permission_approver
                .for_session(ctx.session_id.as_str().to_string()),
        ),
        ctx.profile_persona,
        ctx.profile_memories,
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

    if let Some(context_snapshot_json) = ctx.context_snapshot_json.as_deref() {
        agent
            .restore_context_json(context_snapshot_json)
            .map_err(anyhow::Error::from)?;
        for message in ctx.incremental_messages {
            agent.inner_mut().push_message(message);
        }
    } else {
        for message in ctx.history_messages {
            agent.inner_mut().push_message(message);
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
    state.browser_takeover_session.lock().await.as_deref() == Some(session_id.as_str())
}

async fn take_over_alert_pending(state: &UiServerState, session_id: &SessionId) -> bool {
    state
        .browser_takeover_alert_pending
        .lock()
        .await
        .remove(session_id.as_str())
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
        title: args.title,
        body: args.body,
        session_id: args.session_id,
        callback_context_xml: args.callback_context_xml,
        callback_message: args.callback_message,
    };
    let sent = notification_service
        .send_payload(payload)
        .await
        .map_err(|e| format!("failed to send notification payload: {e}"))?;

    Ok(crate::agent::notify_tool::NotifyResult { sent })
}

async fn handle_register_skills_request(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
) -> Result<crate::agent::register_tool::RegisterResult, String> {
    let source = resolve_registration_source_path(state, &request.path).await?;
    let session_root = resolve_session_root(state, request.session_id.as_str()).await?;
    let skills_dir = session_root.join("skills");
    let approval_message = format!(
        "Session {} requests registering skill from {} into this session's skills. Approve?",
        request.session_id,
        source.display()
    );
    request_registration_approval(state, request.session_id.as_str(), &approval_message).await?;

    let manager = crate::skill::SkillManager::from_skills_dir(skills_dir);
    let info = manager
        .register_from_local_path(&source)
        .await
        .map_err(|e| format!("failed to register skill from {}: {e}", source.display()))?;

    Ok(crate::agent::register_tool::RegisterResult {
        registered: vec![info.dir_name],
    })
}

async fn handle_register_subagents_request(
    state: &Arc<UiServerState>,
    request: crate::agent::register_tool::RegisterRequest,
) -> Result<crate::agent::register_tool::RegisterResult, String> {
    let source = resolve_registration_source_path(state, &request.path).await?;
    let session_root = resolve_session_root(state, request.session_id.as_str()).await?;
    let subagents_dir = session_root.join("subagents");
    let approval_message = format!(
        "Session {} requests registering subagent definitions from {} into this session's subagents. Approve?",
        request.session_id,
        source.display()
    );
    request_registration_approval(state, request.session_id.as_str(), &approval_message).await?;

    let manager = crate::subagent::SubagentManager::from_subagents_dir(subagents_dir);
    let infos = manager
        .register_from_local_path(&source)
        .await
        .map_err(|e| {
            format!(
                "failed to register subagents from {}: {e}",
                source.display()
            )
        })?;

    Ok(crate::agent::register_tool::RegisterResult {
        registered: infos.into_iter().map(|item| item.file_name).collect(),
    })
}

async fn resolve_session_root(
    state: &UiServerState,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let sessions_root = state.may_dir.join("sessions");
    let sessions_root_canonical =
        tokio::fs::canonicalize(&sessions_root)
            .await
            .map_err(|error| {
                format!(
                    "failed to resolve sessions root {}: {error}",
                    sessions_root.display()
                )
            })?;
    let session_root = sessions_root.join(session_id);
    let session_root_canonical = tokio::fs::canonicalize(&session_root)
        .await
        .map_err(|error| {
            format!(
                "failed to resolve session {}: {error}",
                session_root.display()
            )
        })?;
    if !session_root_canonical.starts_with(&sessions_root_canonical) {
        return Err(format!(
            "session path {} is outside sessions root {}",
            session_root_canonical.display(),
            sessions_root_canonical.display()
        ));
    }
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

    let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|e| {
        format!(
            "failed to resolve registration path {}: {e}",
            candidate.display()
        )
    })?;
    let project_root = tokio::fs::canonicalize(&state.cwd).await.map_err(|e| {
        format!(
            "failed to resolve project root {}: {e}",
            state.cwd.display()
        )
    })?;

    if !canonical.starts_with(&project_root) {
        return Err(format!(
            "registration path must be inside project root {}",
            project_root.display()
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
        ask_user::AskUserArgs {
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
        tx,
    );
    state
        .ask_user_approver
        .enqueue(request, Some(session_id.to_string()))
        .await;
    let answers = rx
        .recv()
        .await
        .map_err(|e| format!("failed to receive user approval: {e}"))?;

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
