use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_channel::{Receiver, Sender, bounded, unbounded};
use async_fs;
use base64::Engine;
use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::extract::{Extractor, Query};
use skyzen::responder::{Sse, sse::Event};
use skyzen::routing::{CreateRouteNode, Params, Route, Router};
use skyzen::runtime::native;
use skyzen::utils::{Json, State};
use skyzen::{EmbeddedStaticDir, Error as SkyError, Result as SkyResult, StatusCode};
use tokio::sync::Mutex;

use crate::agent::{MayAgent, PermissionApprover, PermissionRequest, generate_session_title};
use crate::ask_user::{self, AnswerValue, AskUserRequest, Question, QuestionAnswer};
use crate::config::{
    LoadBalanceStrategy, MayConfig, ProviderInstance, ProviderType, SshServerConfig,
};
use crate::copilot::{clear_copilot_credentials, has_copilot_token, save_copilot_token};
use crate::mcp::{MayMcpServerConfig, McpConfig, McpManager, McpServerInfo};
use crate::models::fetch_all_models;
use crate::session::{ChatMessage, MessageRole, SessionId, SessionManager};
use crate::skill::{SkillInfo, SkillManager};
use crate::workspace_request;
use aither_core::llm::Usage;
use aither_models::lookup as lookup_model_info;

static WEB_UI_DIST: include_dir::Dir<'static> =
    skyzen::embed_dir!(include_dir::include_dir!("$CARGO_MANIFEST_DIR/web-ui/dist"));

struct UiServerState {
    session_manager: Mutex<SessionManager>,
    mcp_manager: Mutex<McpManager>,
    skill_manager: SkillManager,
    permission_approver: PermissionApprover,
    ask_user_approver: AskUserApprover,
    active_streaming_sessions: Mutex<HashSet<String>>,
    request_logs: Mutex<VecDeque<RequestLogEntry>>,
    cwd: PathBuf,
    may_dir: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AskUserRequestPayload {
    id: String,
    questions: Vec<Question>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct RequestLogEntry {
    session_id: String,
    timestamp: String,
    model: String,
    provider_id: String,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
    context_window: Option<u32>,
    elapsed_ms: u64,
    ok: bool,
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct RequestLogsResponse {
    logs: Vec<RequestLogEntry>,
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

    async fn respond(&self, request_id: &str, answers: Vec<QuestionAnswer>) -> bool {
        let sender = self.pending.lock().await.remove(request_id);
        if let Some(tx) = sender {
            let _ = tx.send(answers).await;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct ApiHealth {
    ok: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct RequestStats {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    elapsed_ms: Option<u64>,
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
    upload_data_url: Option<String>,
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
    #[serde(default)]
    tool_event: Option<serde_json::Value>,
    #[serde(default)]
    stats: Option<RequestStats>,
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
    abilities: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct ModelsResponse {
    models: Vec<String>,
    #[serde(default)]
    details: Vec<ModelInfoItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProviderModelsItem {
    provider_id: String,
    models: Vec<String>,
    #[serde(default)]
    details: Vec<ModelInfoItem>,
}

#[derive(Debug, serde::Serialize)]
struct ProviderModelsResponse {
    providers: Vec<ProviderModelsItem>,
}

#[derive(Debug, serde::Deserialize)]
struct PermissionPollQuery {
    session_id: String,
}

#[derive(Debug, serde::Serialize)]
struct PermissionPollResponse {
    request: Option<PermissionRequest>,
    state: Option<&'static str>,
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
    ui_font: String,
    ui_theme: String,
}

#[derive(Debug, serde::Serialize)]
struct SessionRuntimeResponse {
    state: &'static str,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ArtifactEntry {
    path: String,
    name: String,
    size: u64,
    is_html: bool,
    fingerprint: String,
}

#[derive(Debug, serde::Serialize)]
struct SessionArtifactsResponse {
    artifacts: Vec<ArtifactEntry>,
}

#[derive(Debug, serde::Serialize)]
struct ArtifactStreamPayload {
    count: usize,
    signature: String,
}

#[derive(Debug, serde::Deserialize)]
struct ArtifactPreviewQuery {
    path: String,
}

#[derive(Debug, Clone)]
struct IfNoneMatchHeader(Option<String>);

impl Extractor for IfNoneMatchHeader {
    type Error = std::convert::Infallible;

    async fn extract(request: &mut skyzen::Request) -> Result<Self, Self::Error> {
        let value = request
            .headers()
            .get(skyzen::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        Ok(Self(value))
    }
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
    show_token_usage: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UiAppearanceConfig {
    font: String,
    theme: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UiFullConfig {
    providers: Vec<UiProviderConfig>,
    models: UiModelsConfig,
    preferences: UiPreferencesConfig,
    appearance: UiAppearanceConfig,
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

#[derive(Debug, serde::Deserialize)]
struct CopilotPollRequest {
    device_code: String,
}

#[derive(Debug, serde::Serialize)]
struct CopilotStatusResponse {
    authenticated: bool,
}

#[derive(Debug, serde::Serialize)]
struct CopilotStartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
}

#[derive(Debug, serde::Serialize)]
struct CopilotPollResponse {
    status: String,
    message: Option<String>,
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

    let mut mcp_manager =
        McpManager::load().unwrap_or_else(|_| McpManager::new(McpConfig::default()));
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
        active_streaming_sessions: Mutex::new(HashSet::new()),
        request_logs: Mutex::new(VecDeque::new()),
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
        "/api".route(Route::new((
            "/health".at(health),
            "/models".at(models),
            "/models/by-provider".at(models_by_provider),
            "/logs".route(Route::new(("/requests".at(get_request_logs),))),
            "/sessions".route(Route::new((
                "".at(list_sessions),
                "".post(create_session),
                "/delete-all".post(delete_all_sessions),
                "/{id}".put(update_session),
                "/{id}".delete(delete_session),
                "/{id}/runtime".at(get_session_runtime),
                "/{id}/artifacts".at(list_session_artifacts),
                "/{id}/artifacts/preview".at(get_artifact_preview),
                "/{id}/artifacts/stream".at(stream_session_artifacts),
            ))),
            "/chat".route(Route::new(("".post(chat), "/stream".post(chat_stream)))),
            "/permissions".route(Route::new((
                "/next".at(poll_permission_request),
                "/respond".post(respond_permission_request),
            ))),
            "/ask-user".route(Route::new((
                "/next".at(poll_ask_user_request),
                "/respond".post(respond_ask_user_request),
            ))),
            "/config".route(Route::new((
                "".at(get_ui_config),
                "/stream".at(config_stream),
                "/full".at(get_full_config),
                "/full".post(save_full_config),
            ))),
            "/memory".at(get_memory),
            "/memory".post(save_memory),
            "/ssh".route(Route::new((
                "".post(add_ssh_server),
                "/{name}".delete(remove_ssh_server),
            ))),
            "/mcp".route(Route::new((
                "".at(list_mcp_servers),
                "".post(add_mcp_server),
                "/{name}".delete(remove_mcp_server),
                "/{name}/reconnect".post(reconnect_mcp_server),
            ))),
            "/skills".route(Route::new((
                "".at(list_skills),
                "/install".post(install_skill),
                "/{dir}".delete(delete_skill),
            ))),
            "/copilot".route(Route::new((
                "/status".at(copilot_status),
                "/start".post(copilot_start),
                "/poll".post(copilot_poll),
                "/logout".post(copilot_logout),
            ))),
        ))),
        EmbeddedStaticDir::new("/", &WEB_UI_DIST),
    ))
    .middleware(State(state))
    .build()
}

async fn health() -> SkyResult<Json<ApiHealth>> {
    Ok(Json(ApiHealth { ok: true }))
}

async fn get_request_logs(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<RequestLogsResponse>> {
    let logs = state
        .request_logs
        .lock()
        .await
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(RequestLogsResponse { logs }))
}

async fn models() -> SkyResult<Json<ModelsResponse>> {
    let config = MayConfig::load().unwrap_or_default();

    let fetched = fetch_all_models(&config).await;
    let mut model_map = std::collections::HashMap::<String, ModelInfoItem>::new();

    for item in fetched {
        model_map.entry(item.id.clone()).or_insert(ModelInfoItem {
            id: item.id,
            nickname: item.nickname,
            context_window: item.context_window,
            abilities: item.abilities,
        });
    }

    for tier in crate::config::ModelTier::ALL {
        for model in config.models.models_for_tier(*tier) {
            model_map.entry(model.clone()).or_insert_with(|| {
                let fallback = lookup_model_info(model);
                ModelInfoItem {
                    id: model.clone(),
                    nickname: fallback
                        .map(|m| m.name.to_string())
                        .unwrap_or_else(|| model.clone()),
                    context_window: fallback.map(|m| m.context_window),
                    abilities: fallback
                        .map(|m| {
                            m.abilities
                                .iter()
                                .map(|a| format!("{:?}", a).to_lowercase())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                }
            });
        }
    }

    let mut details = model_map.into_values().collect::<Vec<_>>();
    details.sort_by(|a, b| a.id.cmp(&b.id));
    let models = details.iter().map(|d| d.id.clone()).collect::<Vec<_>>();

    Ok(Json(ModelsResponse { models, details }))
}

async fn models_by_provider() -> SkyResult<Json<ProviderModelsResponse>> {
    let config = MayConfig::load().unwrap_or_default();
    let fetched = fetch_all_models(&config).await;
    let mut grouped: std::collections::HashMap<String, Vec<ModelInfoItem>> =
        std::collections::HashMap::new();

    for item in fetched {
        grouped
            .entry(item.provider_id)
            .or_default()
            .push(ModelInfoItem {
                id: item.id,
                nickname: item.nickname,
                context_window: item.context_window,
                abilities: item.abilities,
            });
    }

    let mut providers = grouped
        .into_iter()
        .map(|(provider_id, mut details)| {
            details.sort_by(|a, b| a.id.cmp(&b.id));
            details.dedup_by(|a, b| a.id == b.id);
            let models = details.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
            ProviderModelsItem {
                provider_id,
                models,
                details,
            }
        })
        .collect::<Vec<_>>();

    providers.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    Ok(Json(ProviderModelsResponse { providers }))
}

async fn list_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<ApiSession>>> {
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
        title: current
            .title
            .clone()
            .unwrap_or_else(|| "New Chat".to_string()),
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
        let session_id = req.session_id.clone();
        state
            .active_streaming_sessions
            .lock()
            .await
            .insert(session_id.clone());
        let _ = stream_chat_events(state.clone(), req, sender).await;
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
                    send_event(&sender, "reasoning", &serde_json::json!({ "chunk": text })).await;
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
                tokio::time::sleep(Duration::from_millis(400 * (attempt as u64 + 1))).await;
                continue;
            }

            {
                let mut logs = state.request_logs.lock().await;
                let context_window = lookup_model_info(&current_model).map(|m| m.context_window);
                logs.push_front(RequestLogEntry {
                    session_id: session_id.as_str().to_string(),
                    timestamp: jiff::Timestamp::now().to_string(),
                    model: current_model.clone(),
                    provider_id: provider_id.clone(),
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    context_window,
                    elapsed_ms: started_at.elapsed().as_millis() as u64,
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

        let elapsed_ms = started_at.elapsed().as_millis() as u64;

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

async fn poll_permission_request(
    Query(query): Query<PermissionPollQuery>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<PermissionPollResponse>> {
    let session_id = query.session_id.as_str();
    if session_id.is_empty() {
        return Err(bad_request_error("session_id is required"));
    }

    let request = match tokio::time::timeout(
        Duration::from_secs(20),
        state
            .permission_approver
            .next_request_for_session(Some(session_id)),
    )
    .await
    {
        Ok(req) => req,
        Err(_) => None,
    };

    let (request, state_label) = if let Some(req) = request {
        (Some(req), Some("waiting_permission"))
    } else if state
        .permission_approver
        .snapshot_session_runtime(Some(session_id))
        .is_some()
    {
        (
            state
                .permission_approver
                .snapshot_session_runtime(Some(session_id)),
            Some("waiting_permission"),
        )
    } else {
        (None, None)
    };

    Ok(Json(PermissionPollResponse {
        request,
        state: state_label,
    }))
}

async fn respond_permission_request(
    Json(req): Json<PermissionRespondRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let responded = state.permission_approver.respond(&req.id, req.approved);
    if !responded {
        return Err(bad_request_error(
            "permission request not found or already resolved",
        ));
    }
    Ok(Json(ApiHealth { ok: true }))
}

async fn get_session_runtime(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    if state
        .permission_approver
        .snapshot_session_runtime(Some(session_id.as_str()))
        .is_some()
    {
        return Ok(Json(SessionRuntimeResponse {
            state: "waiting_permission",
        }));
    }

    let is_streaming = state
        .active_streaming_sessions
        .lock()
        .await
        .contains(session_id.as_str());

    Ok(Json(SessionRuntimeResponse {
        state: if is_streaming { "streaming" } else { "idle" },
    }))
}

async fn collect_session_artifacts(
    state: &UiServerState,
    session_id: &str,
) -> SkyResult<Vec<ArtifactEntry>> {
    let artifacts_dir = state
        .may_dir
        .join("sessions")
        .join(session_id)
        .join("workdir")
        .join("artifacts");

    let mut entries = Vec::new();

    if async_fs::metadata(&artifacts_dir).await.is_ok() {
        let mut stack = vec![artifacts_dir.clone()];

        while let Some(dir) = stack.pop() {
            let mut rd = async_fs::read_dir(&dir).await.map_err(internal_error)?;
            while let Some(entry) = rd.next().await {
                let entry = entry.map_err(internal_error)?;
                let path = entry.path();
                let meta = entry.metadata().await.map_err(internal_error)?;

                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }

                if !meta.is_file() {
                    continue;
                }

                let rel = path.strip_prefix(&artifacts_dir).map_err(internal_error)?;
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let name = rel
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("artifact")
                    .to_string();
                let lower = name.to_lowercase();
                let is_html = lower.ends_with(".html") || lower.ends_with(".htm");

                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);

                entries.push(ArtifactEntry {
                    path: rel_str,
                    name,
                    size: meta.len(),
                    is_html,
                    fingerprint: format!("{}-{}", meta.len(), modified),
                });
            }
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn artifacts_signature(entries: &[ArtifactEntry]) -> String {
    entries
        .iter()
        .map(|e| format!("{}:{}:{}", e.path, e.fingerprint, e.is_html))
        .collect::<Vec<_>>()
        .join("|")
}

async fn list_session_artifacts(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionArtifactsResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let entries = collect_session_artifacts(&state, &session_id).await?;
    Ok(Json(SessionArtifactsResponse { artifacts: entries }))
}

async fn stream_session_artifacts(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Sse> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let (sender, sse) = Sse::channel();
    let state_cloned = state.clone();

    tokio::spawn(async move {
        let mut last_sig = String::new();
        loop {
            if let Ok(entries) = collect_session_artifacts(&state_cloned, &session_id).await {
                let sig = artifacts_signature(&entries);
                if sig != last_sig {
                    send_event(
                        &sender,
                        "artifacts",
                        &ArtifactStreamPayload {
                            count: entries.len(),
                            signature: sig.clone(),
                        },
                    )
                    .await;
                    last_sig = sig;
                }
            }

            send_event(&sender, "heartbeat", &serde_json::json!({ "ok": true })).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    });

    Ok(sse)
}

async fn get_artifact_preview(
    params: Params,
    Query(query): Query<ArtifactPreviewQuery>,
    if_none_match: IfNoneMatchHeader,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let requested = query.path.trim();
    if requested.is_empty() {
        return Err(bad_request_error("artifact path is required"));
    }

    let rel = PathBuf::from(requested);
    if rel.is_absolute() {
        return Err(bad_request_error("artifact path must be relative"));
    }
    for comp in rel.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return Err(bad_request_error("artifact path traversal is not allowed"));
        }
    }

    let artifacts_dir = state
        .may_dir
        .join("sessions")
        .join(&session_id)
        .join("workdir")
        .join("artifacts");
    let full = artifacts_dir.join(&rel);

    let canonical_artifacts = async_fs::canonicalize(&artifacts_dir)
        .await
        .map_err(internal_error)?;
    let canonical_full = async_fs::canonicalize(&full)
        .await
        .map_err(|_| bad_request_error("artifact not found"))?;
    if !canonical_full.starts_with(&canonical_artifacts) {
        return Err(bad_request_error("artifact path traversal is not allowed"));
    }

    let meta = async_fs::metadata(&canonical_full)
        .await
        .map_err(internal_error)?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag_value = format!("\"{}-{}\"", meta.len(), modified);

    let incoming_etag = if_none_match
        .0
        .as_deref()
        .map(str::trim)
        .map(|v| v.trim_start_matches("W/").trim_matches('"'));
    let current_etag = etag_value.trim_start_matches("W/").trim_matches('"');

    if incoming_etag.is_some_and(|v| v == "*" || v == current_etag) {
        let mut not_modified = skyzen::Response::new(skyzen::Body::empty());
        *not_modified.status_mut() = StatusCode::NOT_MODIFIED;
        if let Ok(etag_header) = skyzen::header::HeaderValue::from_str(&etag_value) {
            not_modified
                .headers_mut()
                .insert(skyzen::header::ETAG, etag_header);
        }
        if let Ok(cache_control) = skyzen::header::HeaderValue::from_str("private, no-cache") {
            not_modified
                .headers_mut()
                .insert(skyzen::header::CACHE_CONTROL, cache_control);
        }
        return Ok(not_modified);
    }

    let bytes = async_fs::read(&canonical_full)
        .await
        .map_err(internal_error)?;

    let ext = rel
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_lowercase();

    let content_type = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" | "text" | "log" | "csv" | "json" | "xml" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
    .to_string();

    let mut response = skyzen::Response::new(skyzen::Body::from(bytes));
    if let Ok(content_type_header) = skyzen::header::HeaderValue::from_str(&content_type) {
        response
            .headers_mut()
            .insert(skyzen::header::CONTENT_TYPE, content_type_header);
    }
    if let Ok(etag_header) = skyzen::header::HeaderValue::from_str(&etag_value) {
        response
            .headers_mut()
            .insert(skyzen::header::ETAG, etag_header);
    }
    if let Ok(cache_control) = skyzen::header::HeaderValue::from_str("private, no-cache") {
        response
            .headers_mut()
            .insert(skyzen::header::CACHE_CONTROL, cache_control);
    }

    Ok(response)
}

async fn poll_ask_user_request(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<AskUserPollResponse>> {
    let request = match tokio::time::timeout(
        Duration::from_secs(20),
        state.ask_user_approver.next_request(),
    )
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

    let responded = state.ask_user_approver.respond(&req.id, answers).await;
    if !responded {
        return Err(bad_request_error(
            "ask_user request not found or already resolved",
        ));
    }

    Ok(Json(ApiHealth { ok: true }))
}

async fn get_ui_config(
    State(_state): State<Arc<UiServerState>>,
) -> SkyResult<Json<UiConfigResponse>> {
    let config = MayConfig::load().unwrap_or_default();
    Ok(Json(UiConfigResponse {
        ssh_servers: config.ssh_servers.clone(),
        max_iterations: config.max_iterations,
        ui_font: config.appearance.font.clone(),
        ui_theme: config.appearance.theme.clone(),
    }))
}

async fn config_stream() -> SkyResult<Sse> {
    let (sender, sse) = Sse::channel();

    tokio::spawn(async move {
        let mut last_sig = String::new();
        loop {
            let config = MayConfig::load().unwrap_or_default();
            let sig = format!(
                "{}|{}|{}|{}|{}",
                config.appearance.font,
                config.appearance.theme,
                config.max_iterations,
                config.ssh_servers.len(),
                config.last_model.clone().unwrap_or_default()
            );

            if sig != last_sig {
                send_event(
                    &sender,
                    "config",
                    &serde_json::json!({
                        "ui_font": config.appearance.font,
                        "ui_theme": config.appearance.theme,
                    }),
                )
                .await;
                last_sig = sig;
            }

            send_event(&sender, "heartbeat", &serde_json::json!({ "ok": true })).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    Ok(sse)
}

async fn get_full_config(
    State(_state): State<Arc<UiServerState>>,
) -> SkyResult<Json<UiFullConfig>> {
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
            show_token_usage: config.preferences.show_token_usage,
        },
        appearance: UiAppearanceConfig {
            font: config.appearance.font.clone(),
            theme: config.appearance.theme.clone(),
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
    config.preferences.show_token_usage = req.preferences.show_token_usage;
    config.appearance.font = req.appearance.font;
    config.appearance.theme = req.appearance.theme;
    config.last_model = req.last_model;
    config.max_iterations = req.max_iterations;

    config.save().map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn get_memory(State(state): State<Arc<UiServerState>>) -> SkyResult<Json<MemoryPayload>> {
    let memory_path = state.may_dir.join("MEMORY.md");
    let content = async_fs::read_to_string(memory_path)
        .await
        .unwrap_or_default();
    Ok(Json(MemoryPayload { content }))
}

async fn save_memory(
    Json(payload): Json<MemoryPayload>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
    let memory_path = state.may_dir.join("MEMORY.md");
    async_fs::write(memory_path, payload.content)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
}

async fn delete_all_sessions(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ApiHealth>> {
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

async fn list_skills(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<SkillInfoDto>>> {
    let skills = state
        .skill_manager
        .list_skills()
        .await
        .map_err(internal_error)?;
    Ok(Json(
        skills.into_iter().map(|s| map_skill(s)).collect::<Vec<_>>(),
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
                Ok(aither_agent::AgentEvent::ToolCallEnd { id, result, .. }) => {
                    had_output = true;
                    if let Ok(tool_result_json) = serde_json::to_string(&result) {
                        let mut manager = state.session_manager.lock().await;
                        if manager.load_session(&session_id).await.is_ok() {
                            if let Some(session) = manager.current_mut() {
                                session
                                    .history
                                    .push(ChatMessage::tool(id, tool_result_json));
                            }
                            let _ = manager.save_current().await;
                        }
                    }
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

            {
                let mut logs = state.request_logs.lock().await;
                let context_window = lookup_model_info(&current_model).map(|m| m.context_window);
                logs.push_front(RequestLogEntry {
                    session_id: session_id.as_str().to_string(),
                    timestamp: jiff::Timestamp::now().to_string(),
                    model: current_model.clone(),
                    provider_id: provider_id.clone(),
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    context_window,
                    elapsed_ms: started_at.elapsed().as_millis() as u64,
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

        let elapsed_ms = started_at.elapsed().as_millis() as u64;

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

        let current_model = manager
            .current()
            .and_then(|s| s.model.clone())
            .or_else(|| MayConfig::load().ok().and_then(|c| c.last_model))
            .unwrap_or_else(|| "gpt-4".to_string());

        if persist_user_message {
            if let Some(session) = manager.current_mut() {
                session.history.push(ChatMessage::user(req.content.clone()));
                if session.title.is_none() {
                    let title = truncate_preview(&req.content);
                    session.title = Some(title.clone());
                    session.history.metadata.title = Some(title);
                }
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

async fn append_history_message(
    state: &Arc<UiServerState>,
    session_id: &SessionId,
    message: ChatMessage,
) -> anyhow::Result<()> {
    let mut manager = state.session_manager.lock().await;
    manager
        .load_session(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(session) = manager.current_mut() {
        session.history.push(message);
    }
    manager.save_current().await.map_err(anyhow::Error::from)
}

async fn send_event<T: Serialize>(sender: &skyzen::responder::sse::Sender, kind: &str, data: &T) {
    if let Ok(payload) = serde_json::to_string(data) {
        let _ = sender.send(Event::data(payload).event(kind)).await;
    }
}

async fn copilot_status() -> SkyResult<Json<CopilotStatusResponse>> {
    Ok(Json(CopilotStatusResponse {
        authenticated: has_copilot_token(),
    }))
}

async fn copilot_start() -> SkyResult<Json<CopilotStartResponse>> {
    let device = aither_copilot::auth::request_device_code()
        .await
        .map_err(internal_error)?;
    Ok(Json(CopilotStartResponse {
        device_code: device.device_code,
        user_code: device.user_code,
        verification_uri: device.verification_uri,
        interval: device.interval,
    }))
}

async fn copilot_poll(Json(req): Json<CopilotPollRequest>) -> SkyResult<Json<CopilotPollResponse>> {
    match aither_copilot::auth::try_get_token(&req.device_code).await {
        Ok(token) => {
            save_copilot_token(&token).map_err(internal_error)?;
            Ok(Json(CopilotPollResponse {
                status: "success".to_string(),
                message: None,
            }))
        }
        Err(aither_copilot::CopilotError::AuthorizationPending) => Ok(Json(CopilotPollResponse {
            status: "pending".to_string(),
            message: None,
        })),
        Err(aither_copilot::CopilotError::DeviceCodeExpired) => Ok(Json(CopilotPollResponse {
            status: "expired".to_string(),
            message: Some("Device code expired. Please restart authentication.".to_string()),
        })),
        Err(aither_copilot::CopilotError::AccessDenied) => Ok(Json(CopilotPollResponse {
            status: "denied".to_string(),
            message: Some("Authorization denied by user.".to_string()),
        })),
        Err(err) => Ok(Json(CopilotPollResponse {
            status: "error".to_string(),
            message: Some(err.to_string()),
        })),
    }
}

async fn copilot_logout() -> SkyResult<Json<ApiHealth>> {
    clear_copilot_credentials().map_err(internal_error)?;
    Ok(Json(ApiHealth { ok: true }))
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
    let (role, tool_event, content) = match msg.role {
        MessageRole::User => ("user".to_string(), None, msg.content.clone()),
        MessageRole::Assistant => ("assistant".to_string(), None, msg.content.clone()),
        MessageRole::System => ("system".to_string(), None, msg.content.clone()),
        MessageRole::Tool => (
            "assistant".to_string(),
            serde_json::from_str::<serde_json::Value>(&msg.content).ok(),
            String::new(),
        ),
    };

    ApiMessage {
        id,
        role,
        content,
        timestamp: msg
            .timestamp
            .unwrap_or_else(jiff::Timestamp::now)
            .to_string(),
        attachments: Vec::new(),
        thinking: None,
        is_streaming: Some(false),
        is_thinking: Some(false),
        tool_event,
        stats: None,
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

const INLINE_PASTED_TEXT_THRESHOLD: usize = 1200;

async fn build_prompt_and_attachments(
    state: &UiServerState,
    req: &ChatRequest,
) -> SkyResult<(String, Vec<url::Url>)> {
    let mut prompt = req.content.clone();
    let mut attachments = Vec::new();
    let mut xml_items = Vec::new();

    for attachment in &req.attachments {
        if let Some(data_url) = attachment.upload_data_url.as_deref().or_else(|| {
            if attachment.attachment_type == "image" && attachment.url.starts_with("data:") {
                Some(attachment.url.as_str())
            } else {
                None
            }
        }) {
            if let Some((saved_path, relative_path)) =
                save_upload_data_url(state, &req.session_id, attachment, data_url).await?
            {
                xml_items.push(format!(
                    "<attachment name=\"{}\" type=\"{}\" path=\"{}\" />",
                    xml_escape(&attachment.name),
                    xml_escape(&attachment.attachment_type),
                    xml_escape(&relative_path),
                ));

                if attachment.attachment_type == "image" {
                    let url = url::Url::from_file_path(saved_path).map_err(|_| {
                        SkyError::msg("Failed to map image path to URL")
                            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                    })?;
                    attachments.push(url);
                }
                continue;
            }
        }

        if let Some(text) = &attachment.text_content {
            if text.chars().count() <= INLINE_PASTED_TEXT_THRESHOLD {
                prompt.push_str("\n\n<pasted_text>");
                prompt.push_str(&xml_escape(text));
                prompt.push_str("</pasted_text>");
            } else {
                let (saved_path, relative_path) =
                    save_large_pasted_text(state, &req.session_id, text).await?;
                xml_items.push(format!(
                    "<attachment name=\"{}\" type=\"document\" path=\"{}\" />",
                    xml_escape(&attachment.name),
                    xml_escape(&relative_path),
                ));

                if attachment.attachment_type == "image" {
                    let url = url::Url::from_file_path(saved_path).map_err(|_| {
                        SkyError::msg("Failed to map image path to URL")
                            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                    })?;
                    attachments.push(url);
                }
            }
            continue;
        }

        xml_items.push(format!(
            "<attachment name=\"{}\" type=\"{}\" />",
            xml_escape(&attachment.name),
            xml_escape(&attachment.attachment_type),
        ));
    }

    if !xml_items.is_empty() {
        prompt.push_str("\n\n<attachments>\n");
        prompt.push_str(&xml_items.join("\n"));
        prompt.push_str("\n</attachments>");
    }

    Ok((prompt, attachments))
}

async fn save_upload_data_url(
    state: &UiServerState,
    session_id: &str,
    attachment: &ApiAttachment,
    data_url: &str,
) -> SkyResult<Option<(PathBuf, String)>> {
    let Some((header, data)) = data_url.split_once(',') else {
        return Ok(None);
    };

    if !header.starts_with("data:") {
        return Ok(None);
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(bad_request_error)?;

    let ext = guess_extension(header, &attachment.name);
    let base_name = sanitize_upload_stem(&attachment.name);
    let (path, relative_path) = allocate_upload_path(state, session_id, &base_name, &ext).await?;

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, bytes)
        .await
        .map_err(internal_error)?;

    Ok(Some((path, relative_path)))
}

async fn save_large_pasted_text(
    state: &UiServerState,
    session_id: &str,
    text: &str,
) -> SkyResult<(PathBuf, String)> {
    let filename_base = generate_pasted_text_filename(state, text).await;
    let (path, relative_path) =
        allocate_upload_path(state, session_id, &filename_base, "txt").await?;

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, text.as_bytes())
        .await
        .map_err(internal_error)?;

    Ok((path, relative_path))
}

async fn generate_pasted_text_filename(state: &UiServerState, text: &str) -> String {
    let config = MayConfig::load().unwrap_or_default();
    let snippet: String = text.chars().take(600).collect();

    let generated = generate_session_title(&config, &snippet, "").await;
    let base = generated
        .as_deref()
        .map(sanitize_upload_stem)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pasted-text".to_string());

    if base.is_empty() {
        "pasted-text".to_string()
    } else {
        base
    }
}

async fn allocate_upload_path(
    state: &UiServerState,
    session_id: &str,
    base_name: &str,
    ext: &str,
) -> SkyResult<(PathBuf, String)> {
    let uploads_dir = state
        .may_dir
        .join("sessions")
        .join(session_id)
        .join("uploads");

    async_fs::create_dir_all(&uploads_dir)
        .await
        .map_err(internal_error)?;

    let clean_ext = ext.trim_start_matches('.');
    let mut file_name = format!("{}.{}", base_name, clean_ext);
    let mut path = uploads_dir.join(&file_name);

    if async_fs::metadata(&path).await.is_ok() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        file_name = format!("{}-{}.{}", base_name, ts, clean_ext);
        path = uploads_dir.join(&file_name);
    }

    Ok((path, format!("uploads/{}", file_name)))
}

fn guess_extension(data_url_header: &str, fallback_name: &str) -> String {
    let lower = data_url_header.to_lowercase();
    if lower.contains("image/png") {
        return "png".to_string();
    }
    if lower.contains("image/jpeg") || lower.contains("image/jpg") {
        return "jpg".to_string();
    }
    if lower.contains("image/webp") {
        return "webp".to_string();
    }
    if lower.contains("application/pdf") {
        return "pdf".to_string();
    }
    if lower.contains("text/plain") {
        return "txt".to_string();
    }

    Path::new(fallback_name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("bin")
        .to_string()
}

fn sanitize_upload_stem(raw: &str) -> String {
    let stem = Path::new(raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);

    let mut out = String::new();
    for ch in stem.chars() {
        let safe = match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        };
        out.push(safe);
    }

    let cleaned = out.trim_matches([' ', '.', '-']).to_string();
    if cleaned.is_empty() {
        "upload".to_string()
    } else {
        cleaned
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn internal_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
}

fn bad_request_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::BAD_REQUEST)
}
