//! Daemon wiring and REST API handlers for May Bots (`/api/bots/*`).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aither_core::llm::model::Parameters;
use arc_swap::{ArcSwap, ArcSwapOption};
use may_protocol::bot::{
    BotApprovalItem, BotChatMessage, BotDelegateSummary, BotInboxEntry, BotMemoryCommit,
    BotMemoryNode, BotScheduleItem, BotSummary, BotTraceEntry, CreateBotRequest,
    ResolveBotApprovalRequest, SendBotMessageRequest, UpdateBotRequest,
};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::agent::{ProviderLlm, Providers, resolve_provider_runtime};
use crate::bot::{
    BashDelegateRunner, BotModelFactory, BotService, BotServiceError, ConfinedShellCuratorSession,
    CuratorFactory, CuratorSession, SessionBridge,
};
use crate::cloud::access::PushedProviders;
use crate::config::MayConfig;
use crate::model_selector::ModelSelector;
use crate::repo_search::RepoSearchCollections;
use crate::secrets::SecretRedactor;
use crate::session::MayHomePaths;
use crate::tools::{ToolDistribution, ToolEnvironments};

use super::error::{bad_request_error, internal_error};
use super::model_resolution::resolve_default_model;
use super::state::DaemonState;
use super::types::{API_OK, ApiHealth};

/// Concrete daemon-hosted [`BotService`] using [`ProviderLlm`] and [`RepoSearchCollections`].
pub type DaemonBotService = BotService<ProviderLlm, RepoSearchCollections>;

/// Resolves the LLM and request parameters for a bot or its delegated workers
/// / curator using the daemon's live config and pushed providers.
#[derive(Clone)]
pub struct DaemonBotModelFactory {
    config: Arc<ArcSwap<MayConfig>>,
    pushed_providers: PushedProviders,
    may_dir: PathBuf,
}

impl std::fmt::Debug for DaemonBotModelFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonBotModelFactory")
            .field("may_dir", &self.may_dir)
            .finish_non_exhaustive()
    }
}

impl DaemonBotModelFactory {
    /// Creates a new model factory reading from the daemon's live config.
    #[must_use]
    pub const fn new(
        config: Arc<ArcSwap<MayConfig>>,
        pushed_providers: PushedProviders,
        may_dir: PathBuf,
    ) -> Self {
        Self {
            config,
            pushed_providers,
            may_dir,
        }
    }

    async fn resolve_selector_str(
        &self,
        model: Option<&str>,
    ) -> anyhow::Result<(ProviderLlm, Parameters)> {
        let cfg = self.config.load();
        let raw_selector = match model {
            Some(m) if !m.trim().is_empty() => m.to_string(),
            _ => resolve_default_model(&cfg, None)
                .ok_or_else(|| anyhow::anyhow!("no default model configured for May Bot"))?,
        };
        let selector = raw_selector.parse::<ModelSelector>()?;
        let providers = Providers::new(&cfg, &self.pushed_providers);
        let redactor = SecretRedactor::new(&self.may_dir);
        let resolved = resolve_provider_runtime(providers, &selector, 0, &redactor).await?;
        let params = Parameters::default();
        Ok((resolved.llm, params))
    }

    /// Resolves the model for a delegated worker or curator running on behalf of `bot_id`.
    pub async fn resolve_for_bot_id(
        &self,
        db: &crate::db::ChatDb,
        bot_id: &str,
    ) -> anyhow::Result<(ProviderLlm, Parameters)> {
        let bot = db.get_bot(bot_id).await?;
        let model = bot.as_ref().and_then(|b| b.model.as_deref());
        self.resolve_selector_str(model).await
    }
}

impl BotModelFactory<ProviderLlm> for DaemonBotModelFactory {
    fn build_model(
        &self,
        bot: &crate::db::BotRow,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<(ProviderLlm, Parameters)>> + Send + '_>> {
        let model = bot.model.clone();
        Box::pin(async move { self.resolve_selector_str(model.as_deref()).await })
    }
}

/// Builds confined bash-first curator sessions for `store_fact` curation.
pub struct DaemonCuratorFactory {
    db: crate::db::ChatDb,
    may_dir: PathBuf,
    tool_distribution: Arc<ToolDistribution>,
    models: DaemonBotModelFactory,
}

impl std::fmt::Debug for DaemonCuratorFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonCuratorFactory")
            .field("may_dir", &self.may_dir)
            .finish_non_exhaustive()
    }
}

impl DaemonCuratorFactory {
    /// Creates a new curator factory.
    #[must_use]
    pub const fn new(
        db: crate::db::ChatDb,
        may_dir: PathBuf,
        tool_distribution: Arc<ToolDistribution>,
        models: DaemonBotModelFactory,
    ) -> Self {
        Self {
            db,
            may_dir,
            tool_distribution,
            models,
        }
    }
}

impl CuratorFactory for DaemonCuratorFactory {
    fn open_session(
        &self,
        bot_id: &str,
        staging_dir: PathBuf,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn CuratorSession>, crate::bot::BotMemoryError>>
                + Send
                + '_,
        >,
    > {
        let bot_id = bot_id.to_string();
        Box::pin(async move {
            let (llm, params) = self
                .models
                .resolve_for_bot_id(&self.db, &bot_id)
                .await
                .map_err(|e| crate::bot::BotMemoryError::Curation(e.to_string()))?;
            let tools_dir = MayHomePaths::new(self.may_dir.clone()).tools_dir();
            let tools = ToolEnvironments::prepare_host(&self.tool_distribution, &tools_dir, false)
                .await
                .map_err(|e| crate::bot::BotMemoryError::Curation(e.to_string()))?;
            let session =
                ConfinedShellCuratorSession::build(llm, params, staging_dir, tools).await?;
            Ok(Box::new(session) as Box<dyn CuratorSession>)
        })
    }
}

/// Bridges `create_session` and `stop_session` native bot tools onto the daemon.
#[derive(Clone, Default)]
pub struct DaemonSessionBridge {
    state: Arc<ArcSwapOption<DaemonState>>,
}

impl std::fmt::Debug for DaemonSessionBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonSessionBridge")
            .finish_non_exhaustive()
    }
}

impl DaemonSessionBridge {
    /// Creates an empty bridge; call [`Self::bind`] once [`DaemonState`] is built.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(ArcSwapOption::empty()),
        }
    }

    /// Binds the live [`DaemonState`].
    pub fn bind(&self, state: Arc<DaemonState>) {
        self.state.store(Some(state));
    }
}

impl SessionBridge for DaemonSessionBridge {
    fn create_session(
        &self,
        bot_id: String,
        prompt: String,
        title: Option<String>,
        profile_id: Option<String>,
        model: Option<String>,
        cwd: Option<String>,
        repo: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let Some(state) = self.state.load_full() else {
                return Err("daemon state not bound yet".to_string());
            };
            let cfg = state.config_snapshot();
            let resolved_model = resolve_default_model(&cfg, model)
                .ok_or_else(|| "no model configured to create a session".to_string())?;
            let target_profile = profile_id.unwrap_or_else(|| state.default_profile_id.clone());
            let session_id = super::sessions::create_session_for_profile(
                &state,
                super::sessions::SessionName::Generate,
                Some(target_profile.as_str()),
                &resolved_model,
            )
            .await
            .map_err(|e| e.to_string())?;

            if let Some(title_text) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let _ = state
                    .db()
                    .update_session(&session_id, Some(title_text), None, None)
                    .await;
            }
            if let Some(cwd_path) = cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let _ = state.db().set_session_cwd(&session_id, cwd_path).await;
            }
            if let Some(repo_spec) = repo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let _ = state
                    .db()
                    .set_session_repo(&session_id, Some(repo_spec))
                    .await;
            }

            state
                .db()
                .register_bot_supervised_session(&session_id, &bot_id)
                .await
                .map_err(|e| e.to_string())?;
            state.notify_ui_events(crate::ui_event_bus::UiEventTopics::PROFILE_SESSIONS);

            super::chat::deliver_user_message(
                &state,
                super::types::ChatRequest {
                    session_id: session_id.clone(),
                    content: prompt,
                    attachments: Vec::new(),
                    hidden_context_xml: None,
                    delivery_mode: crate::session::PendingUserMessageMode::Queue,
                    pending_entry_id: None,
                },
            )
            .await
            .map_err(|e| e.to_string())?;

            Ok(session_id)
        })
    }

    fn stop_session(
        &self,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let Some(state) = self.state.load_full() else {
                return Err("daemon state not bound yet".to_string());
            };
            let _ = state
                .runtime_service
                .runs()
                .cancel(&session_id, crate::session::CancelReason::User);
            Ok(())
        })
    }
}

/// Builds the production [`BashDelegateRunner`] wired to `models`.
#[must_use]
pub fn build_daemon_delegate_runner(
    db: crate::db::ChatDb,
    may_dir: PathBuf,
    tool_distribution: Arc<ToolDistribution>,
    models: DaemonBotModelFactory,
) -> Arc<dyn crate::bot::DelegateRunner> {
    let db_for_model = db.clone();
    Arc::new(BashDelegateRunner::new(
        db,
        may_dir,
        tool_distribution,
        move |bot_id: &str| {
            let db = db_for_model.clone();
            let models = models.clone();
            let bot_id = bot_id.to_string();
            Box::pin(async move { models.resolve_for_bot_id(&db, &bot_id).await })
        },
    ))
}

// ── REST API Handlers ───────────────────────────────────────────

fn map_bot_error(err: BotServiceError) -> SkyError {
    match err {
        BotServiceError::NotFound(msg)
        | BotServiceError::Approval(crate::bot::BotApprovalError::NotFound(msg))
        | BotServiceError::Delegate(crate::bot::DelegateError::NotFound(msg)) => {
            SkyError::msg(msg).set_status(StatusCode::NOT_FOUND)
        }
        BotServiceError::Invalid(msg)
        | BotServiceError::Approval(crate::bot::BotApprovalError::AlreadyResolved(msg)) => {
            bad_request_error(msg)
        }
        other => internal_error(other),
    }
}

#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct LimitQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/bots`
pub async fn list_bots(State(state): State<Arc<DaemonState>>) -> SkyResult<Json<Vec<BotSummary>>> {
    let bots = state.bot_service.list_bots().await.map_err(map_bot_error)?;
    Ok(Json(bots))
}

/// `POST /api/bots`
pub async fn create_bot(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<CreateBotRequest>,
) -> SkyResult<Json<BotSummary>> {
    let bot = state
        .bot_service
        .create_bot(req)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(bot))
}

/// `GET /api/bots/{id}`
pub async fn get_bot(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<BotSummary>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let bot = state
        .bot_service
        .get_bot(&id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(bot))
}

/// `PATCH /api/bots/{id}`
pub async fn update_bot(
    params: Params,
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<UpdateBotRequest>,
) -> SkyResult<Json<BotSummary>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let bot = state
        .bot_service
        .update_bot(&id, req)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(bot))
}

/// `DELETE /api/bots/{id}`
pub async fn delete_bot(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    state
        .bot_service
        .delete_bot(&id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(API_OK))
}

/// `GET /api/bots/{id}/messages`
pub async fn list_bot_messages(
    params: Params,
    Query(query): Query<LimitQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotChatMessage>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let msgs = state
        .bot_service
        .list_messages(&id, limit)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(msgs))
}

/// `POST /api/bots/{id}/messages`
pub async fn post_bot_message(
    params: Params,
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<SendBotMessageRequest>,
) -> SkyResult<Json<BotChatMessage>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let (chat, _inbox) = state
        .bot_service
        .send_user_message(&id, &req.content)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(chat))
}

/// `GET /api/bots/{id}/trace`
pub async fn list_bot_trace(
    params: Params,
    Query(query): Query<LimitQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotTraceEntry>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let trace = state
        .bot_service
        .list_trace(&id, limit)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(trace))
}

/// `GET /api/bots/{id}/inbox`
pub async fn list_bot_inbox(
    params: Params,
    Query(query): Query<LimitQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotInboxEntry>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let inbox = state
        .bot_service
        .list_inbox(&id, limit)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(inbox))
}

/// `GET /api/bots/{id}/delegates`
pub async fn list_bot_delegates(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotDelegateSummary>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let delegates = state
        .bot_service
        .list_delegates(&id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(delegates))
}

/// `POST /api/bots/{id}/delegates/{agentId}/stop`
pub async fn stop_bot_delegate(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<BotDelegateSummary>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let agent_id = params
        .get("agentId")
        .map_err(bad_request_error)?
        .to_string();
    let summary = state
        .bot_service
        .stop_delegate(&id, &agent_id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(summary))
}

/// `GET /api/bots/{id}/approvals`
pub async fn list_bot_approvals(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotApprovalItem>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let items = state
        .bot_service
        .list_approvals(&id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(items))
}

/// `POST /api/bots/{id}/approvals/{requestId}`
pub async fn resolve_bot_approval(
    params: Params,
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ResolveBotApprovalRequest>,
) -> SkyResult<Json<BotApprovalItem>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let request_id = params
        .get("requestId")
        .map_err(bad_request_error)?
        .to_string();
    let item = state
        .bot_service
        .resolve_approval_by_user(&id, &request_id, req.approved, req.reason)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(item))
}

/// `GET /api/bots/{id}/schedules`
pub async fn list_bot_schedules(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotScheduleItem>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let items = state
        .bot_service
        .list_schedules(&id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(items))
}

/// `DELETE /api/bots/{id}/schedules/{scheduleId}`
pub async fn delete_bot_schedule(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let schedule_id = params
        .get("scheduleId")
        .map_err(bad_request_error)?
        .to_string();
    let _ = state
        .bot_service
        .delete_schedule(&id, &schedule_id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(API_OK))
}

/// `GET /api/bots/{id}/memory`
pub async fn get_bot_memory(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<BotMemoryNode>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let tree = state
        .bot_service
        .memory_tree(&id)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(tree))
}

/// `GET /api/bots/{id}/memory/history`
pub async fn get_bot_memory_history(
    params: Params,
    Query(query): Query<LimitQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<BotMemoryCommit>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let commits = state
        .bot_service
        .memory_history(&id, limit)
        .await
        .map_err(map_bot_error)?;
    Ok(Json(commits))
}
