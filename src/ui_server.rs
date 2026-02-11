use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_fs;
use base64::Engine;
use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::responder::{Sse, sse::Event};
use skyzen::routing::{CreateRouteNode, Params, Route, Router};
use skyzen::runtime::native;
use skyzen::utils::{Json, State};
use skyzen::{Body, Error as SkyError, Response, Result as SkyResult, StatusCode, header};
use tokio::sync::Mutex;

use crate::agent::{MayAgent, PermissionApprover, PermissionRequest};
use crate::ask_user::{self, AnswerValue, QuestionAnswer};
use crate::config::{MayConfig, SshServerConfig};
use crate::mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
use crate::session::{ChatMessage, MessageRole, SessionId, SessionManager};
use crate::skill::{SkillInfo, SkillManager};
use crate::workspace_request;

struct UiServerState {
    session_manager: Mutex<SessionManager>,
    mcp_manager: Mutex<McpManager>,
    skill_manager: SkillManager,
    permission_approver: PermissionApprover,
    cwd: PathBuf,
    may_dir: PathBuf,
    web_root: PathBuf,
}

#[derive(Debug, serde::Serialize)]
struct ApiHealth {
    ok: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct MemoryPayload {
    content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ApiAttachment {
    id: String,
    name: String,
    #[serde(rename = "type")]
    attachment_type: String,
    url: String,
    #[serde(default)]
    text_content: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ApiMessage {
    id: String,
    role: String,
    content: String,
    timestamp: String,
    #[serde(default)]
    attachments: Vec<ApiAttachment>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    is_streaming: Option<bool>,
    #[serde(default)]
    is_thinking: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ApiSession {
    id: String,
    title: String,
    last_modified: String,
    preview: String,
    messages: Vec<ApiMessage>,
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct CreateSessionRequest {
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct UpdateSessionRequest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatRequest {
    session_id: String,
    content: String,
    #[serde(default)]
    attachments: Vec<ApiAttachment>,
}

#[derive(Debug, serde::Serialize)]
struct ChatResponse {
    session_id: String,
    assistant: ApiMessage,
}

#[derive(Debug, serde::Serialize)]
struct ModelsResponse {
    models: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct PermissionPollResponse {
    request: Option<PermissionRequest>,
}

#[derive(Debug, serde::Deserialize)]
struct PermissionRespondRequest {
    id: String,
    approved: bool,
}

#[derive(Debug, serde::Serialize)]
struct UiConfigResponse {
    ssh_servers: Vec<SshServerConfig>,
    max_iterations: usize,
}

#[derive(Debug, serde::Deserialize)]
struct SshServerRequest {
    name: String,
    target: String,
}

#[derive(Debug, serde::Deserialize)]
struct McpAddRequest {
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct SkillInstallRequest {
    repo: String,
}

#[derive(Debug, serde::Serialize)]
struct SkillInfoDto {
    name: String,
    description: String,
    dir_name: String,
    source: String,
}

pub async fn run_ui_server(args: &[String]) -> anyhow::Result<()> {
    let host = parse_arg_value(args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = parse_arg_value(args, "--port")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8787);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("Invalid host/port: {host}:{port}"))?;

    unsafe {
        std::env::set_var("SKYZEN_ADDRESS", addr.to_string());
    }

    let cwd = std::env::current_dir().context("Failed to get current working directory")?;
    let may_dir = MayConfig::may_dir().context("Failed to resolve ~/.may")?;
    let web_root = cwd.join("web-ui").join("dist");

    let mut mcp_manager = McpManager::load().unwrap_or_else(|_| McpManager::new(McpConfig::default()));
    if mcp_manager.has_servers() {
        let _ = mcp_manager.connect_all().await;
    }

    let permission_approver = PermissionApprover::new();

    let state = Arc::new(UiServerState {
        session_manager: Mutex::new(SessionManager::new()?),
        mcp_manager: Mutex::new(mcp_manager),
        skill_manager: SkillManager::new(&may_dir),
        permission_approver,
        cwd,
        may_dir,
        web_root,
    });

    let state_for_server = state.clone();
    tokio::task::spawn_blocking(move || {
        native::init_logging();
        native::launch(move || {
            let state = state_for_server.clone();
            async move { build_router(state) }
        });
    })
    .await
    .context("UI server task failed")?;

    Ok(())
}

fn build_router(state: Arc<UiServerState>) -> Router {
    Route::new((
        "/api/health".at(health),
        "/api/models".at(models),
        "/api/sessions".route(Route::new((
            "".at(list_sessions),
            "".post(create_session),
            "/delete-all".post(delete_all_sessions),
            "/{id}".put(update_session),
            "/{id}".delete(delete_session),
        ))),
        "/api/chat".route(Route::new((
            "".post(chat),
            "/stream".post(chat_stream),
        ))),
        "/api/permissions".route(Route::new((
            "/next".at(poll_permission_request),
            "/respond".post(respond_permission_request),
        ))),
        "/api/config".at(get_ui_config),
        "/api/memory".at(get_memory),
        "/api/memory".post(save_memory),
        "/api/ssh".route(Route::new((
            "".post(add_ssh_server),
            "/{name}".delete(remove_ssh_server),
        ))),
        "/api/mcp".route(Route::new((
            "".at(list_mcp_servers),
            "".post(add_mcp_server),
            "/{name}".delete(remove_mcp_server),
            "/{name}/reconnect".post(reconnect_mcp_server),
        ))),
        "/api/skills".route(Route::new((
            "".at(list_skills),
            "/install".post(install_skill),
            "/{dir}".delete(delete_skill),
        ))),
        "/{*path}".at(static_or_index),
    ))
    .middleware(State(state))
    .build()
}

async fn health() -> SkyResult<Json<ApiHealth>> {
    Ok(Json(ApiHealth { ok: true }))
}

async fn models() -> SkyResult<Json<ModelsResponse>> {
    let config = MayConfig::load().unwrap_or_default();
    let mut models: Vec<String> = config.available_models().into_iter().map(|m| m.id).collect();

    for tier in crate::config::ModelTier::ALL {
        for model in config.models.models_for_tier(*tier) {
            if !models.iter().any(|m| m == model) {
                models.push(model.clone());
            }
        }
    }

    models.sort();
    models.dedup();
    Ok(Json(ModelsResponse { models }))
}

async fn list_sessions(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<Vec<ApiSession>>> {
    let manager = state.session_manager.lock().await;
    let store = manager.store().clone();
    let session_ids = store.list_sessions().await.map_err(internal_error)?;

    let mut sessions = Vec::new();
    for id in session_ids {
        let history = store.read_history(&id).await.map_err(internal_error)?;
        sessions.push(session_from_history(&id, &history));
    }
    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Ok(Json(sessions))
}

async fn create_session(
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let config = MayConfig::load().unwrap_or_default();
    let fallback_model = config
        .default_model_for_tier(crate::config::ModelTier::Balanced)
        .unwrap_or("gpt-4")
        .to_string();

    let mut manager = state.session_manager.lock().await;
    let model = req.model.or(config.last_model).unwrap_or(fallback_model);

    let session = manager
        .create_session_with_model(Some(model.clone()))
        .map_err(internal_error)?;
    let id = session.id.as_str().to_string();

    manager.ensure_persisted().await.map_err(internal_error)?;
    manager
        .set_current_model(model.clone())
        .await
        .map_err(internal_error)?;

    Ok(Json(ApiSession {
        id,
        title: "New Chat".to_string(),
        last_modified: jiff::Timestamp::now().to_string(),
        preview: "New conversation started".to_string(),
        messages: Vec::new(),
        model,
        reasoning_effort: None,
    }))
}

async fn update_session(
    params: Params,
    Json(req): Json<UpdateSessionRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let session_id = SessionId::from_string(id.clone());

    let mut manager = state.session_manager.lock().await;
    manager
        .load_session(&session_id)
        .await
        .map_err(not_found_error)?;

    if let Some(title) = req.title {
        manager
            .set_current_title(title)
            .await
            .map_err(internal_error)?;
    }
    if let Some(model) = req.model {
        manager
            .set_current_model(model)
            .await
            .map_err(internal_error)?;
    }

    let current = manager.current().ok_or_else(|| {
        SkyError::msg("No active session").set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    Ok(Json(ApiSession {
        id,
        title: current.title.clone().unwrap_or_else(|| "New Chat".to_string()),
        last_modified: jiff::Timestamp::now().to_string(),
        preview: current
            .history
            .messages()
            .last()
            .map(|m| truncate_preview(&m.content))
            .unwrap_or_else(|| "New conversation started".to_string()),
        messages: current
            .history
            .messages()
            .iter()
            .enumerate()
            .map(|(idx, msg)| map_message(format!("msg-{idx}"), msg))
            .collect(),
        model: current.model.clone().unwrap_or_else(|| "gpt-4".to_string()),
        reasoning_effort: None,
    }))
}

async fn delete_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let manager = state.session_manager.lock().await;
    manager
        .store()
        .delete_session(&id)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn chat(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ChatResponse>> {
    let response = run_chat_collect(state, req).await?;
    Ok(Json(response))
}

async fn chat_stream(
    Json(req): Json<ChatRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Sse> {
    let (sender, sse) = Sse::channel();

    tokio::spawn(async move {
        let _ = stream_chat_events(state, req, sender).await;
    });

    Ok(sse)
}

async fn stream_chat_events(
    state: Arc<UiServerState>,
    req: ChatRequest,
    sender: skyzen::responder::sse::Sender,
) -> anyhow::Result<()> {
    let (session_id, mut agent, prompt, attachments) = prepare_agent_for_request(&state, &req).await?;

    let assistant_id = format!("assistant-{}", uuid::Uuid::new_v4());
    let mut assistant_text = String::new();
    let mut reasoning_text = String::new();

    send_event(&sender, "start", &serde_json::json!({
        "session_id": req.session_id,
        "assistant_id": assistant_id,
    }))
    .await;

    let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
    while let Some(event) = stream.next().await {
        match event {
            Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                reasoning_text.push_str(&text);
                send_event(&sender, "reasoning", &serde_json::json!({ "chunk": text })).await;
            }
            Ok(aither_agent::AgentEvent::Text(text)) => {
                assistant_text.push_str(&text);
                send_event(&sender, "text", &serde_json::json!({ "chunk": text })).await;
            }
            Ok(aither_agent::AgentEvent::ToolCallStart { id, name, arguments }) => {
                send_event(&sender, "tool_start", &serde_json::json!({
                    "id": id,
                    "name": name,
                    "arguments": arguments,
                }))
                .await;
            }
            Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
                send_event(&sender, "tool_end", &serde_json::json!({
                    "id": id,
                    "name": name,
                    "result": result,
                }))
                .await;
            }
            Ok(aither_agent::AgentEvent::Error(err)) => {
                send_event(&sender, "error", &serde_json::json!({ "message": err.to_string() })).await;
                return Ok(());
            }
            Err(err) => {
                send_event(&sender, "error", &serde_json::json!({ "message": err.to_string() })).await;
                return Ok(());
            }
            Ok(_) => {}
        }
    }

    persist_assistant_message(&state, &session_id, assistant_text.clone()).await?;

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
                "is_thinking": false
            }
        }),
    )
    .await;

    Ok(())
}

async fn poll_permission_request(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<PermissionPollResponse>> {
    let request = match tokio::time::timeout(Duration::from_secs(20), state.permission_approver.next_request()).await {
        Ok(req) => req,
        Err(_) => None,
    };
    Ok(Json(PermissionPollResponse { request }))
}

async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let _ = state.permission_approver.respond(&req.id, req.approved);
    Ok(Json(ApiHealth { ok: true }))
}

async fn get_ui_config(State(_state): State<Arc<UiServerState>>) -> SkyResult<Json<UiConfigResponse>> {
    let config = MayConfig::load().unwrap_or_default();
    Ok(Json(UiConfigResponse {
        ssh_servers: config.ssh_servers.clone(),
        max_iterations: config.max_iterations,
    }))
}

async fn get_memory(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<MemoryPayload>> {
    let memory_path = state.may_dir.join("memory.md");
    let content = async_fs::read_to_string(memory_path)
        .await
        .unwrap_or_default();
    Ok(Json(MemoryPayload { content }))
}

async fn save_memory(
    Json(payload): Json<MemoryPayload>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let memory_path = state.may_dir.join("memory.md");
    async_fs::write(memory_path, payload.content)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn delete_all_sessions(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<ApiHealth>> {
    let mut manager = state.session_manager.lock().await;
    let ids = manager
        .store()
        .list_sessions()
        .await
        .map_err(internal_error)?;

    for id in ids {
        manager
            .store()
            .delete_session(&id)
            .await
            .map_err(internal_error)?;
    }

    let _ = manager.create_session();
    Ok(Json(ApiHealth { ok: true }))
}

async fn add_ssh_server(
    Json(req): Json<SshServerRequest>,
    State(_state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let mut config = MayConfig::load().unwrap_or_default();
    config.upsert_ssh_server(SshServerConfig {
        name: req.name,
        target: req.target,
    });
    config.save().map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn remove_ssh_server(
    params: Params,
    State(_state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let mut config = MayConfig::load().unwrap_or_default();
    let _ = config.remove_ssh_server(name);
    config.save().map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn list_mcp_servers(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<McpServerInfo>>> {
    let manager = state.mcp_manager.lock().await;
    Ok(Json(manager.server_info()))
}

async fn add_mcp_server(
    Json(req): Json<McpAddRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let mut manager = state.mcp_manager.lock().await;
    let config = MayMcpServerConfig {
        command: req.command,
        args: req.args,
        url: req.url,
        env: std::collections::HashMap::new(),
    };
    manager
        .add_server(req.name, config)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn remove_mcp_server(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let mut manager = state.mcp_manager.lock().await;
    manager.remove_server(name).await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn reconnect_mcp_server(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let mut manager = state.mcp_manager.lock().await;
    manager.reconnect(name).await.map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn list_skills(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    let skills = state.skill_manager.list_skills().await.map_err(internal_error)?;
    Ok(Json(
        skills
            .into_iter()
            .map(|s| map_skill(s))
            .collect::<Vec<_>>(),
    ))
}

async fn install_skill(
    Json(req): Json<SkillInstallRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    let (owner, repo) = SkillManager::parse_github_repo(&req.repo).map_err(bad_request_error)?;
    let installed = state
        .skill_manager
        .install_from_github(owner, repo)
        .await
        .map_err(internal_error)?;
    Ok(Json(installed.into_iter().map(map_skill).collect()))
}

async fn delete_skill(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let dir = params.get("dir").map_err(bad_request_error)?;
    state
        .skill_manager
        .delete_skill(dir)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn static_or_index(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Response> {
    let raw = params.get("path").unwrap_or("");
    let relative = sanitize_relative_path(raw)
        .ok_or_else(|| SkyError::msg("Invalid path").set_status(StatusCode::BAD_REQUEST))?;

    let target = if relative.as_os_str().is_empty() {
        state.web_root.join("index.html")
    } else {
        state.web_root.join(&relative)
    };

    let final_path = if async_fs::metadata(&target).await.is_ok() {
        target
    } else {
        state.web_root.join("index.html")
    };

    let bytes = async_fs::read(&final_path).await.map_err(not_found_error)?;
    let mut response = Response::new(Body::from(bytes));

    if let Some(mime) = mime_guess::from_path(&final_path).first_raw() {
        if let Ok(value) = header::HeaderValue::from_str(mime) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }

    Ok(response)
}

async fn run_chat_collect(state: Arc<UiServerState>, req: ChatRequest) -> SkyResult<ChatResponse> {
    let (session_id, mut agent, prompt, attachments) = prepare_agent_for_request(&state, &req)
        .await
        .map_err(internal_error)?;

    let mut assistant_text = String::new();
    let mut reasoning_text = String::new();
    let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
    while let Some(event) = stream.next().await {
        match event {
            Ok(aither_agent::AgentEvent::Text(text)) => assistant_text.push_str(&text),
            Ok(aither_agent::AgentEvent::Reasoning(text)) => reasoning_text.push_str(&text),
            Ok(aither_agent::AgentEvent::Error(err)) => return Err(internal_error(err)),
            Err(err) => return Err(internal_error(err)),
            Ok(_) => {}
        }
    }

    persist_assistant_message(&state, &session_id, assistant_text.clone())
        .await
        .map_err(internal_error)?;

    Ok(ChatResponse {
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
        },
    })
}

async fn prepare_agent_for_request(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
) -> anyhow::Result<(SessionId, MayAgent, String, Vec<url::Url>)> {
    let session_id = SessionId::from_string(req.session_id.clone());

    let (current_model, sandbox_dir, history_messages) = {
        let mut manager = state.session_manager.lock().await;
        manager
            .load_session(&session_id)
            .await
            .map_err(anyhow::Error::from)?;

        let current_model = manager
            .current()
            .and_then(|s| s.model.clone())
            .or_else(|| MayConfig::load().ok().and_then(|c| c.last_model))
            .unwrap_or_else(|| "gpt-4".to_string());

        if let Some(session) = manager.current_mut() {
            session.history.push(ChatMessage::user(req.content.clone()));
        }
        manager.save_current().await.map_err(anyhow::Error::from)?;

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

    tokio::spawn(async move {
        while let Some(request) = ask_user_queue.next().await {
            let answers = request
                .args
                .clone()
                .into_questions()
                .into_iter()
                .map(|question| QuestionAnswer {
                    section: question.section,
                    answer: AnswerValue::Skipped,
                })
                .collect::<Vec<_>>();
            let _ = request.respond(answers);
        }
    });

    let config = MayConfig::load().unwrap_or_default();
    let mut agent = MayAgent::from_config_with_model_and_permissions(
        &config,
        sandbox_dir,
        state.may_dir.clone(),
        state.cwd.clone(),
        session_id.clone(),
        workspace_broker,
        ask_user_broker,
        &current_model,
        Some(state.permission_approver.clone()),
    )
    .await
    .map_err(anyhow::Error::from)?;

    for message in history_messages {
        agent.inner_mut().push_message(message);
    }

    let (prompt, attachments) = build_prompt_and_attachments(state, req)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    Ok((session_id, agent, prompt, attachments))
}

async fn persist_assistant_message(
    state: &Arc<UiServerState>,
    session_id: &SessionId,
    assistant_text: String,
) -> anyhow::Result<()> {
    let mut manager = state.session_manager.lock().await;
    manager
        .load_session(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(session) = manager.current_mut() {
        session.history.push(ChatMessage::assistant(assistant_text));
    }
    manager.save_current().await.map_err(anyhow::Error::from)
}

async fn send_event<T: Serialize>(sender: &skyzen::responder::sse::Sender, kind: &str, data: &T) {
    if let Ok(payload) = serde_json::to_string(data) {
        let _ = sender.send(Event::data(payload).event(kind)).await;
    }
}

fn session_from_history(id: &str, history: &crate::session::ChatHistory) -> ApiSession {
    let title = history
        .metadata
        .title
        .clone()
        .unwrap_or_else(|| "New Chat".to_string());
    let model = history
        .metadata
        .model
        .clone()
        .or_else(|| MayConfig::load().ok().and_then(|c| c.last_model))
        .unwrap_or_else(|| "gpt-4".to_string());

    let messages = history
        .messages()
        .iter()
        .enumerate()
        .map(|(idx, msg)| map_message(format!("msg-{idx}"), msg))
        .collect::<Vec<_>>();

    let last_modified = history
        .messages()
        .last()
        .and_then(|m| m.timestamp)
        .map(|t| t.to_string())
        .unwrap_or_else(|| jiff::Timestamp::now().to_string());

    let preview = history
        .messages()
        .last()
        .map(|m| truncate_preview(&m.content))
        .unwrap_or_else(|| "New conversation started".to_string());

    ApiSession {
        id: id.to_string(),
        title,
        last_modified,
        preview,
        messages,
        model,
        reasoning_effort: None,
    }
}

fn map_message(id: String, msg: &crate::session::ChatMessage) -> ApiMessage {
    ApiMessage {
        id,
        role: match msg.role {
            MessageRole::User => "user".to_string(),
            MessageRole::Assistant => "assistant".to_string(),
            MessageRole::System => "system".to_string(),
            MessageRole::Tool => "assistant".to_string(),
        },
        content: msg.content.clone(),
        timestamp: msg
            .timestamp
            .unwrap_or_else(jiff::Timestamp::now)
            .to_string(),
        attachments: Vec::new(),
        thinking: None,
        is_streaming: Some(false),
        is_thinking: Some(false),
    }
}

fn map_skill(skill: SkillInfo) -> SkillInfoDto {
    SkillInfoDto {
        name: skill.name,
        description: skill.description,
        dir_name: skill.dir_name,
        source: skill.source.to_string(),
    }
}

fn parse_arg_value(args: &[String], key: &str) -> Option<String> {
    for (idx, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix(&format!("{key}=")) {
            return Some(value.to_string());
        }
        if arg == key {
            if let Some(value) = args.get(idx + 1) {
                return Some(value.clone());
            }
        }
    }
    None
}

fn truncate_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "New conversation started".to_string();
    }

    let mut preview = String::new();
    for ch in trimmed.chars().take(100) {
        preview.push(ch);
    }
    if trimmed.chars().count() > 100 {
        preview.push_str("...");
    }
    preview
}

fn sanitize_relative_path(path: &str) -> Option<PathBuf> {
    let mut sanitized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(seg) => sanitized.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(sanitized)
}

async fn build_prompt_and_attachments(
    state: &UiServerState,
    req: &ChatRequest,
) -> SkyResult<(String, Vec<url::Url>)> {
    let mut prompt = req.content.clone();
    let mut attachments = Vec::new();

    for attachment in &req.attachments {
        if attachment.attachment_type == "image" && attachment.url.starts_with("data:image/") {
            if let Some(path) = save_data_image(state, &req.session_id, attachment).await? {
                let url = url::Url::from_file_path(path).map_err(|_| {
                    SkyError::msg("Failed to map image path to URL")
                        .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                })?;
                attachments.push(url);
                continue;
            }
        }

        if let Some(text) = &attachment.text_content {
            prompt.push_str("\n\n[Pasted text attachment]\n");
            prompt.push_str(text);
            continue;
        }

        prompt.push_str(&format!("\n\n[Attachment: {}]", attachment.name));
    }

    Ok((prompt, attachments))
}

async fn save_data_image(
    state: &UiServerState,
    session_id: &str,
    attachment: &ApiAttachment,
) -> SkyResult<Option<PathBuf>> {
    let Some((_, data)) = attachment.url.split_once(',') else {
        return Ok(None);
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(bad_request_error)?;

    let path = state
        .may_dir
        .join("sessions")
        .join(session_id)
        .join("images")
        .join(format!("{}-{}.png", attachment.id, uuid::Uuid::new_v4()));

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, bytes)
        .await
        .map_err(internal_error)?;
    Ok(Some(path))
}

fn internal_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
}

fn bad_request_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::BAD_REQUEST)
}

fn not_found_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::NOT_FOUND)
}
