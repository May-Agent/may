use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use skyzen::responder::Sse;
use skyzen::utils::{Json, State};
use skyzen::Result as SkyResult;

use aither_core::llm::Usage;
use aither_models::lookup as lookup_model_info;

use crate::agent::MayAgent;
use crate::session::{ChatMessage, SessionId};
use crate::{ask_user, workspace_request};

use super::helpers::{
    append_history_message, build_prompt_and_attachments, elapsed_ms_saturating, internal_error,
    is_retryable_llm_error, resolve_default_model, send_event, truncate_preview,
    update_bash_snapshot_from_tool_event, RETRY_BACKOFF_MS,
};
use super::state::UiServerState;
use super::types::{
    ApiMessage, BashStateSnapshot, ChatRequest, ChatResponse, RequestLogEntry, RequestStats,
};

pub(super) async fn chat(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ChatResponse>> {
    let response = run_chat_collect(state, req).await?;
    Ok(Json(response))
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
                &serde_json::json!({ "message": err.to_string() }),
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

async fn stream_chat_events(
    state: Arc<UiServerState>,
    req: ChatRequest,
    sender: skyzen::responder::sse::Sender,
) -> anyhow::Result<()> {
    const MAX_RETRIES: usize = 2;

    for attempt in 0..=MAX_RETRIES {
        let (session_id, mut agent, current_model, provider_id, prompt, attachments) =
            prepare_agent_for_request(&state, &req, attempt == 0).await?;

        let assistant_id = format!("assistant-{}", uuid::Uuid::new_v4());
        let started_at = Instant::now();
        let mut usage = Usage::default();
        let mut assistant_text = String::new();
        let mut reasoning_text = String::new();
        let mut had_output = false;
        let mut bash_snapshot = BashStateSnapshot::default();

        send_event(
            &sender,
            "start",
            &serde_json::json!({
                "session_id": req.session_id,
                "assistant_id": assistant_id,
            }),
        )
        .await;

        let mut terminal_error: Option<String> = None;
        let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
        while let Some(event) = stream.next().await {
            match event {
                Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                    had_output = true;
                    reasoning_text.push_str(&text);
                    send_event(&sender, "reasoning", &serde_json::json!({ "chunk": text }))
                        .await;
                }
                Ok(aither_agent::AgentEvent::Text(text)) => {
                    had_output = true;
                    assistant_text.push_str(&text);
                    send_event(&sender, "text", &serde_json::json!({ "chunk": text })).await;
                }
                Ok(aither_agent::AgentEvent::Usage(u)) => {
                    usage = u;
                }
                Ok(aither_agent::AgentEvent::ToolCallStart {
                    id,
                    name,
                    arguments,
                }) => {
                    had_output = true;

                    if !assistant_text.is_empty() {
                        append_history_message(
                            &state,
                            &session_id,
                            ChatMessage::assistant(assistant_text.clone()),
                        )
                        .await?;
                        assistant_text.clear();
                    }

                    let tool_start_payload = serde_json::json!({
                        "stage": "start",
                        "id": id,
                        "name": name,
                        "arguments": arguments,
                    });
                    if let Ok(serialized) = serde_json::to_string(&tool_start_payload) {
                        append_history_message(
                            &state,
                            &session_id,
                            ChatMessage::tool(id.clone(), serialized),
                        )
                        .await?;
                    }

                    update_bash_snapshot_from_tool_event(&mut bash_snapshot, &tool_start_payload);

                    send_event(
                        &sender,
                        "tool_start",
                        &serde_json::json!({
                            "id": id,
                            "name": name,
                            "arguments": arguments,
                        }),
                    )
                    .await;
                }
                Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
                    had_output = true;

                    let tool_end_payload = serde_json::json!({
                        "stage": "end",
                        "id": id,
                        "name": name,
                        "result": result,
                    });
                    if let Ok(serialized) = serde_json::to_string(&tool_end_payload) {
                        append_history_message(
                            &state,
                            &session_id,
                            ChatMessage::tool(id.clone(), serialized),
                        )
                        .await?;
                    }

                    update_bash_snapshot_from_tool_event(&mut bash_snapshot, &tool_end_payload);

                    send_event(
                        &sender,
                        "tool_end",
                        &serde_json::json!({
                            "id": id,
                            "name": name,
                            "result": result,
                        }),
                    )
                    .await;
                }
                Ok(aither_agent::AgentEvent::Error(err)) => {
                    terminal_error = Some(err.to_string());
                    break;
                }
                Err(err) => {
                    terminal_error = Some(err.to_string());
                    break;
                }
                Ok(_) => {}
            }
        }

        if let Some(err) = terminal_error {
            if !had_output && attempt < MAX_RETRIES && is_retryable_llm_error(&err) {
                tokio::time::sleep(Duration::from_millis(
                    RETRY_BACKOFF_MS * (attempt as u64 + 1),
                ))
                .await;
                continue;
            }

            {
                let mut logs = state.request_logs.lock().await;
                let context_window =
                    lookup_model_info(&current_model).map(|m| m.context_window);
                logs.push_front(RequestLogEntry {
                    session_id: session_id.as_str().to_string(),
                    timestamp: jiff::Timestamp::now().to_string(),
                    model: current_model.clone(),
                    provider_id: provider_id.clone(),
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    context_window,
                    elapsed_ms: elapsed_ms_saturating(&started_at),
                    ok: false,
                    error: Some(err.clone()),
                });
                while logs.len() > 300 {
                    logs.pop_back();
                }
            }

            send_event(&sender, "error", &serde_json::json!({ "message": err })).await;
            return Ok(());
        }

        if !assistant_text.is_empty() {
            append_history_message(
                &state,
                &session_id,
                ChatMessage::assistant(assistant_text.clone()),
            )
            .await?;
        }

        let elapsed_ms = elapsed_ms_saturating(&started_at);

        let context_window = lookup_model_info(&current_model).map(|m| m.context_window);
        {
            let mut logs = state.request_logs.lock().await;
            logs.push_front(RequestLogEntry {
                session_id: session_id.as_str().to_string(),
                timestamp: jiff::Timestamp::now().to_string(),
                model: current_model.clone(),
                provider_id: provider_id.clone(),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                context_window,
                elapsed_ms,
                ok: true,
                error: None,
            });
            while logs.len() > 300 {
                logs.pop_back();
            }
        }

        send_event(
            &sender,
            "done",
            &serde_json::json!({
                "session_id": req.session_id,
                "assistant": {
                    "id": assistant_id,
                    "role": "assistant",
                    "content": assistant_text,
                    "timestamp": jiff::Timestamp::now().to_string(),
                    "attachments": [],
                    "thinking": if reasoning_text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(reasoning_text) },
                    "is_streaming": false,
                    "is_thinking": false,
                    "bash_executions": bash_snapshot.executions,
                    "stats": {
                        "provider_id": provider_id,
                        "prompt_tokens": usage.prompt_tokens,
                        "completion_tokens": usage.completion_tokens,
                        "total_tokens": usage.total_tokens,
                        "context_window": context_window,
                        "elapsed_ms": elapsed_ms
                    }
                }
            }),
        )
        .await;

        return Ok(());
    }

    send_event(
        &sender,
        "error",
        &serde_json::json!({ "message": "LLM endpoint unavailable after retries" }),
    )
    .await;
    Ok(())
}

async fn run_chat_collect(
    state: Arc<UiServerState>,
    req: ChatRequest,
) -> SkyResult<ChatResponse> {
    const MAX_RETRIES: usize = 2;

    for attempt in 0..=MAX_RETRIES {
        let mut bash_snapshot = BashStateSnapshot::default();
        let (session_id, mut agent, current_model, provider_id, prompt, attachments) =
            prepare_agent_for_request(&state, &req, attempt == 0)
                .await
                .map_err(internal_error)?;

        let started_at = Instant::now();
        let mut usage = Usage::default();
        let mut assistant_text = String::new();
        let mut reasoning_text = String::new();
        let mut had_output = false;

        let mut terminal_error: Option<String> = None;
        let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
        while let Some(event) = stream.next().await {
            match event {
                Ok(aither_agent::AgentEvent::Text(text)) => {
                    had_output = true;
                    assistant_text.push_str(&text);
                }
                Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                    had_output = true;
                    reasoning_text.push_str(&text);
                }
                Ok(aither_agent::AgentEvent::Usage(u)) => {
                    usage = u;
                }
                Ok(aither_agent::AgentEvent::ToolCallStart {
                    id,
                    name,
                    arguments,
                }) => {
                    had_output = true;
                    let tool_start_payload = serde_json::json!({
                        "stage": "start",
                        "id": id,
                        "name": name,
                        "arguments": arguments,
                    });

                    if let Ok(serialized) = serde_json::to_string(&tool_start_payload) {
                        let mut manager = state.session_manager.lock().await;
                        if manager.load_session(&session_id).await.is_ok() {
                            if let Some(session) = manager.current_mut() {
                                session
                                    .history
                                    .push(ChatMessage::tool(id.clone(), serialized));
                            }
                            let _ = manager.save_current().await;
                        }
                    }

                    update_bash_snapshot_from_tool_event(&mut bash_snapshot, &tool_start_payload);
                }
                Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
                    had_output = true;
                    let tool_end_payload = serde_json::json!({
                        "stage": "end",
                        "id": id,
                        "name": name,
                        "result": result,
                    });

                    if let Ok(serialized) = serde_json::to_string(&tool_end_payload) {
                        let mut manager = state.session_manager.lock().await;
                        if manager.load_session(&session_id).await.is_ok() {
                            if let Some(session) = manager.current_mut() {
                                session
                                    .history
                                    .push(ChatMessage::tool(id.clone(), serialized));
                            }
                            let _ = manager.save_current().await;
                        }
                    }

                    update_bash_snapshot_from_tool_event(&mut bash_snapshot, &tool_end_payload);
                }
                Ok(aither_agent::AgentEvent::Error(err)) => {
                    terminal_error = Some(err.to_string());
                    break;
                }
                Err(err) => {
                    terminal_error = Some(err.to_string());
                    break;
                }
                Ok(_) => {}
            }
        }

        if let Some(err) = terminal_error {
            if !had_output && attempt < MAX_RETRIES && is_retryable_llm_error(&err) {
                tokio::time::sleep(Duration::from_millis(
                    RETRY_BACKOFF_MS * (attempt as u64 + 1),
                ))
                .await;
                continue;
            }

            {
                let mut logs = state.request_logs.lock().await;
                let context_window =
                    lookup_model_info(&current_model).map(|m| m.context_window);
                logs.push_front(RequestLogEntry {
                    session_id: session_id.as_str().to_string(),
                    timestamp: jiff::Timestamp::now().to_string(),
                    model: current_model.clone(),
                    provider_id: provider_id.clone(),
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    context_window,
                    elapsed_ms: elapsed_ms_saturating(&started_at),
                    ok: false,
                    error: Some(err.clone()),
                });
                while logs.len() > 300 {
                    logs.pop_back();
                }
            }

            return Err(internal_error(err));
        }

        if !assistant_text.is_empty() {
            append_history_message(
                &state,
                &session_id,
                ChatMessage::assistant(assistant_text.clone()),
            )
            .await
            .map_err(internal_error)?;
        }

        let elapsed_ms = elapsed_ms_saturating(&started_at);

        let context_window = lookup_model_info(&current_model).map(|m| m.context_window);
        {
            let mut logs = state.request_logs.lock().await;
            logs.push_front(RequestLogEntry {
                session_id: session_id.as_str().to_string(),
                timestamp: jiff::Timestamp::now().to_string(),
                model: current_model.clone(),
                provider_id: provider_id.clone(),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                context_window,
                elapsed_ms,
                ok: true,
                error: None,
            });
            while logs.len() > 300 {
                logs.pop_back();
            }
        }

        return Ok(ChatResponse {
            session_id: req.session_id,
            assistant: ApiMessage {
                id: format!("assistant-{}", uuid::Uuid::new_v4()),
                role: "assistant".to_string(),
                content: assistant_text,
                timestamp: jiff::Timestamp::now().to_string(),
                attachments: Vec::new(),
                thinking: if reasoning_text.is_empty() {
                    None
                } else {
                    Some(reasoning_text)
                },
                is_streaming: Some(false),
                is_thinking: Some(false),
                tool_event: None,
                bash_executions: Vec::new(),
                stats: Some(RequestStats {
                    provider_id: Some(provider_id),
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    context_window,
                    elapsed_ms: Some(elapsed_ms),
                }),
            },
        });
    }

    Err(internal_error("LLM endpoint unavailable after retries"))
}

async fn prepare_agent_for_request(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
) -> anyhow::Result<(SessionId, MayAgent, String, String, String, Vec<url::Url>)> {
    let session_id = SessionId::from_string(req.session_id.clone());

    let (current_model, sandbox_dir, history_messages) = {
        let mut manager = state.session_manager.lock().await;
        manager
            .load_session(&session_id)
            .await
            .map_err(anyhow::Error::from)?;

        let session_model = manager.current().and_then(|s| s.model.clone());
        let cfg = state.config.read().await;
        let current_model = resolve_default_model(&cfg, session_model)
            .ok_or_else(|| anyhow::anyhow!("No model configured"))?;
        drop(cfg);

        if persist_user_message {
            // Set title from first user message if not already set
            let needs_title = manager.current().is_some_and(|s| s.title.is_none());
            if needs_title {
                let title = truncate_preview(&req.content);
                manager
                    .set_current_title(title)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            // Persist user message via DB INSERT + in-memory push
            manager
                .append_message(ChatMessage::user(req.content.clone()))
                .await
                .map_err(anyhow::Error::from)?;
        }

        let history_messages = manager
            .current()
            .map(|s| s.history.to_aither_messages())
            .unwrap_or_default();

        let sandbox_dir = manager
            .current()
            .map(|s| s.sandbox_dir.clone())
            .ok_or_else(|| anyhow::anyhow!("No active session"))?;

        (current_model, sandbox_dir, history_messages)
    };

    let (workspace_broker, workspace_queue) = workspace_request::channel();
    let (ask_user_broker, ask_user_queue) = ask_user::channel();

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

    let config = state.config.read().await;
    let mut agent = MayAgent::from_config_with_model_and_permissions(
        &config,
        sandbox_dir,
        state.may_dir.clone(),
        state.cwd.clone(),
        session_id.clone(),
        workspace_broker,
        ask_user_broker,
        &current_model,
        Some(
            state
                .permission_approver
                .for_session(session_id.as_str().to_string()),
        ),
    )
    .await
    .map_err(anyhow::Error::from)?;

    for message in history_messages {
        agent.inner_mut().push_message(message);
    }

    let provider_id = agent.provider_id().to_string();

    let (prompt, attachments) = build_prompt_and_attachments(state, req)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    Ok((
        session_id,
        agent,
        current_model,
        provider_id,
        prompt,
        attachments,
    ))
}
