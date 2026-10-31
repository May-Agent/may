use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::time::Instant as TokioInstant;

use super::local_url_mapper::LocalUrlPortMapper;
use crate::agent::PermissionApprover;
use crate::agent::browser_tool::{BrowserBroker, BrowserQueue};
use crate::agent::register_tool::RegisterBroker;
use crate::agent::register_tool::RegisterQueue;
use crate::agent::ssh_authorizer::PermissionChoice;
use crate::agent::task_tool::{TaskBroker, TaskQueue};
use crate::ask_user::{AnswerValue, AskUserRequest, Question, QuestionAnswer};
use crate::config::MayConfig;
use crate::daemon::types::BrowserLifecycleEvent;
use crate::live_response::LiveResponseRegistry;
use crate::mcp::{MayMcpServerConfig, McpManager, McpServerInfo};
use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};
use crate::workspace_request::RequestWorkspaceArgs;

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContainerPrepareStage {
    Checking,
    Building,
    Ready,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
pub struct ContainerPrepareStatus {
    pub(crate) profile_id: String,
    pub(crate) stage: ContainerPrepareStage,
    pub(crate) summary: String,
    pub(crate) recent_logs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
pub struct ContainerPrepareStatusesPayload {
    pub(crate) statuses: Vec<ContainerPrepareStatus>,
}

#[derive(Debug, Clone, Default)]
struct ContainerPrepareSnapshot {
    statuses: HashMap<String, ContainerPrepareStatus>,
    inflight: HashSet<String>,
}

enum ContainerPrepareCommand {
    TryBegin {
        profile_id: String,
        reply: oneshot::Sender<bool>,
    },
    Finish {
        profile_id: String,
    },
    SetStatus {
        status: ContainerPrepareStatus,
    },
    ClearStatus {
        profile_id: String,
    },
}

#[derive(Clone)]
pub struct ContainerPrepareTracker {
    tx: tokio::sync::mpsc::UnboundedSender<ContainerPrepareCommand>,
    snapshot_rx: tokio::sync::watch::Receiver<ContainerPrepareSnapshot>,
}

impl std::fmt::Debug for ContainerPrepareTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerPrepareTracker")
            .finish_non_exhaustive()
    }
}

impl ContainerPrepareTracker {
    pub(crate) fn new(ui_event_bus: UiEventBus) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (snapshot_tx, snapshot_rx) =
            tokio::sync::watch::channel(ContainerPrepareSnapshot::default());
        tokio::spawn(async move {
            let mut snapshot = ContainerPrepareSnapshot::default();
            while let Some(command) = rx.recv().await {
                match command {
                    ContainerPrepareCommand::TryBegin { profile_id, reply } => {
                        let inserted = snapshot.inflight.insert(profile_id);
                        let _ = reply.send(inserted);
                    }
                    ContainerPrepareCommand::Finish { profile_id } => {
                        snapshot.inflight.remove(profile_id.as_str());
                    }
                    ContainerPrepareCommand::SetStatus { status } => {
                        snapshot.statuses.insert(status.profile_id.clone(), status);
                        snapshot_tx.send_replace(snapshot.clone());
                        ui_event_bus.publish(UiEventTopics::CONTAINER_PREPARE);
                    }
                    ContainerPrepareCommand::ClearStatus { profile_id } => {
                        snapshot.statuses.remove(profile_id.as_str());
                        snapshot_tx.send_replace(snapshot.clone());
                        ui_event_bus.publish(UiEventTopics::CONTAINER_PREPARE);
                    }
                }
            }
        });
        Self { tx, snapshot_rx }
    }

    pub(crate) async fn try_begin(&self, profile_id: &str) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(ContainerPrepareCommand::TryBegin {
                profile_id: profile_id.to_string(),
                reply: reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    pub(crate) fn finish(&self, profile_id: &str) {
        let _ = self.tx.send(ContainerPrepareCommand::Finish {
            profile_id: profile_id.to_string(),
        });
    }

    pub(crate) fn set_status(&self, status: ContainerPrepareStatus) {
        let _ = self.tx.send(ContainerPrepareCommand::SetStatus { status });
    }

    pub(crate) fn get_status(&self, profile_id: &str) -> Option<ContainerPrepareStatus> {
        self.snapshot_rx.borrow().statuses.get(profile_id).cloned()
    }

    pub(crate) fn clear_status(&self, profile_id: &str) {
        let _ = self.tx.send(ContainerPrepareCommand::ClearStatus {
            profile_id: profile_id.to_string(),
        });
    }

    pub(crate) fn snapshot_payload(&self) -> ContainerPrepareStatusesPayload {
        let mut statuses = self
            .snapshot_rx
            .borrow()
            .statuses
            .values()
            .cloned()
            .collect::<Vec<_>>();
        statuses.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        ContainerPrepareStatusesPayload { statuses }
    }
}

#[derive(Debug, Clone)]
pub struct TitleGenerationTask {
    pub(crate) session_id: String,
    pub(crate) user_message: String,
    pub(crate) assistant_response: String,
}

#[derive(Debug, Clone)]
pub struct TitleGenerationQueue {
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
pub struct UiToolDispatchers {
    pub(crate) task: TaskBroker,
    pub(crate) notify: crate::agent::notify_tool::NotifyBroker,
    pub(crate) browser: BrowserBroker,
    pub(crate) register_skills: RegisterBroker,
    pub(crate) register_subagents: RegisterBroker,
}

pub struct UiToolQueues {
    pub(crate) task: TaskQueue,
    pub(crate) notify: crate::agent::notify_tool::NotifyQueue,
    pub(crate) browser: BrowserQueue,
    pub(crate) register_skills: RegisterQueue,
    pub(crate) register_subagents: RegisterQueue,
}

pub fn tool_dispatch_channels() -> (UiToolDispatchers, UiToolQueues) {
    let (task, task_queue) = crate::agent::task_tool::channel();
    let (notify, notify_queue) = crate::agent::notify_tool::channel();
    let (browser, browser_queue) = crate::agent::browser_tool::channel();
    let (register_skills, register_skills_queue) = crate::agent::register_tool::skills_channel();
    let (register_subagents, register_subagents_queue) =
        crate::agent::register_tool::subagents_channel();

    (
        UiToolDispatchers {
            task,
            notify,
            browser,
            register_skills,
            register_subagents,
        },
        UiToolQueues {
            task: task_queue,
            notify: notify_queue,
            browser: browser_queue,
            register_skills: register_skills_queue,
            register_subagents: register_subagents_queue,
        },
    )
}

#[derive(Debug)]
pub struct RefreshableCache<T> {
    ttl: Duration,
    inner: ArcSwapOption<CachedValue<T>>,
    refresh_lock: Mutex<()>,
}

#[derive(Debug)]
struct CachedValue<T> {
    updated_at: TokioInstant,
    value: T,
}

impl<T: Clone + Send + Sync> RefreshableCache<T> {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: ArcSwapOption::empty(),
            refresh_lock: Mutex::new(()),
        }
    }

    pub(crate) async fn get_or_refresh<F, Fut>(&self, refresh: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send,
    {
        if let Some(cached) = self.inner.load_full()
            && cached.updated_at.elapsed() < self.ttl
        {
            return cached.value.clone();
        }

        let _refresh_guard = self.refresh_lock.lock().await;
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
pub struct McpService {
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
        let has_servers = manager.has_servers();
        tokio::spawn(async move {
            // Eagerly start connection but don't block the command loop.
            // The first few commands may see unconnected servers.
            if has_servers {
                let results = manager.connect_all().await;
                for (name, result) in results {
                    if let Err(e) = result {
                        tracing::warn!(server = %name, error = %e, "background MCP server connection failed");
                    }
                }
            }
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
        reply_rx
            .await
            .expect("MCP actor dropped add_server response")
    }

    pub(crate) async fn remove_server(&self, name: String) -> Result<(), aither_mcp::McpError> {
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

pub struct DaemonState {
    pub(crate) tokio_handle: tokio::runtime::Handle,
    pub(crate) db: crate::db::ChatDb,
    pub(crate) default_profile_id: String,
    pub(crate) mcp_service: McpService,
    pub(crate) permission_approver: PermissionApprover,
    pub(crate) workspace_approver: WorkspaceApprover,
    pub(crate) ask_user_approver: AskUserApprover,
    pub(crate) tool_dispatchers: UiToolDispatchers,
    pub(crate) fetched_models_cache: RefreshableCache<Vec<crate::models::FetchedModel>>,
    pub(crate) config: ArcSwap<MayConfig>,
    pub(crate) ui_event_bus: UiEventBus,
    pub(crate) container_prepare: ContainerPrepareTracker,
    pub(crate) container_manager: Option<Arc<crate::container::lifecycle::ContainerManager>>,
    pub(crate) browser_manager: ArcSwapOption<crate::container::browser::BrowserManager>,
    pub(crate) browser_owner_session_id: ArcSwapOption<String>,
    /// Sequential lifecycle facts emitted by the browser actor, fanned out
    /// to every active SSE client subscribed to `browser_lifecycle`.
    pub(crate) browser_lifecycle_tx: broadcast::Sender<BrowserLifecycleEvent>,
    /// Each session's virtual desktop, acquired on demand and released
    /// with its session — computer use.
    #[cfg(target_os = "macos")]
    pub(crate) session_displays: Mutex<crate::computer::SessionDisplays>,
    /// Each session's desktop stream — created with the first start,
    /// released with its session.
    #[cfg(target_os = "macos")]
    pub(crate) session_streams: Mutex<crate::computer::SessionStreams>,
    pub(crate) remote_access_manager: Arc<super::remote_access::RemoteAccessManager>,
    pub(crate) notification_service: Arc<crate::notification::NotificationService>,
    pub(crate) memory_processor: Option<crate::memory::MemoryProcessor>,
    pub(crate) local_url_port_mapper: Arc<LocalUrlPortMapper>,
    pub(crate) title_generation_queue: TitleGenerationQueue,
    pub(crate) location_provider: crate::location::LocationProvider,
    pub(crate) runtime_service: Arc<crate::session::SessionRuntimeService>,
    pub(crate) task_runtime: Arc<crate::scheduler::TaskRuntime>,
    /// Embedded baseline tool distribution shared by agents, tasks and containers.
    pub(crate) tool_distribution: Arc<aither_sandbox::ToolDistribution>,
    /// Prepared tool environments reused by every agent rebuilt for a session.
    pub(crate) tool_environments: crate::agent::ToolEnvironmentCache,
    pub(crate) cwd: PathBuf,
    pub(crate) may_dir: PathBuf,
    /// Wakes the search indexer after new messages land.
    pub(crate) search_index_wake_tx: async_channel::Sender<()>,
    /// The indexer's side of [`Self::search_index_wake_tx`].
    pub(crate) search_index_wake: async_channel::Receiver<()>,
    /// Latest outcome of the search indexer.
    pub(crate) search_index_status: crate::repo_search::indexer::IndexerStatus,
    /// Signalled by `POST /api/daemon/shutdown`.
    pub(crate) daemon_shutdown: Arc<tokio::sync::Notify>,
    /// Build of the running executable; clients with another build replace
    /// the daemon.
    pub(crate) build_id: String,
    /// Live turns fanned out to every client of their session.
    pub(crate) turn_hub: Arc<super::chat::TurnHub>,
    /// Each session's agent between turns.
    pub(crate) parked_agents: super::chat::ParkedAgents,
    /// Each session's background jobs and subagent runs, across turns.
    pub(crate) session_background: super::chat::SessionBackgrounds,
}

impl DaemonState {
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

    pub(crate) fn browser_manager(&self) -> Option<Arc<crate::container::browser::BrowserManager>> {
        self.browser_manager.load_full()
    }

    pub(crate) fn browser_owner_session_id(&self) -> Option<String> {
        self.browser_owner_session_id
            .load_full()
            .map(|session_id| session_id.as_ref().clone())
    }

    pub(crate) fn set_browser_owner_session_id(&self, session_id: Option<String>) {
        self.browser_owner_session_id
            .store(session_id.map(Arc::new));
    }

    pub(crate) fn set_browser_manager(
        &self,
        browser_manager: Option<Arc<crate::container::browser::BrowserManager>>,
    ) {
        self.browser_manager.store(browser_manager);
    }

    pub(crate) fn browser_lifecycle_subscribe(&self) -> broadcast::Receiver<BrowserLifecycleEvent> {
        self.browser_lifecycle_tx.subscribe()
    }
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAnswerInput {
    Selected { values: Vec<String> },
    Custom { text: String },
}

#[derive(Debug, thiserror::Error)]
pub enum UiAnswerInputError {
    #[error("ask-user selected answer requires at least one option")]
    MissingSelection,
    #[error("ask-user selected answers must not contain empty options")]
    BlankSelectedOption,
    #[error("ask-user custom answer must not be empty")]
    BlankCustom,
}

impl TryFrom<UiAnswerInput> for AnswerValue {
    type Error = UiAnswerInputError;

    fn try_from(value: UiAnswerInput) -> Result<Self, Self::Error> {
        match value {
            UiAnswerInput::Selected { values } => {
                if values.is_empty() {
                    return Err(UiAnswerInputError::MissingSelection);
                }
                if values.iter().any(|value| value.trim().is_empty()) {
                    return Err(UiAnswerInputError::BlankSelectedOption);
                }
                Ok(Self::Selected(values))
            }
            UiAnswerInput::Custom { text } => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Err(UiAnswerInputError::BlankCustom);
                }
                Ok(Self::Custom(trimmed.to_string()))
            }
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiQuestionAnswer {
    pub(crate) section: String,
    pub(crate) answer: UiAnswerInput,
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserRespondRequest {
    pub(crate) id: String,
    pub(crate) answers: Vec<UiQuestionAnswer>,
}

/// What resolving a question did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskUserOutcome {
    /// The run waiting on the prompt was woken with the answer.
    Woke,
    /// The answer to an open question joined the session queue; the caller
    /// dispatches it.
    Queued,
    /// The run that blocked on the prompt is gone. The decision is
    /// committed; the session resumes with the interrupted-run reminder, not
    /// with a question of the agent's own.
    Resume {
        /// Whether the user allowed what the prompt asked for.
        approved: bool,
    },
    /// An earlier call committed the answer; nothing more happens.
    AlreadyAnswered,
}

/// A question that an answer resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserResolved {
    pub session_id: String,
    pub outcome: AskUserOutcome,
}

/// Whether `answers` select the allow option of the internal prompt
/// `request`: the test the live gate applies.
fn answers_allow(request: &PersistedAskUserRequest, answers: &[QuestionAnswer]) -> bool {
    request
        .questions
        .first()
        .and_then(|question| PermissionChoice::from_options(&question.option))
        .is_some_and(|choice| choice.allows(answers))
}

#[derive(Debug, Clone)]
pub struct AskUserApprover {
    live_responses: Arc<LiveResponseRegistry<Vec<QuestionAnswer>>>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceApprover {
    live_responses: Arc<LiveResponseRegistry<bool>>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceApprovalError {
    #[error("failed to persist waiting workspace request: {0}")]
    PersistWaiting(#[source] crate::db::ChatDbError),
    #[error("failed to append workspace request history: {0}")]
    PersistHistory(#[source] crate::db::ChatDbError),
    #[error("workspace request interrupted")]
    Interrupted,
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
        let rx = self.live_responses.open(&request_id);

        if let Some(session_id) = request_session_id.as_deref()
            && let Some(runtime_service) = self.runtime_service.clone()
        {
            let persisted_request = PersistedAskUserRequest {
                id: request_id.clone(),
                questions: Self::persisted_questions(&questions),
            };
            if let Err(error) = runtime_service
                .register_ask_user_request(session_id, &persisted_request)
                .await
            {
                tracing::warn!(session_id = %session_id, error = %error, "failed to register ask-user request");
            }
        }

        // A closed registry means the server is going down; dropping the
        // request tells the asking run it was interrupted.
        if let Ok(answers) = rx.recv().await {
            let _ = request.respond(answers);
        }
    }

    /// Resolve the pending question `request_id` with `answers`: the answer
    /// is committed first, then the waiting run is woken. The waiter is
    /// taken out of the registry before the commit, so nothing else resolves
    /// it meanwhile; it goes back if nothing is committed. A run that is gone
    /// by the wake leaves the committed decision to the caller to resume.
    /// Returns `None` when the question is not pending and was never
    /// answered.
    ///
    /// # Errors
    /// Returns [`crate::session::AnswerError`] when the answers do not fit
    /// the question or the commit fails; nothing is woken or queued then.
    ///
    /// # Panics
    /// Panics if the approver has no runtime service.
    pub(crate) async fn resolve(
        &self,
        request_id: &str,
        answers: Vec<QuestionAnswer>,
    ) -> Result<Option<AskUserResolved>, crate::session::AnswerError> {
        let runtime = self
            .runtime_service
            .as_ref()
            .expect("resolving an ask-user question needs the runtime service");
        let waiter = self.live_responses.take(request_id);
        let restore = |waiter: &Option<async_channel::Sender<Vec<QuestionAnswer>>>| {
            if let Some(waiter) = waiter {
                self.live_responses.restore(request_id, waiter);
            }
        };
        let answered = match runtime.answer_ask_user(request_id, &answers).await {
            Ok(answered) => answered,
            Err(error) => {
                restore(&waiter);
                return Err(error);
            }
        };
        let committed = match answered {
            None => {
                restore(&waiter);
                return Ok(None);
            }
            Some(crate::session::AskUserAnswer::AlreadyAnswered { session_id }) => {
                restore(&waiter);
                return Ok(Some(AskUserResolved {
                    session_id,
                    outcome: AskUserOutcome::AlreadyAnswered,
                }));
            }
            Some(crate::session::AskUserAnswer::Committed(committed)) => committed,
        };
        let outcome = if committed.delivered {
            AskUserOutcome::Queued
        } else if waiter.is_some_and(|waiter| waiter.try_send(answers.clone()).is_ok()) {
            AskUserOutcome::Woke
        } else {
            AskUserOutcome::Resume {
                approved: answers_allow(&committed.request, &answers),
            }
        };
        Ok(Some(AskUserResolved {
            session_id: committed.session_id,
            outcome,
        }))
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
    ) -> Result<bool, WorkspaceApprovalError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let rx = self.live_responses.open(&request_id);

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
                .map_err(WorkspaceApprovalError::PersistWaiting)?;

            runtime_service
                .db()
                .append_workspace_request_message(
                    session_id,
                    request_id.as_str(),
                    request.path.as_str(),
                    request.reason.as_str(),
                    None,
                )
                .await
                .map_err(WorkspaceApprovalError::PersistHistory)?;
        }

        rx.recv()
            .await
            .map_err(|_| WorkspaceApprovalError::Interrupted)
    }

    /// Resolve the pending request `request_id`: the decision is committed
    /// first (the request cleared by its id, the decision in the history),
    /// and only then is the waiting run woken. Returns `None` when the
    /// request is not pending.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] when the commit fails; nothing is
    /// woken then.
    ///
    /// # Panics
    /// Panics if the approver has no runtime service.
    pub(crate) async fn resolve(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<crate::session::ResolvedRequest>, crate::db::ChatDbError> {
        let runtime = self
            .runtime_service
            .as_ref()
            .expect("resolving a workspace request needs the runtime service");
        let Some(session_id) = runtime
            .commit_workspace_response(request_id, approved)
            .await?
        else {
            return Ok(None);
        };
        let woke = self.live_responses.resolve(request_id, approved);
        Ok(Some(crate::session::ResolvedRequest { session_id, woke }))
    }
}

impl DaemonState {
    pub(crate) fn notify_ui_events(&self, topics: UiEventTopics) {
        self.ui_event_bus.publish(topics);
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
                self.tool_distribution.clone(),
            ),
        ))
    }

    pub(crate) async fn try_begin_container_prepare(&self, profile_id: &str) -> bool {
        self.container_prepare.try_begin(profile_id).await
    }

    pub(crate) fn finish_container_prepare(&self, profile_id: &str) {
        self.container_prepare.finish(profile_id);
    }

    pub(crate) fn set_container_prepare_status(&self, status: ContainerPrepareStatus) {
        self.container_prepare.set_status(status);
    }

    pub(crate) fn get_container_prepare_status(
        &self,
        profile_id: &str,
    ) -> Option<ContainerPrepareStatus> {
        self.container_prepare.get_status(profile_id)
    }

    pub(crate) fn clear_container_prepare_status(&self, profile_id: &str) {
        self.container_prepare.clear_status(profile_id);
    }

    pub(crate) fn snapshot_container_prepare_statuses(&self) -> ContainerPrepareStatusesPayload {
        self.container_prepare.snapshot_payload()
    }
}
