use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_channel::{Receiver, Sender, bounded, unbounded};
use async_fs;
use base64::Engine;
use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::responder::{Sse, sse::Event};
use skyzen::routing::{CreateRouteNode, Params, Route, Router};
use skyzen::runtime::native;
use skyzen::utils::{Json, State};
use skyzen::{EmbeddedStaticDir, Error as SkyError, Result as SkyResult, StatusCode};
use tokio::sync::Mutex;

use crate::agent::{MayAgent, PermissionApprover, PermissionRequest};
use crate::ask_user::{self, AnswerValue, AskUserRequest, Question, QuestionAnswer};
use crate::config::{LoadBalanceStrategy, MayConfig, ProviderInstance, ProviderType, SshServerConfig};
use crate::mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
use crate::session::{ChatMessage, MessageRole, SessionId, SessionManager};
use crate::skill::{SkillInfo, SkillManager};
use crate::workspace_request;

static WEB_UI_DIST: include_dir::Dir<'static> =
    skyzen::embed_dir!(include_dir::include_dir!("$CARGO_MANIFEST_DIR/web-ui/dist"));

struct UiServerState {
    session_manager: Mutex<SessionManager>,
    mcp_manager: Mutex<McpManager>,
    skill_manager: SkillManager,
    permission_approver: PermissionApprover,
    ask_user_approver: AskUserApprover,
    cwd: PathBuf,
    may_dir: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AskUserRequestPayload {
    id: String,
    questions: Vec<Question>,
}

#[derive(Debug, serde::Serialize)]
struct AskUserPollResponse {
    request: Option<AskUserRequestPayload>,
}

#[derive(Debug, serde::Deserialize)]
struct UiQuestionAnswer {
    section: String,
    answer: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
struct AskUserRespondRequest {
    id: String,
    answers: Vec<UiQuestionAnswer>,
}

#[derive(Debug, Clone)]
struct AskUserApprover {
    request_tx: Sender<AskUserRequestPayload>,
    request_rx: Receiver<AskUserRequestPayload>,
    pending: Arc<Mutex<std::collections::HashMap<String, Sender<Vec<QuestionAnswer>>>>>,
}

impl AskUserApprover {
    fn new() -> Self {
        let (request_tx, request_rx) = unbounded();
        Self {
            request_tx,
            request_rx,
            pending: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    async fn enqueue(&self, request: AskUserRequest) {
        let id = uuid::Uuid::new_v4().to_string();
        let questions = request.args.clone().into_questions();
        let payload = AskUserRequestPayload {
            id: id.clone(),
            questions,
        };

        let (tx, rx) = bounded(1);
        self.pending.lock().await.insert(id.clone(), tx);

        if self.request_tx.send(payload).await.is_err() {
            let _ = self.pending.lock().await.remove(&id);
            let fallback = request
                .args
                .clone()
                .into_questions()
                .into_iter()
                .map(|question| QuestionAnswer {
                    section: question.section,
                    answer: AnswerValue::Skipped,
                })
                .collect::<Vec<_>>();
            let _ = request.respond(fallback);
            return;
        }

        let answers = rx.recv().await.unwrap_or_else(|_| {
            request
                .args
                .clone()
                .into_questions()
                .into_iter()
                .map(|question| QuestionAnswer {
                    section: question.section,
                    answer: AnswerValue::Skipped,
                })
                .collect::<Vec<_>>()
        });

        let _ = request.respond(answers);
    }

    async fn next_request(&self) -> Option<AskUserRequestPayload> {
        self.request_rx.recv().await.ok()
    }

    async fn respond(&self, request_id: &str, answers: Vec<QuestionAnswer>) {
        let sender = self.pending.lock().await.remove(request_id);
        if let Some(tx) = sender {
            let _ = tx.send(answers).await;
        }
    }
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

#[derive(Debug, Clone, serde::Serialize)]
struct ModelInfoItem {
    id: String,
    nickname: String,
    context_window: Option<u32>,
    kind: String,
    provider: String,
    abilities: Vec<String>,
    #[serde(default)]
    reasoning_efforts: Vec<String>,
    #[serde(default)]
    reasoning_effort_default: Option<String>,
    adaptive_reasoning: bool,
    #[serde(default)]
    reasoning_budget_tokens_min: Option<u32>,
    #[serde(default)]
    reasoning_budget_tokens_max: Option<u32>,
    #[serde(default)]
    embedding_dimensions: Option<u32>,
    #[serde(default)]
    image_max_resolution: Option<String>,
    #[serde(default)]
    reranker_max_documents: Option<u32>,
}

#[derive(Debug, serde::Serialize)]
struct ModelsResponse {
    models: Vec<String>,
    #[serde(default)]
    details: Vec<ModelInfoItem>,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiProviderConfig {
    id: String,
    provider_type: ProviderType,
    api_key: Option<String>,
    base_url: Option<String>,
    display_name: Option<String>,
    enabled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiModelsConfig {
    advanced: Vec<String>,
    balanced: Vec<String>,
    fast: Vec<String>,
    image_generation: Vec<String>,
    model_providers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiPreferencesConfig {
    load_balance_strategy: LoadBalanceStrategy,
    warn_on_model_change: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UiFullConfig {
    providers: Vec<UiProviderConfig>,
    models: UiModelsConfig,
    preferences: UiPreferencesConfig,
    last_model: Option<String>,
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

    let mut mcp_manager = McpManager::load().unwrap_or_else(|_| McpManager::new(McpConfig::default()));
    if mcp_manager.has_servers() {
        let _ = mcp_manager.connect_all().await;
    }

    let permission_approver = PermissionApprover::new();
    let ask_user_approver = AskUserApprover::new();

    let state = Arc::new(UiServerState {
        session_manager: Mutex::new(SessionManager::new()?),
        mcp_manager: Mutex::new(mcp_manager),
        skill_manager: SkillManager::new(&may_dir),
        permission_approver,
        ask_user_approver,
        cwd,
        may_dir,
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
        "/api/ask-user".route(Route::new((
            "/next".at(poll_ask_user_request),
            "/respond".post(respond_ask_user_request),
        ))),
        "/api/config".at(get_ui_config),
        "/api/config/full".at(get_full_config),
        "/api/config/full".post(save_full_config),
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
        EmbeddedStaticDir::new("/", &WEB_UI_DIST),
    ))
    .middleware(State(state))
    .build()
}

async fn health() -> SkyResult<Json<ApiHealth>> {
    Ok(Json(ApiHealth { ok: true }))
}

async fn models() -> SkyResult<Json<ModelsResponse>> {
    let config = MayConfig::load().unwrap_or_default();
    let mut model_map: std::collections::HashMap<String, ModelInfoItem> =
        std::collections::HashMap::new();

    for available in config.available_models() {
        model_map
            .entry(available.id.clone())
            .or_insert_with(|| {
                let meta = aither_models::model_meta(&available.id);
                let reasoning_meta = aither_models::reasoning_meta(&available.id);
                let specialized_meta = aither_models::specialized_meta(&available.id);
                let mut abilities = aither_models::metadata_capabilities(&available.id)
                    .iter()
                    .map(|cap| cap.to_string())
                    .collect::<Vec<_>>();
                abilities.sort();
                abilities.dedup();

                ModelInfoItem {
                    id: available.id.clone(),
                    nickname: meta
                        .map(|m| m.name.to_string())
                        .unwrap_or_else(|| available.id.clone()),
                    context_window: meta.map(|m| m.context_window),
                    kind: meta
                        .map(|m| m.kind.to_string())
                        .unwrap_or_else(|| "llm".to_string()),
                    provider: meta
                        .map(|m| m.provider.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    abilities,
                    reasoning_efforts: aither_models::reasoning_efforts(&available.id)
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    reasoning_effort_default: reasoning_meta.default_effort.map(|s| s.to_string()),
                    adaptive_reasoning: reasoning_meta.adaptive_reasoning,
                    reasoning_budget_tokens_min: reasoning_meta.budget_tokens_min,
                    reasoning_budget_tokens_max: reasoning_meta.budget_tokens_max,
                    embedding_dimensions: specialized_meta.embedding_dimensions,
                    image_max_resolution: specialized_meta.image_max_resolution.map(|s| s.to_string()),
                    reranker_max_documents: specialized_meta.reranker_max_documents,
                }
            });
    }

    for tier in crate::config::ModelTier::ALL {
        for model in config.models.models_for_tier(*tier) {
            model_map.entry(model.clone()).or_insert_with(|| {
                let meta = aither_models::model_meta(model);
                let reasoning_meta = aither_models::reasoning_meta(model);
                let specialized_meta = aither_models::specialized_meta(model);
                let mut abilities = aither_models::metadata_capabilities(model)
                    .iter()
                    .map(|cap| cap.to_string())
                    .collect::<Vec<_>>();
                abilities.sort();
                abilities.dedup();

                ModelInfoItem {
                    id: model.clone(),
                    nickname: meta
                        .map(|m| m.name.to_string())
                        .unwrap_or_else(|| model.clone()),
                    context_window: meta.map(|m| m.context_window),
                    kind: meta
                        .map(|m| m.kind.to_string())
                        .unwrap_or_else(|| "llm".to_string()),
                    provider: meta
                        .map(|m| m.provider.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    abilities,
                    reasoning_efforts: aither_models::reasoning_efforts(model)
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    reasoning_effort_default: reasoning_meta.default_effort.map(|s| s.to_string()),
                    adaptive_reasoning: reasoning_meta.adaptive_reasoning,
                    reasoning_budget_tokens_min: reasoning_meta.budget_tokens_min,
                    reasoning_budget_tokens_max: reasoning_meta.budget_tokens_max,
                    embedding_dimensions: specialized_meta.embedding_dimensions,
                    image_max_resolution: specialized_meta.image_max_resolution.map(|s| s.to_string()),
                    reranker_max_documents: specialized_meta.reranker_max_documents,
                }
            });
        }
    }

    let mut details = model_map.into_values().collect::<Vec<_>>();
    details.sort_by(|a, b| a.id.cmp(&b.id));
    let models = details.iter().map(|m| m.id.clone()).collect::<Vec<_>>();

    Ok(Json(ModelsResponse { models, details }))
}

async fn list_sessions(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<Vec<ApiSession>>> {
    let manager = state.session_manager.lock().await;
    let store = manager.store().clone();
    let session_ids = store.list_sessions().await.map_err(internal_error)?;

    let mut sessions = Vec::new();
    for id in session_ids {
        let Ok(history) = store.read_history(&id).await else {
            continue;
        };
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
    persist_last_model(&model).map_err(internal_error)?;

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
        .map_err(internal_error)?;

    if let Some(title) = req.title {
        manager
            .set_current_title(title)
            .await
            .map_err(internal_error)?;
    }
    if let Some(model) = req.model {
        manager
            .set_current_model(model.clone())
            .await
            .map_err(internal_error)?;
        persist_last_model(&model).map_err(internal_error)?;
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
    const MAX_RETRIES: usize = 2;

    for attempt in 0..=MAX_RETRIES {
        let (session_id, mut agent, prompt, attachments) =
            prepare_agent_for_request(&state, &req, attempt == 0).await?;

        let assistant_id = format!("assistant-{}", uuid::Uuid::new_v4());
        let mut assistant_text = String::new();
        let mut reasoning_text = String::new();
        let mut had_output = false;

        send_event(&sender, "start", &serde_json::json!({
            "session_id": req.session_id,
            "assistant_id": assistant_id,
        }))
        .await;

        let mut terminal_error: Option<String> = None;
        let mut stream = std::pin::pin!(agent.run(&prompt, attachments));
        while let Some(event) = stream.next().await {
            match event {
                Ok(aither_agent::AgentEvent::Reasoning(text)) => {
                    had_output = true;
                    reasoning_text.push_str(&text);
                    send_event(&sender, "reasoning", &serde_json::json!({ "chunk": text })).await;
                }
                Ok(aither_agent::AgentEvent::Text(text)) => {
                    had_output = true;
                    assistant_text.push_str(&text);
                    send_event(&sender, "text", &serde_json::json!({ "chunk": text })).await;
                }
                Ok(aither_agent::AgentEvent::ToolCallStart { id, name, arguments }) => {
                    had_output = true;
                    send_event(&sender, "tool_start", &serde_json::json!({
                        "id": id,
                        "name": name,
                        "arguments": arguments,
                    }))
                    .await;
                }
                Ok(aither_agent::AgentEvent::ToolCallEnd { id, name, result }) => {
                    had_output = true;
                    send_event(&sender, "tool_end", &serde_json::json!({
                        "id": id,
                        "name": name,
                        "result": result,
                    }))
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
                tokio::time::sleep(Duration::from_millis(400 * (attempt as u64 + 1))).await;
                continue;
            }
            send_event(&sender, "error", &serde_json::json!({ "message": err })).await;
            return Ok(());
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

        return Ok(());
    }

    send_event(&sender, "error", &serde_json::json!({ "message": "LLM endpoint unavailable after retries" })).await;
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

async fn poll_ask_user_request(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<AskUserPollResponse>> {
    let request =
        match tokio::time::timeout(Duration::from_secs(20), state.ask_user_approver.next_request())
            .await
        {
            Ok(req) => req,
            Err(_) => None,
        };
    Ok(Json(AskUserPollResponse { request }))
}

async fn respond_ask_user_request(
    Json(req): Json<AskUserRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let answers = req
        .answers
        .into_iter()
        .map(|answer| QuestionAnswer {
            section: answer.section,
            answer: parse_ui_answer_value(answer.answer),
        })
        .collect::<Vec<_>>();

    state.ask_user_approver.respond(&req.id, answers).await;
    Ok(Json(ApiHealth { ok: true }))
}

async fn get_ui_config(State(_state): State<Arc<UiServerState>>) -> SkyResult<Json<UiConfigResponse>> {
    let config = MayConfig::load().unwrap_or_default();
    Ok(Json(UiConfigResponse {
        ssh_servers: config.ssh_servers.clone(),
        max_iterations: config.max_iterations,
    }))
}

async fn get_full_config(State(_state): State<Arc<UiServerState>>) -> SkyResult<Json<UiFullConfig>> {
    let config = MayConfig::load().unwrap_or_default();

    let mut providers = config
        .provider
        .iter()
        .map(|(id, instance)| UiProviderConfig {
            id: id.clone(),
            provider_type: instance.resolve_type(id),
            api_key: instance.api_key.clone(),
            base_url: instance.base_url.clone(),
            display_name: instance.display_name.clone(),
            enabled: instance.enabled,
        })
        .collect::<Vec<_>>();
    providers.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(Json(UiFullConfig {
        providers,
        models: UiModelsConfig {
            advanced: config.models.advanced.clone(),
            balanced: config.models.balanced.clone(),
            fast: config.models.fast.clone(),
            image_generation: config.models.image_generation.clone(),
            model_providers: config.models.model_providers.clone(),
        },
        preferences: UiPreferencesConfig {
            load_balance_strategy: config.preferences.load_balance_strategy,
            warn_on_model_change: config.preferences.warn_on_model_change,
        },
        last_model: config.last_model.clone(),
        max_iterations: config.max_iterations,
    }))
}

async fn save_full_config(
    Json(req): Json<UiFullConfig>,
    State(_state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let mut config = MayConfig::load().unwrap_or_default();

    let mut provider_map = std::collections::HashMap::new();
    for p in req.providers {
        let mut instance = ProviderInstance::new(p.provider_type);
        instance.api_key = p.api_key.filter(|v| !v.trim().is_empty());
        instance.base_url = p.base_url.filter(|v| !v.trim().is_empty());
        instance.display_name = p.display_name.filter(|v| !v.trim().is_empty());
        instance.enabled = p.enabled;
        provider_map.insert(p.id, instance);
    }

    config.provider = provider_map;
    config.models.advanced = req.models.advanced;
    config.models.balanced = req.models.balanced;
    config.models.fast = req.models.fast;
    config.models.image_generation = req.models.image_generation;
    config.models.model_providers = req.models.model_providers;
    config.preferences.load_balance_strategy = req.preferences.load_balance_strategy;
    config.preferences.warn_on_model_change = req.preferences.warn_on_model_change;
    config.last_model = req.last_model;
    config.max_iterations = req.max_iterations;

    config.save().map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
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

async fn run_chat_collect(state: Arc<UiServerState>, req: ChatRequest) -> SkyResult<ChatResponse> {
    const MAX_RETRIES: usize = 2;

    for attempt in 0..=MAX_RETRIES {
        let (session_id, mut agent, prompt, attachments) = prepare_agent_for_request(&state, &req, attempt == 0)
            .await
            .map_err(internal_error)?;

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
                tokio::time::sleep(Duration::from_millis(400 * (attempt as u64 + 1))).await;
                continue;
            }
            return Err(internal_error(err));
        }

        persist_assistant_message(&state, &session_id, assistant_text.clone())
            .await
            .map_err(internal_error)?;

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
            },
        });
    }

    Err(internal_error("LLM endpoint unavailable after retries"))
}

async fn prepare_agent_for_request(
    state: &Arc<UiServerState>,
    req: &ChatRequest,
    persist_user_message: bool,
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

        if persist_user_message {
            if let Some(session) = manager.current_mut() {
                session.history.push(ChatMessage::user(req.content.clone()));
            }
            manager.save_current().await.map_err(anyhow::Error::from)?;
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
    tokio::spawn(async move {
        while let Some(request) = ask_user_queue.next().await {
            ask_user_approver.enqueue(request).await;
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

fn persist_last_model(model: &str) -> anyhow::Result<()> {
    let mut config = MayConfig::load().unwrap_or_default();
    config.last_model = Some(model.to_string());
    config.save().map_err(anyhow::Error::from)
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

fn parse_ui_answer_value(value: serde_json::Value) -> AnswerValue {
    match value {
        serde_json::Value::Null => AnswerValue::Skipped,
        serde_json::Value::String(s) => {
            if s.trim().is_empty() {
                AnswerValue::Skipped
            } else {
                AnswerValue::Custom(s)
            }
        }
        serde_json::Value::Array(items) => {
            let selected = items
                .into_iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                AnswerValue::Skipped
            } else {
                AnswerValue::Selected(selected)
            }
        }
        _ => AnswerValue::Skipped,
    }
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

fn is_retryable_llm_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("transport error")
        || lower.contains("connection error")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("temporarily unavailable")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
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

