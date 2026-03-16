use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use dashmap::{DashMap, DashSet};
use tokio::sync::{oneshot};
use tokio::time::Instant as TokioInstant;

use super::local_url_mapper::LocalUrlPortMapper;
use super::types::{ModelsResponse, ProviderModelsResponse};
use crate::agent::PermissionApprover;
use crate::agent::register_tool::RegisterBroker;
use crate::agent::register_tool::RegisterQueue;
use crate::agent::task_tool::{TaskBroker, TaskQueue};
use crate::ask_user::{AnswerValue, AskUserRequest, Question, QuestionAnswer};
use crate::config::MayConfig;
use crate::live_response::LiveResponseRegistry;
use crate::mcp::{MayMcpServerConfig, McpManager, McpServerInfo};
use crate::session::{MessageRole, PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::workspace_request::RequestWorkspaceArgs;

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContainerPrepareStage {
    Checking,
    Building,
    Ready,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ContainerPrepareStatus {
    pub(crate) profile_id: String,
    pub(crate) stage: ContainerPrepareStage,
    pub(crate) summary: String,
    pub(crate) recent_logs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ContainerPrepareStatusesPayload {
    pub(crate) statuses: Vec<ContainerPrepareStatus>,
}

#[derive(Debug, Clone)]
pub(crate) struct TitleGenerationTask {
    pub(crate) session_id: String,
    pub(crate) user_message: String,
    pub(crate) assistant_response: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TitleGenerationQueue {
    tx: tokio::sync::mpsc::UnboundedSender<TitleGenerationTask>,
}

impl TitleGenerationQueue {
    pub(crate) fn new() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<TitleGenerationTask>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    pub(crate) fn enqueue(&self, task: TitleGenerationTask) {
        let _ = self.tx.send(task);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UiToolDispatchers {
    pub(crate) task: TaskBroker,
    pub(crate) notify: crate::agent::notify_tool::NotifyBroker,
    pub(crate) register_skills: RegisterBroker,
    pub(crate) register_subagents: RegisterBroker,
}

pub(crate) struct UiToolQueues {
    pub(crate) task: TaskQueue,
    pub(crate) notify: crate::agent::notify_tool::NotifyQueue,
    pub(crate) register_skills: RegisterQueue,
    pub(crate) register_subagents: RegisterQueue,
}

pub(crate) fn tool_dispatch_channels() -> (UiToolDispatchers, UiToolQueues) {
    let (task, task_queue) = crate::agent::task_tool::channel();
    let (notify, notify_queue) = crate::agent::notify_tool::channel();
    let (register_skills, register_skills_queue) = crate::agent::register_tool::skills_channel();
    let (register_subagents, register_subagents_queue) =
        crate::agent::register_tool::subagents_channel();

    (
        UiToolDispatchers {
            task,
            notify,
            register_skills,
            register_subagents,
        },
        UiToolQueues {
            task: task_queue,
            notify: notify_queue,
            register_skills: register_skills_queue,
            register_subagents: register_subagents_queue,
        },
    )
}

#[derive(Debug)]
pub(crate) struct RefreshableCache<T> {
    ttl: Duration,
    inner: ArcSwapOption<CachedValue<T>>,
}

#[derive(Debug)]
struct CachedValue<T> {
    updated_at: TokioInstant,
    value: T,
}

impl<T: Clone> RefreshableCache<T> {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: ArcSwapOption::empty(),
        }
    }

    pub(crate) async fn get_or_refresh<F, Fut>(&self, refresh: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        if let Some(cached) = self.inner.load_full()
            && cached.updated_at.elapsed() < self.ttl
        {
            return cached.value.clone();
        }

        let value = refresh().await;
        self.inner.store(Some(Arc::new(CachedValue {
            updated_at: TokioInstant::now(),
            value: value.clone(),
        })));
        value
    }
}

enum McpCommand {
    ServerInfo {
        reply: oneshot::Sender<Vec<McpServerInfo>>,
    },
    AddServer {
        name: String,
        config: MayMcpServerConfig,
        reply: oneshot::Sender<Result<(), aither_mcp::McpError>>,
    },
    RemoveServer {
        name: String,
        reply: oneshot::Sender<Result<(), aither_mcp::McpError>>,
    },
    Reconnect {
        name: String,
        reply: oneshot::Sender<Result<(), aither_mcp::McpError>>,
    },
}

#[derive(Clone)]
pub(crate) struct McpService {
    tx: tokio::sync::mpsc::UnboundedSender<McpCommand>,
}

impl std::fmt::Debug for McpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpService").finish_non_exhaustive()
    }
}

impl McpService {
    pub(crate) fn spawn(mut manager: McpManager) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    McpCommand::ServerInfo { reply } => {
                        let _ = reply.send(manager.server_info());
                    }
                    McpCommand::AddServer {
                        name,
                        config,
                        reply,
                    } => {
                        let _ = reply.send(manager.add_server(name, config).await);
                    }
                    McpCommand::RemoveServer { name, reply } => {
                        let _ = reply.send(manager.remove_server(&name).await);
                    }
                    McpCommand::Reconnect { name, reply } => {
                        let _ = reply.send(manager.reconnect(&name).await);
                    }
                }
            }
        });
        Self { tx }
    }

    pub(crate) async fn server_info(&self) -> Vec<McpServerInfo> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(McpCommand::ServerInfo { reply: reply_tx })
            .expect("MCP actor channel unexpectedly closed");
        reply_rx
            .await
            .expect("MCP actor dropped server_info response")
    }

    pub(crate) async fn add_server(
        &self,
        name: String,
        config: MayMcpServerConfig,
    ) -> Result<(), aither_mcp::McpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(McpCommand::AddServer {
                name,
                config,
                reply: reply_tx,
            })
            .expect("MCP actor channel unexpectedly closed");
        reply_rx.await.expect("MCP actor dropped add_server response")
    }

    pub(crate) async fn remove_server(
        &self,
        name: String,
    ) -> Result<(), aither_mcp::McpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(McpCommand::RemoveServer {
                name,
                reply: reply_tx,
            })
            .expect("MCP actor channel unexpectedly closed");
        reply_rx
            .await
            .expect("MCP actor dropped remove_server response")
    }

    pub(crate) async fn reconnect(&self, name: String) -> Result<(), aither_mcp::McpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(McpCommand::Reconnect {
                name,
                reply: reply_tx,
            })
            .expect("MCP actor channel unexpectedly closed");
        reply_rx
            .await
            .expect("MCP actor dropped reconnect response")
    }
}

pub(crate) struct UiServerState {
    pub(crate) db: crate::db::ChatDb,
    pub(crate) default_profile_id: String,
    pub(crate) mcp_service: McpService,
    pub(crate) permission_approver: PermissionApprover,
    pub(crate) workspace_approver: WorkspaceApprover,
    pub(crate) ask_user_approver: AskUserApprover,
    pub(crate) tool_dispatchers: UiToolDispatchers,
    pub(crate) run_cancellation_registry: SessionRunCancellationRegistry,
    pub(crate) models_cache: RefreshableCache<ModelsResponse>,
    pub(crate) provider_models_cache: RefreshableCache<ProviderModelsResponse>,
    pub(crate) config: ArcSwap<MayConfig>,
    pub(crate) ui_events_notify: Arc<tokio::sync::Notify>,
    pub(crate) container_prepare_statuses: DashMap<String, ContainerPrepareStatus>,
    pub(crate) container_prepare_inflight: DashSet<String>,
    pub(crate) container_manager: Option<Arc<crate::container::lifecycle::ContainerManager>>,
    pub(crate) browser_manager: Option<Arc<crate::container::playwright::BrowserManager>>,
    pub(crate) remote_access_manager: Arc<super::remote_access::RemoteAccessManager>,
    pub(crate) notification_service: Arc<crate::notification_service::NotificationService>,
    pub(crate) memory_processor: Option<crate::memory::MemoryProcessor>,
    pub(crate) local_url_port_mapper: Arc<LocalUrlPortMapper>,
    pub(crate) title_generation_queue: TitleGenerationQueue,
    pub(crate) location_provider: crate::location::LocationProvider,
    pub(crate) runtime_service: Arc<crate::session::SessionRuntimeService>,
    pub(crate) task_runtime: Arc<crate::scheduler::TaskRuntime>,
    pub(crate) cwd: PathBuf,
    pub(crate) may_dir: PathBuf,
}

impl UiServerState {
    pub(crate) fn mcp(&self) -> McpService {
        self.mcp_service.clone()
    }

    pub(crate) fn db(&self) -> crate::db::ChatDb {
        self.db.clone()
    }

    pub(crate) fn config_snapshot(&self) -> Arc<MayConfig> {
        self.config.load_full()
    }

    pub(crate) fn update_config<R>(&self, update: impl FnOnce(&mut MayConfig) -> R) -> MayConfig {
        let mut config = (*self.config.load_full()).clone();
        let _ = update(&mut config);
        self.config.store(Arc::new(config.clone()));
        config
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiQuestionAnswer {
    pub(crate) section: String,
    pub(crate) answer: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AskUserRespondRequest {
    pub(crate) id: String,
    pub(crate) answers: Vec<UiQuestionAnswer>,
}

#[derive(Debug, Clone)]
pub(crate) struct AskUserApprover {
    live_responses: Arc<LiveResponseRegistry<Vec<QuestionAnswer>>>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceApprover {
    live_responses: Arc<LiveResponseRegistry<bool>>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionRunCancellationRegistry {
    live_responses: Arc<LiveResponseRegistry<()>>,
}

impl SessionRunCancellationRegistry {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
        }
    }

    pub(crate) fn try_open(
        &self,
        session_id: impl Into<String>,
    ) -> Option<async_channel::Receiver<()>> {
        self.live_responses.try_open(session_id.into())
    }

    pub(crate) fn cancel(&self, session_id: &str) -> bool {
        self.live_responses.resolve(session_id, ())
    }

    pub(crate) fn close(&self, session_id: &str) -> bool {
        self.live_responses.discard(session_id)
    }
}

/// Generate skipped answers for all questions in the request.
fn skipped_answers(questions: &[Question]) -> Vec<QuestionAnswer> {
    questions
        .iter()
        .map(|q| QuestionAnswer {
            section: q.section.clone(),
            answer: AnswerValue::Skipped,
        })
        .collect()
}

impl AskUserApprover {
    pub(crate) fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_service: None,
        }
    }

    #[must_use]
    pub(crate) fn with_runtime_service(
        mut self,
        runtime_service: Arc<crate::session::SessionRuntimeService>,
    ) -> Self {
        self.runtime_service = Some(runtime_service);
        self
    }

    fn persisted_questions(questions: &[Question]) -> Vec<PersistedAskUserQuestion> {
        questions
            .iter()
            .map(|question| PersistedAskUserQuestion {
                section: question.section.clone(),
                question: question.question.clone(),
                option: question.option.clone(),
                multi_select: question.multi_select,
            })
            .collect()
    }

    pub(crate) async fn enqueue(&self, request: AskUserRequest, session_id: Option<String>) {
        let questions = request.args.request.clone().into_questions();
        let request_session_id = Some(request.args.session_id.clone()).or(session_id);
        let request_id = uuid::Uuid::new_v4().to_string();
        let rx = self.live_responses.open(request_id.clone());

        if let Some(session_id) = request_session_id.as_deref()
            && let Some(runtime_service) = self.runtime_service.clone()
        {
            let persisted_request = PersistedAskUserRequest {
                id: request_id.clone(),
                questions: Self::persisted_questions(&questions),
            };
            if let Err(error) = runtime_service
                .set_waiting_ask_user(session_id, Some(persisted_request))
                .await
            {
                tracing::warn!(session_id = %session_id, error = %error, "failed to persist ask-user runtime state");
            }

            let content = serde_json::to_string(&crate::session::history::AskUserRequestData {
                id: request_id.clone(),
                questions: serde_json::to_value(&questions)
                    .expect("ask_user questions must serialize"),
            })
            .expect("ask_user request content must serialize");
            if let Err(error) = runtime_service
                .db()
                .append_message(
                    session_id,
                    MessageRole::AskUserRequest,
                    &content,
                    None,
                    None,
                    None,
                )
                .await
            {
                tracing::warn!(session_id, error = %error, "failed to append ask-user request history");
            }
        }

        let answers = rx
            .recv()
            .await
            .unwrap_or_else(|_| skipped_answers(&questions));
        let _ = request.respond(answers);
    }

    pub(crate) fn respond(&self, request_id: &str, answers: Vec<QuestionAnswer>) -> bool {
        self.live_responses.resolve(request_id, answers)
    }
}

impl WorkspaceApprover {
    pub(crate) fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_service: None,
        }
    }

    #[must_use]
    pub(crate) fn with_runtime_service(
        mut self,
        runtime_service: Arc<crate::session::SessionRuntimeService>,
    ) -> Self {
        self.runtime_service = Some(runtime_service);
        self
    }

    fn persisted_request(
        request_id: String,
        session_id: &str,
        request: &RequestWorkspaceArgs,
    ) -> crate::session::PersistedWorkspaceRequest {
        crate::session::PersistedWorkspaceRequest {
            id: request_id,
            request: crate::session::PersistedWorkspaceRequestPayload {
                path: request.path.clone(),
                reason: request.reason.clone(),
                session_id: Some(session_id.to_string()),
            },
        }
    }

    pub(crate) async fn request_and_wait(
        &self,
        session_id: &str,
        request: RequestWorkspaceArgs,
    ) -> anyhow::Result<bool> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let rx = self.live_responses.open(request_id.clone());

        if let Some(runtime_service) = self.runtime_service.clone() {
            runtime_service
                .set_waiting_workspace(
                    session_id,
                    Some(Self::persisted_request(
                        request_id.clone(),
                        session_id,
                        &request,
                    )),
                )
                .await
                .map_err(anyhow::Error::from)?;

            let content = serde_json::to_string(&crate::session::history::WorkspaceRequestData {
                id: request_id.clone(),
                path: request.path.clone(),
                reason: request.reason.clone(),
            })
            .expect("workspace request content must serialize");
            runtime_service
                .db()
                .append_message(
                    session_id,
                    MessageRole::WorkspaceRequest,
                    &content,
                    None,
                    None,
                    None,
                )
                .await
                .map_err(anyhow::Error::from)?;
        }

        rx.recv()
            .await
            .map_err(|_| anyhow::anyhow!("workspace request interrupted"))
    }

    pub(crate) fn respond(&self, request_id: &str, approved: bool) -> bool {
        self.live_responses.resolve(request_id, approved)
    }
}

impl UiServerState {
    pub(crate) fn notify_ui_events(&self) {
        self.ui_events_notify.notify_waiters();
    }

    pub(crate) async fn resolve_container_manager(
        &self,
        allow_managed_podman_download: bool,
    ) -> Result<Arc<crate::container::lifecycle::ContainerManager>, crate::container::ContainerError>
    {
        if let Some(manager) = self.container_manager.as_ref() {
            return Ok(manager.clone());
        }

        let runtime = crate::container::ContainerRuntime::detect_with_preference_and_options(
            aither_sandbox::RuntimePreference::default(),
            crate::container::RuntimeDetectOptions {
                allow_managed_podman_download,
            },
        )
        .await?;
        Ok(Arc::new(
            crate::container::lifecycle::ContainerManager::new(
                Arc::new(runtime),
                self.may_dir.clone(),
            ),
        ))
    }

    pub(crate) async fn try_begin_container_prepare(&self, profile_id: &str) -> bool {
        self.container_prepare_inflight
            .insert(profile_id.to_string())
    }

    pub(crate) fn finish_container_prepare(&self, profile_id: &str) {
        self.container_prepare_inflight.remove(profile_id);
    }

    pub(crate) fn set_container_prepare_status(&self, status: ContainerPrepareStatus) {
        self.container_prepare_statuses
            .insert(status.profile_id.clone(), status);
        self.notify_ui_events();
    }

    pub(crate) fn get_container_prepare_status(
        &self,
        profile_id: &str,
    ) -> Option<ContainerPrepareStatus> {
        self.container_prepare_statuses
            .get(profile_id)
            .map(|entry| entry.clone())
    }

    pub(crate) fn clear_container_prepare_status(&self, profile_id: &str) {
        self.container_prepare_statuses.remove(profile_id);
        self.notify_ui_events();
    }

    pub(crate) fn snapshot_container_prepare_statuses(&self) -> ContainerPrepareStatusesPayload {
        let mut statuses = self
            .container_prepare_statuses
            .iter()
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        statuses.sort_by(|a, b| a.profile_id.cmp(&b.profile_id));
        ContainerPrepareStatusesPayload { statuses }
    }
}
