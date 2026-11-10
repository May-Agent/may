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
use crate::secrets::{SecretScope, SecretStore};
use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest, PersistedSecretRequest};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};
use crate::workspace_request::RequestWorkspaceArgs;
use askama::Template;

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContainerPrepareStage {
    Checking,
    Pulling,
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
    pub(crate) browser: BrowserBroker,
    pub(crate) register_skills: RegisterBroker,
    pub(crate) tunnel: crate::agent::tunnel_tool::TunnelBroker,
}

pub struct UiToolQueues {
    pub(crate) task: TaskQueue,
    pub(crate) browser: BrowserQueue,
    pub(crate) register_skills: RegisterQueue,
    pub(crate) tunnel: crate::agent::tunnel_tool::TunnelQueue,
}

pub fn tool_dispatch_channels() -> (UiToolDispatchers, UiToolQueues) {
    let (task, task_queue) = crate::agent::task_tool::channel();
    let (browser, browser_queue) = crate::agent::browser_tool::channel();
    let (register_skills, register_skills_queue) = crate::agent::register_tool::skills_channel();
    let (tunnel, tunnel_queue) = crate::agent::tunnel_tool::channel();

    (
        UiToolDispatchers {
            task,
            browser,
            register_skills,
            tunnel,
        },
        UiToolQueues {
            task: task_queue,
            browser: browser_queue,
            register_skills: register_skills_queue,
            tunnel: tunnel_queue,
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

    /// The stored value of any age, without refreshing.
    pub(crate) fn latest(&self) -> Option<T> {
        self.inner.load_full().map(|cached| cached.value.clone())
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
            let executor = executor_core::DefaultExecutor;
            // Eagerly start connection but don't block the command loop.
            // The first few commands may see unconnected servers.
            if has_servers {
                let results = manager.connect_all(&executor).await;
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
                        let _ = reply.send(manager.add_server(&executor, name, config).await);
                    }
                    McpCommand::RemoveServer { name, reply } => {
                        let _ = reply.send(manager.remove_server(&name).await);
                    }
                    McpCommand::Reconnect { name, reply } => {
                        let _ = reply.send(manager.reconnect(&executor, &name).await);
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
    /// Runs the connected ACP agents, one host per session; reads the live
    /// config on every spawn.
    pub(crate) acp_host: Arc<crate::agent::acp_subagent::AcpHosts>,
    pub(crate) tokio_handle: tokio::runtime::Handle,
    pub(crate) db: crate::db::ChatDb,
    /// The host's GitHub REST client, for the repository picker.
    pub(crate) github_api: crate::issue::github::GitHubRest<crate::pr::PrTokenSource>,
    pub(crate) default_profile_id: String,
    pub(crate) mcp_service: McpService,
    pub(crate) permission_approver: PermissionApprover,
    pub(crate) workspace_approver: WorkspaceApprover,
    pub(crate) ask_user_approver: AskUserApprover,
    pub(crate) tool_dispatchers: UiToolDispatchers,
    pub(crate) fetched_models_cache: RefreshableCache<Vec<crate::models::FetchedModel>>,
    pub(crate) config: Arc<ArcSwap<MayConfig>>,
    /// The limits every agent is built with, moved by each config change.
    pub(crate) live_limits: crate::agent::LiveLimits,
    pub(crate) ui_event_bus: UiEventBus,
    pub(crate) container_prepare: ContainerPrepareTracker,
    pub(crate) container_manager: Option<Arc<crate::container::lifecycle::ContainerManager>>,
    pub(crate) browser_manager: ArcSwapOption<crate::container::browser::BrowserManager>,
    pub(crate) browser_owner_session_id: ArcSwapOption<String>,
    /// Sequential lifecycle facts emitted by the browser actor, fanned out
    /// to every active SSE client subscribed to `browser_lifecycle`.
    pub(crate) browser_lifecycle_tx: broadcast::Sender<BrowserLifecycleEvent>,
    /// The `computer-agent` helper client — every session's desktop
    /// (display, stream, input) lives in that respawnable process.
    pub(crate) computer: Arc<crate::computer::ComputerAgent>,
    pub(crate) remote_access_manager: Arc<super::remote_access::RemoteAccessManager>,
    /// Whether the user is asleep, for every session and client.
    pub(crate) sleep: Arc<crate::sleep::SleepState>,
    pub(crate) notification_service: Arc<crate::notification::NotificationService>,
    pub(crate) memory_processor: Option<crate::memory::MemoryProcessor>,
    pub(crate) local_url_port_mapper: Arc<LocalUrlPortMapper>,
    pub(crate) title_generation_queue: TitleGenerationQueue,
    pub(crate) location_provider: crate::location::LocationProvider,
    pub(crate) runtime_service: Arc<crate::session::SessionRuntimeService>,
    pub(crate) task_runtime: Arc<crate::scheduler::TaskRuntime>,
    /// Embedded baseline tool distribution shared by agents, tasks and containers.
    pub(crate) tool_distribution: Arc<crate::tools::ToolDistribution>,
    /// Prepared tool environments reused by every agent rebuilt for a session.
    pub(crate) tool_environments: crate::agent::ToolEnvironmentCache,
    /// The machine-wide jobserver pool every terminal draws its jobs from.
    pub(crate) job_server: crate::jobserver::JobServer,
    pub(crate) may_dir: PathBuf,
    /// Wakes the search indexer after new messages land.
    pub(crate) search_index_wake_tx: async_channel::Sender<()>,
    /// The indexer's side of [`Self::search_index_wake_tx`].
    pub(crate) search_index_wake: async_channel::Receiver<()>,
    /// Latest outcome of the search indexer.
    pub(crate) search_index_status: crate::repo_search::indexer::IndexerStatus,
    /// The repo-search daemon handle worktree coverage is polled through.
    pub(crate) repo_search: crate::repo_search::RepoSearch,
    /// Signalled by `POST /api/daemon/shutdown`.
    pub(crate) daemon_shutdown: Arc<tokio::sync::Notify>,
    /// Set once shutdown starts draining: new turns are refused so the
    /// run count only ever falls.
    pub(crate) draining: std::sync::atomic::AtomicBool,
    /// Build of the running executable; clients with another build replace
    /// the daemon.
    pub(crate) build_id: String,
    /// Live turns fanned out to every client of their session.
    pub(crate) turn_hub: Arc<super::chat::TurnHub>,
    /// Each session's agent between turns.
    pub(crate) parked_agents: super::chat::ParkedAgents,
    /// Each session agent's fork source and model, working or idle, for
    /// `btw` questions from other agents.
    pub(crate) agent_forks: crate::agent_link::ForkHosts,
    /// Each session's in-flight aux task (suggestion, recap), if any.
    pub(crate) aux_tasks: super::chat::AuxTasks,
    /// Each session's armed idle-recap timer, if any.
    pub(crate) recap_schedules: super::chat::RecapSchedules,
    /// Each session's background jobs and subagent runs, across turns.
    pub(crate) session_background: super::chat::SessionBackgrounds,
    /// The live checks of every in-flight GitHub PR, as the poller last read
    /// them.
    pub(crate) pr_checks: super::pr_checks::PrChecksCache,
    pub(crate) tunnels: Arc<super::tunnel::TunnelRegistry>,
    /// The machine's reachability and sleep/wake state: the one source every
    /// agent and terminal the daemon builds watches, and the turn runner
    /// waits on after a network failure.
    pub(crate) connectivity: aither_connectivity::Connectivity,
    /// Epoch of the session-event cursors, fixed at daemon start (ns): a
    /// cursor another daemon minted fails its epoch check and resyncs.
    pub(crate) session_events_epoch: u64,
    /// Producers' side of the session-change channel; the dispatcher on the
    /// receiver recomputes and diffs each piece.
    pub(crate) session_change_tx: super::session_events::SessionChangeSender,
    pub(crate) session_change_rx: super::session_events::SessionChangeReceiver,
    /// The dispatcher spawns once, when the daemon starts or the first
    /// session stream attaches.
    pub(crate) session_publisher_once: std::sync::Once,
}

impl DaemonState {
    pub(crate) fn mcp(&self) -> McpService {
        self.mcp_service.clone()
    }

    pub(crate) fn db(&self) -> crate::db::ChatDb {
        self.db.clone()
    }

    /// The context window of `selector`: what its provider reports in the
    /// fetched model list, else the `aither_models` table. The list is read
    /// from the cache at any age; the network is touched only when the cache
    /// holds no list and the table cannot answer.
    pub(crate) async fn model_context_window(
        &self,
        selector: &crate::ModelSelector,
    ) -> Option<u32> {
        let resolve = |fetched: &[crate::models::FetchedModel]| {
            crate::models::resolve_context_window(
                fetched,
                selector.provider_id(),
                selector.model_id(),
            )
        };
        let cached = self.fetched_models_cache.latest().unwrap_or_default();
        if !cached.is_empty() {
            return resolve(&cached);
        }
        if let Some(window) = resolve(&[]) {
            return Some(window);
        }
        let config = (*self.config_snapshot()).clone();
        let fetched = self
            .fetched_models_cache
            .get_or_refresh(move || async move { crate::models::fetch_all_models(&config).await })
            .await;
        resolve(&fetched)
    }

    pub(crate) fn config_snapshot(&self) -> Arc<MayConfig> {
        self.config.load_full()
    }

    /// The tool distribution's `heel`, prepared in the home's tools
    /// directory, which runs the sandboxes of ACP probes and trials.
    pub(crate) async fn heel(&self) -> Result<heel::HeelBinary, crate::tools::ToolRuntimeError> {
        crate::tools::prepare_heel(
            &self.tool_distribution,
            &crate::session::MayHomePaths::new(self.may_dir.clone()).tools_dir(),
        )
        .await
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

    /// Queues `kind` for the session's diff publisher, which recomputes
    /// the piece and emits `*.upserted` / `*.removed` / `*.changed` frames
    /// on `/api/sessions/{id}/events` for the real differences.
    pub(crate) fn publish_session_change(
        &self,
        session_id: &str,
        kind: super::session_events::SessionChange,
    ) {
        super::session_events::notify_change(&self.session_change_tx, session_id, kind);
    }

    /// Starts the session-change dispatcher when it has not started yet.
    pub(crate) fn ensure_session_publisher(self: &Arc<Self>) {
        self.session_publisher_once.call_once(|| {
            super::session_events::spawn(self.clone(), self.session_change_rx.clone());
        });
    }

    /// The cursor `GET /api/sessions/{id}` reports: epoch plus the index a
    /// subscriber naming it as `after` continues from.
    pub(crate) fn session_cursor(&self, session_id: &str) -> String {
        super::session_events::format_cursor(
            self.session_events_epoch,
            self.turn_hub.cursor_index(session_id),
        )
    }
}

/// What a client sends for one question. For a secret question `custom`
/// carries the secret value and `declined` refuses to give it; neither value
/// nor request body is ever printed.
#[derive(serde::Deserialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAnswerInput {
    Selected { values: Vec<String> },
    Custom { text: String },
    Declined,
}

impl std::fmt::Debug for UiAnswerInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Selected { .. } => "UiAnswerInput::Selected",
            Self::Custom { .. } => "UiAnswerInput::Custom",
            Self::Declined => "UiAnswerInput::Declined",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UiAnswerInputError {
    #[error("ask-user selected answer requires at least one option")]
    MissingSelection,
    #[error("ask-user selected answers must not contain empty options")]
    BlankSelectedOption,
    #[error("ask-user custom answer must not be empty")]
    BlankCustom,
    #[error("only a secret question can be declined")]
    DeclinedPlainQuestion,
    #[error("a secret question takes the value as text, not a selection")]
    SelectedSecret,
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
            UiAnswerInput::Declined => Err(UiAnswerInputError::DeclinedPlainQuestion),
        }
    }
}

#[derive(Template)]
#[template(path = "ask_user_secret_stored.txt", escape = "none")]
struct SecretStoredText<'a> {
    name: &'a str,
    scope: SecretScope,
}

#[derive(Template)]
#[template(path = "ask_user_secret_declined.txt", escape = "none")]
struct SecretDeclinedText<'a> {
    name: &'a str,
}

/// Why the answers to a question could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum AskUserRespondError {
    /// The client's answer does not fit its question.
    #[error(transparent)]
    Input(#[from] UiAnswerInputError),
    /// The answers do not fit the question, or the commit failed.
    #[error(transparent)]
    Answer(#[from] crate::session::AnswerError),
    /// The question could not be read.
    #[error("failed to read the question: {0}")]
    Lookup(#[source] crate::db::ChatDbError),
    /// The secret could not be stored; the question stays open.
    #[error("failed to store secret {name}: {source}")]
    Store {
        /// The secret's name.
        name: String,
        /// Why the keychain or index refused it.
        #[source]
        source: crate::secrets::SecretError,
    },
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiQuestionAnswer {
    pub(crate) section: String,
    pub(crate) answer: UiAnswerInput,
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
    /// Where the value a secret question collects is stored.
    secrets: Option<SecretStore>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceApprover {
    live_responses: Arc<LiveResponseRegistry<bool>>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    may_dir: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceApprovalError {
    #[error("failed to persist waiting workspace request: {0}")]
    PersistWaiting(#[source] crate::db::ChatDbError),
    #[error("failed to append workspace request history: {0}")]
    PersistHistory(#[source] crate::db::ChatDbError),
    #[error("workspace request interrupted")]
    Interrupted,
    #[error("failed to read the session's yolo switch: {0}")]
    Yolo(#[source] crate::db::ChatDbError),
    #[error("failed to grant the workspace request under yolo: {0}")]
    YoloGrant(#[source] Box<WorkspaceResolveError>),
}

/// Why a workspace response could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceResolveError {
    /// Storing the decision or reading the session/request failed.
    #[error("failed to commit workspace response: {0}")]
    Commit(#[from] crate::db::ChatDbError),
    /// The committed request's session is gone from the store.
    #[error("session {0} not found")]
    SessionMissing(String),
    /// The approved path does not resolve to a canonical directory.
    #[error("workspace path {path} cannot be resolved: {source}")]
    Canonicalize {
        /// The path the request asked for.
        path: String,
        /// Why canonicalization failed.
        #[source]
        source: std::io::Error,
    },
    /// The grant reached the running terminal but `workspace.toml` rejected
    /// it — a rebuilt session will not carry it.
    #[error("workspace grant could not be persisted: {0}")]
    PersistGrant(#[source] crate::workspace::WorkspaceConfigError),
}

impl AskUserApprover {
    pub(crate) fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_service: None,
            secrets: None,
        }
    }

    #[must_use]
    pub(crate) fn with_secret_store(mut self, secrets: SecretStore) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Resolve the pending question `request_id` with the client's `inputs`.
    /// The value a secret question collects goes into the secret store here,
    /// before the commit, and what is committed, persisted and delivered to
    /// the agent is only the text that the secret was stored or declined;
    /// the value goes no further. Otherwise as [`Self::resolve`].
    ///
    /// # Errors
    /// Returns [`AskUserRespondError`] when an input does not fit its
    /// question, the secret cannot be stored or the commit fails; nothing is
    /// committed then.
    ///
    /// # Panics
    /// Panics if the approver has no runtime service, or no secret store when
    /// a secret question is answered.
    pub(crate) async fn respond(
        &self,
        request_id: &str,
        inputs: Vec<UiQuestionAnswer>,
    ) -> Result<Option<AskUserResolved>, AskUserRespondError> {
        let runtime = self
            .runtime_service
            .as_ref()
            .expect("resolving an ask-user question needs the runtime service");
        let Some(pending) = runtime
            .db()
            .pending_ask_user_request(request_id)
            .await
            .map_err(AskUserRespondError::Lookup)?
        else {
            // Answered before, or never pending: no input is stored.
            return Ok(self.resolve(request_id, Vec::new()).await?);
        };
        let questions = &pending.request.questions;
        if questions.len() != inputs.len() {
            return Err(crate::session::AnswerError::Mismatch(
                crate::session::open_questions::AnswerMismatch::Count {
                    questions: questions.len(),
                    answers: inputs.len(),
                },
            )
            .into());
        }
        let mut answers = Vec::with_capacity(inputs.len());
        for (question, input) in questions.iter().zip(inputs) {
            let answer = match &question.secret {
                None => input.answer.try_into()?,
                Some(secret) => {
                    self.take_secret(secret, &pending.session_id, input.answer)
                        .await?
                }
            };
            answers.push(QuestionAnswer {
                section: input.section,
                answer,
            });
        }
        Ok(self.resolve(request_id, answers).await?)
    }

    /// Store the value `input` carries as `secret` of `session_id` and
    /// return the answer the agent is told instead of it.
    async fn take_secret(
        &self,
        secret: &PersistedSecretRequest,
        session_id: &str,
        input: UiAnswerInput,
    ) -> Result<AnswerValue, AskUserRespondError> {
        let text = match input {
            UiAnswerInput::Custom { text } => {
                let value = text.trim().to_string();
                if value.is_empty() {
                    return Err(UiAnswerInputError::BlankCustom.into());
                }
                let store = self
                    .secrets
                    .clone()
                    .expect("answering a secret question needs the secret store");
                let (scope, name, session) =
                    (secret.scope, secret.name.clone(), session_id.to_string());
                blocking::unblock(move || store.set_scoped(scope, &session, &name, &value))
                    .await
                    .map_err(|source| AskUserRespondError::Store {
                        name: secret.name.clone(),
                        source,
                    })?;
                SecretStoredText {
                    name: &secret.name,
                    scope: secret.scope,
                }
                .render()
            }
            UiAnswerInput::Declined => SecretDeclinedText { name: &secret.name }.render(),
            UiAnswerInput::Selected { .. } => return Err(UiAnswerInputError::SelectedSecret.into()),
        };
        Ok(AnswerValue::Custom(
            text.expect("secret answer template must render"),
        ))
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
                secret: None,
            })
            .collect()
    }

    pub(crate) async fn enqueue(&self, request: AskUserRequest, session_id: Option<String>) {
        let questions = request.args.request.clone().into_questions();
        let request_session_id = Some(request.args.session_id.clone()).or(session_id);
        let request_id = crate::id::new();
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
            } else {
                runtime_service.announce_user_wait(crate::session::UserWait {
                    session_id: session_id.to_string(),
                    ask: crate::session::UserAsk::question(&persisted_request),
                });
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

    /// Interrupt the pending question `request_id`: the waiter is dropped so
    /// the asking run sees the channel close, and the persisted request is
    /// cleared. Used when the daemon drains — no answer will ever arrive.
    pub(crate) async fn interrupt(&self, session_id: &str, request_id: &str) {
        self.live_responses.take(request_id);
        if let Some(runtime) = &self.runtime_service
            && let Err(error) = runtime
                .clear_waiting_ask_user_request(session_id, request_id)
                .await
        {
            tracing::warn!(%error, "failed to clear an interrupted ask-user request");
        }
    }
}

impl WorkspaceApprover {
    pub(crate) fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_service: None,
            may_dir: None,
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

    #[must_use]
    pub(crate) fn with_may_dir(mut self, may_dir: PathBuf) -> Self {
        self.may_dir = Some(may_dir);
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
        let request_id = crate::id::new();
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

            // A yolo session grants at once, through the same commit a user
            // approval takes.
            if runtime_service
                .db()
                .session_yolo(session_id)
                .await
                .map_err(WorkspaceApprovalError::Yolo)?
            {
                self.resolve(&request_id, true)
                    .await
                    .map_err(|error| WorkspaceApprovalError::YoloGrant(Box::new(error)))?;
            } else {
                runtime_service.announce_user_wait(crate::session::UserWait {
                    session_id: session_id.to_string(),
                    ask: crate::session::UserAsk::Workspace {
                        path: request.path.clone(),
                    },
                });
            }
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
    /// [`WorkspaceResolveError::Commit`] on a storage failure,
    /// [`WorkspaceResolveError::Canonicalize`] when an approved path does not
    /// resolve (nothing is committed and nobody is woken then),
    /// [`WorkspaceResolveError::SessionMissing`] when the committed session
    /// vanished, and [`WorkspaceResolveError::PersistGrant`] when the
    /// decision was committed and the run woken but `workspace.toml`
    /// rejected the grant.
    ///
    /// # Panics
    /// Panics if the approver has no runtime service.
    pub(crate) async fn resolve(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<crate::session::ResolvedRequest>, WorkspaceResolveError> {
        let runtime = self
            .runtime_service
            .as_ref()
            .expect("resolving a workspace request needs the runtime service");
        let canonical = if approved {
            let Some(pending) = runtime.db().pending_workspace(request_id).await? else {
                return Ok(None);
            };
            // Canonicalize before the decision is committed: a path that does
            // not resolve leaves the request pending for another approval.
            Some(
                async_fs::canonicalize(&pending.path)
                    .await
                    .map_err(|source| WorkspaceResolveError::Canonicalize {
                        path: pending.path.clone(),
                        source,
                    })?,
            )
        } else {
            None
        };
        let Some(lookup) = runtime
            .commit_workspace_response(request_id, approved)
            .await?
        else {
            return Ok(None);
        };
        if let Some(path) = canonical {
            let session = runtime
                .db()
                .get_session(&lookup.session_id)
                .await?
                .ok_or_else(|| WorkspaceResolveError::SessionMissing(lookup.session_id.clone()))?;
            // Persist under the session's profile home so a rebuilt agent
            // keeps the grant, then push it into the running terminal's
            // grant handle before the run wakes.
            let config_path = crate::session::SessionStore::new(crate::session::MayHomePaths::new(
                self.may_dir
                    .clone()
                    .expect("workspace approval needs the may dir"),
            ))
            .session_paths(&lookup.session_id, session.profile_id.as_deref())
            .workspace_config();
            let reason = lookup.reason.clone();
            let grant_path = path.clone();
            let persist = blocking::unblock(
                move || -> Result<(), crate::workspace::WorkspaceConfigError> {
                    let mut config = crate::workspace::WorkspaceConfig::load(&config_path)?;
                    config.add_workdir(grant_path, Some(reason));
                    config.save(&config_path)
                },
            )
            .await;
            if let Some(grants) = runtime.terminal_grants(&lookup.session_id) {
                grants.add_writable(path);
            }
            let session_id = lookup.session_id;
            let woke = self.live_responses.resolve(request_id, approved);
            // The decision is already committed as approved and the run
            // woken: a persist failure still reports so the client knows the
            // grant will not survive a rebuild.
            return match persist {
                Ok(()) => Ok(Some(crate::session::ResolvedRequest { session_id, woke })),
                Err(error) => Err(WorkspaceResolveError::PersistGrant(error)),
            };
        }
        let session_id = lookup.session_id;
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

#[cfg(test)]
impl DaemonState {
    /// A daemon state for tests: a real `ChatDb` and tool machinery on a
    /// temporary may home, with background services absent.
    pub(crate) async fn for_test(
        may_dir: &std::path::Path,
        config: MayConfig,
    ) -> anyhow::Result<Self> {
        Self::for_test_with_connectivity(
            may_dir,
            config,
            aither_connectivity::Connectivity::manual(true).0,
        )
        .await
    }

    /// [`Self::for_test`] on a given connectivity source.
    pub(crate) async fn for_test_with_connectivity(
        may_dir: &std::path::Path,
        config: MayConfig,
        connectivity: aither_connectivity::Connectivity,
    ) -> anyhow::Result<Self> {
        // Shells spawn their job registries on the global executor, which
        // the `may` binary installs at startup.
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        // The daemon writes the bundled skills there at startup, and a
        // session's shell grants the directory.
        std::fs::create_dir_all(may_dir.join("skills"))?;
        let db = crate::db::ChatDb::open(&may_dir.join("history.db")).await?;
        let ui_event_bus = crate::ui_event_bus::UiEventBus::new();
        let (session_change_tx, session_change_rx) = async_channel::unbounded();
        ui_event_bus.set_session_changes(session_change_tx.clone());
        let lease_handle = tokio::runtime::Handle::current();
        let runtime_service = std::sync::Arc::new(
            crate::session::SessionRuntimeService::new(
                db.clone(),
                ui_event_bus.clone(),
                crate::session::MayHomePaths::new(may_dir.to_path_buf()),
            )
            .with_spawner(std::sync::Arc::new(move |task| {
                lease_handle.spawn(task);
            })),
        );
        let (title_generation_queue, _rx) = TitleGenerationQueue::new();
        let (tool_dispatchers, _queues) = tool_dispatch_channels();
        let tool_distribution = std::sync::Arc::new(
            crate::tools::fixture_distribution(&may_dir.join("tool-fixtures")).await,
        );
        let (search_index_wake_tx, search_index_wake) = async_channel::bounded(1);
        let (browser_lifecycle_tx, _) = tokio::sync::broadcast::channel(4);
        let notification_manager = std::sync::Arc::new(
            crate::notification::NotificationManager::new(may_dir, db.clone()).await?,
        );
        let notification_service =
            std::sync::Arc::new(crate::notification::NotificationService::new(
                db.clone(),
                notification_manager,
                ui_event_bus.clone(),
            ));
        let task_runtime = std::sync::Arc::new(
            crate::scheduler::TaskRuntime::load(
                db.clone(),
                may_dir.to_path_buf(),
                tool_distribution.clone(),
            )
            .await?
            .with_secret_store(SecretStore::in_memory(may_dir)),
        );
        let live_limits = crate::agent::LiveLimits::of(&config);
        let config = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(config));
        Ok(Self {
            acp_host: std::sync::Arc::new(crate::agent::acp_subagent::AcpHosts::new(
                config.clone(),
                crate::session::MayHomePaths::new(may_dir.to_path_buf()),
                tokio::runtime::Handle::current(),
            )),
            tokio_handle: tokio::runtime::Handle::current(),
            sleep: std::sync::Arc::new(crate::sleep::SleepState::load(
                may_dir,
                ui_event_bus.clone(),
                db.clone(),
            )),
            db,
            github_api: crate::pr::PrContext::default_github_api(),
            default_profile_id: "test".to_string(),
            mcp_service: McpService::spawn(crate::mcp::McpManager::new(
                crate::mcp::McpConfig::default(),
            )),
            permission_approver: crate::agent::PermissionApprover::new()
                .with_runtime_service(runtime_service.clone()),
            workspace_approver: WorkspaceApprover::new()
                .with_runtime_service(runtime_service.clone())
                .with_may_dir(may_dir.to_path_buf()),
            ask_user_approver: AskUserApprover::new().with_runtime_service(runtime_service.clone()),
            tool_dispatchers,
            fetched_models_cache: RefreshableCache::new(Duration::from_secs(20)),
            config,
            live_limits,
            ui_event_bus: ui_event_bus.clone(),
            container_prepare: ContainerPrepareTracker::new(ui_event_bus.clone()),
            container_manager: None,
            browser_manager: arc_swap::ArcSwapOption::empty(),
            browser_owner_session_id: arc_swap::ArcSwapOption::empty(),
            browser_lifecycle_tx,
            computer: std::sync::Arc::new(crate::computer::ComputerAgent::detached()),
            remote_access_manager: std::sync::Arc::new(
                super::remote_access::RemoteAccessManager::new(ui_event_bus.clone()),
            ),
            notification_service,
            memory_processor: None,
            local_url_port_mapper: std::sync::Arc::new(
                super::local_url_mapper::LocalUrlPortMapper::new(),
            ),
            title_generation_queue,
            location_provider: crate::location::LocationProvider::new(),
            runtime_service,
            task_runtime,
            tool_distribution,
            tool_environments: crate::agent::ToolEnvironmentCache::spawn(),
            job_server: crate::jobserver::JobServer::start(
                may_dir,
                crate::jobserver::default_pool_size(),
            )
            .await
            .expect("test jobserver starts"),
            may_dir: may_dir.to_path_buf(),
            repo_search: crate::repo_search::RepoSearch::global(may_dir),
            search_index_wake_tx,
            search_index_wake,
            search_index_status: crate::repo_search::indexer::IndexerStatus::default(),
            daemon_shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            draining: std::sync::atomic::AtomicBool::new(false),
            build_id: "test".to_string(),
            turn_hub: std::sync::Arc::new(super::chat::TurnHub::default()),
            parked_agents: super::chat::ParkedAgents::default(),
            agent_forks: crate::agent_link::ForkHosts::default(),
            aux_tasks: super::chat::AuxTasks::default(),
            recap_schedules: super::chat::RecapSchedules::default(),
            session_background: super::chat::SessionBackgrounds::default(),
            pr_checks: super::pr_checks::PrChecksCache::default(),
            tunnels: std::sync::Arc::new(super::tunnel::TunnelRegistry::new()),
            connectivity,
            session_events_epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| {
                    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
                }),
            session_change_tx,
            session_change_rx,
            session_publisher_once: std::sync::Once::new(),
        })
    }
}
