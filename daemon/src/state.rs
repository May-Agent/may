pub use may_client::daemon::types::UiAnswerInput;
pub use may_client::daemon::types::UiAnswerInputError;

pub use may_client::daemon::types::UiQuestionAnswer;

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
use crate::live_response::LiveResponseRegistry;
use crate::mcp::{MayMcpServerConfig, McpManager, McpServerInfo};
use crate::secrets::{SecretScope, SecretStore};
use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest, PersistedSecretRequest};
use crate::types::BrowserLifecycleEvent;
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
    pub profile_id: String,
    pub stage: ContainerPrepareStage,
    pub summary: String,
    pub recent_logs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
pub struct ContainerPrepareStatusesPayload {
    pub statuses: Vec<ContainerPrepareStatus>,
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
    pub fn new(ui_event_bus: UiEventBus) -> Self {
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

    pub async fn try_begin(&self, profile_id: &str) -> bool {
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

    pub fn finish(&self, profile_id: &str) {
        let _ = self.tx.send(ContainerPrepareCommand::Finish {
            profile_id: profile_id.to_string(),
        });
    }

    pub fn set_status(&self, status: ContainerPrepareStatus) {
        let _ = self.tx.send(ContainerPrepareCommand::SetStatus { status });
    }

    pub fn get_status(&self, profile_id: &str) -> Option<ContainerPrepareStatus> {
        self.snapshot_rx.borrow().statuses.get(profile_id).cloned()
    }

    pub fn clear_status(&self, profile_id: &str) {
        let _ = self.tx.send(ContainerPrepareCommand::ClearStatus {
            profile_id: profile_id.to_string(),
        });
    }

    pub fn snapshot_payload(&self) -> ContainerPrepareStatusesPayload {
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
    pub session_id: String,
    pub user_message: String,
    pub assistant_response: String,
}

#[derive(Debug, Clone)]
pub struct TitleGenerationQueue {
    tx: tokio::sync::mpsc::UnboundedSender<TitleGenerationTask>,
}

impl TitleGenerationQueue {
    pub fn new() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<TitleGenerationTask>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    pub fn enqueue(&self, task: TitleGenerationTask) {
        let _ = self.tx.send(task);
    }
}

#[derive(Debug, Clone)]
pub struct UiToolDispatchers {
    pub task: TaskBroker,
    pub browser: BrowserBroker,
    pub register_skills: RegisterBroker,
    pub tunnel: crate::agent::tunnel_tool::TunnelBroker,
}

pub struct UiToolQueues {
    pub task: TaskQueue,
    pub browser: BrowserQueue,
    pub register_skills: RegisterQueue,
    pub tunnel: crate::agent::tunnel_tool::TunnelQueue,
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
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: ArcSwapOption::empty(),
            refresh_lock: Mutex::new(()),
        }
    }

    /// The stored value of any age, without refreshing.
    pub fn latest(&self) -> Option<T> {
        self.inner.load_full().map(|cached| cached.value.clone())
    }

    /// Runs `update` under the refresh lock against the current cached value
    /// (at any age) and stores the returned value.
    pub async fn update_with<F, Fut, R>(&self, update: F) -> R
    where
        F: FnOnce(Option<T>) -> Fut + Send,
        Fut: Future<Output = (Option<T>, R)> + Send,
    {
        let _refresh_guard = self.refresh_lock.lock().await;
        let current = self.latest();
        let (next, output) = update(current).await;
        if let Some(value) = next {
            self.inner.store(Some(Arc::new(CachedValue {
                updated_at: TokioInstant::now(),
                value,
            })));
        }
        output
    }

    pub async fn get_or_refresh<F, Fut>(&self, refresh: F) -> T
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
    pub fn spawn(mut manager: McpManager) -> Self {
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

    pub async fn server_info(&self) -> Vec<McpServerInfo> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(McpCommand::ServerInfo { reply: reply_tx })
            .expect("MCP actor channel unexpectedly closed");
        reply_rx
            .await
            .expect("MCP actor dropped server_info response")
    }

    pub async fn add_server(
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

    pub async fn remove_server(&self, name: String) -> Result<(), aither_mcp::McpError> {
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

    pub async fn reconnect(&self, name: String) -> Result<(), aither_mcp::McpError> {
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

/// The daemon's shared state: sessions, agents, registries, and services.
pub struct DaemonState {
    /// Runs the connected ACP agents, one host per session; reads the live
    /// config on every spawn.
    pub acp_host: Arc<crate::agent::acp_subagent::AcpHosts>,
    /// The runtime the daemon's background work runs on.
    pub tokio_handle: tokio::runtime::Handle,
    /// The daemon's database.
    pub db: crate::db::ChatDb,
    /// The host's GitHub REST client, for the repository picker.
    pub github_api: crate::issue::github::GitHubRest<crate::pr::PrTokenSource>,
    /// The daemon's forge authenticator, swapped in step with `config` so an
    /// OAuth refresh or a `[forge]` edit applies everywhere at once.
    pub forge_auth: crate::forge::LiveForgeAuth,
    /// Device authorizations `oauth/start` opened, keyed by
    /// `(host, device_code)`, so `oauth/poll` knows each flow's interval.
    pub forge_device_flows: std::sync::Mutex<
        std::collections::HashMap<(String, String), crate::forge::DeviceAuthorization>,
    >,
    pub default_profile_id: String,
    pub mcp_service: McpService,
    pub permission_approver: PermissionApprover,
    pub workspace_approver: WorkspaceApprover,
    pub ask_user_approver: AskUserApprover,
    pub tool_dispatchers: UiToolDispatchers,
    pub fetched_models_cache: RefreshableCache<Vec<crate::models::FetchedModel>>,
    pub config: Arc<ArcSwap<MayConfig>>,
    /// The limits every agent is built with, moved by each config change.
    pub live_limits: crate::agent::LiveLimits,
    pub ui_event_bus: UiEventBus,
    pub container_prepare: ContainerPrepareTracker,
    pub container_manager: ArcSwapOption<crate::container::lifecycle::ContainerManager>,
    pub pending_transcripts: super::disk_exhaustion::PendingTranscriptStore,
    pub browser_manager: ArcSwapOption<crate::container::browser::BrowserManager>,
    pub browser_owner_session_id: ArcSwapOption<String>,
    /// Sequential lifecycle facts emitted by the browser actor, fanned out
    /// to every active SSE client subscribed to `browser_lifecycle`.
    pub browser_lifecycle_tx: broadcast::Sender<BrowserLifecycleEvent>,
    /// Resident May Bot service managing bots, inbox loops, delegates, approvals, and memory.
    pub bot_service: Arc<super::bots::DaemonBotService>,
    /// Live `bot_*` SSE events fanned out onto `/api/events`.
    pub bot_events_tx: broadcast::Sender<may_protocol::bot::BotSseEvent>,
    /// The `computer-agent` helper client — every session's desktop
    /// (display, stream, input) lives in that respawnable process.
    pub computer: Arc<crate::computer::ComputerAgent>,
    pub remote_access_manager: Arc<super::remote_access::RemoteAccessManager>,
    /// The daemon's TCP binds: the `[daemon] port` local listener, the
    /// `[daemon] listen` extras and a `rest` run's explicit address.
    pub tcp_listeners: super::TcpListeners,
    /// The cloud sessions this daemon fronts: their codespaces, hosts and
    /// the steps of bringing them up.
    pub cloud_sessions: Arc<super::cloud_sessions::CloudSessions>,
    /// Reverse peer-agent RPC queue when this daemon runs inside a fronted cloud home.
    pub cloud_home_bridge: super::cloud_home::CloudHomeBridge,
    /// Live `/api/events?topics=swarm` SSE mirror state across reachable tailnet peers.
    pub swarm_mirrors: super::swarm_peers::SwarmMirrorState,
    /// Buffered routine PR/lifecycle swarm notices waiting for `Suspended` codespaces.
    pub buffered_cloud_notices: super::swarm_peers::BufferedCloudNotices,
    /// On a cloud home, the providers the fronting daemon pushed: kept
    /// apart from `config`, so no configuration write carries them.
    pub pushed_providers: crate::cloud::access::PushedProviders,
    /// Whether the user is asleep, for every session and client.
    pub sleep: Arc<crate::sleep::SleepState>,
    pub notification_service: Arc<crate::notification::NotificationService>,
    pub memory_processor: Option<crate::memory::MemoryProcessor>,
    pub local_url_port_mapper: Arc<LocalUrlPortMapper>,
    pub title_generation_queue: TitleGenerationQueue,
    pub location_provider: crate::location::LocationProvider,
    pub runtime_service: Arc<crate::session::SessionRuntimeService>,
    /// Hidden-context prompts delivered to a session's queue and woken with:
    /// subagent approval requests and parked-run results reach parents here.
    pub session_prompts: crate::task_wake::SessionPrompts,
    pub task_runtime: Arc<crate::scheduler::TaskRuntime>,
    /// Embedded baseline tool distribution shared by agents, tasks and containers.
    pub tool_distribution: Arc<crate::tools::ToolDistribution>,
    /// Prepared tool environments reused by every agent rebuilt for a session.
    pub tool_environments: crate::agent::ToolEnvironmentCache,
    /// The machine-wide jobserver pool every terminal draws its jobs from.
    pub job_server: crate::jobserver::JobServer,
    pub may_dir: PathBuf,
    /// Wakes the search indexer after new messages land.
    pub search_index_wake_tx: async_channel::Sender<()>,
    /// The indexer's side of [`Self::search_index_wake_tx`].
    pub search_index_wake: async_channel::Receiver<()>,
    /// Latest outcome of the search indexer.
    pub search_index_status: crate::repo_search::indexer::IndexerStatus,
    /// The compiled quota-reset matchers turns parse upstream errors with.
    pub quota_patterns: crate::quota::QuotaPatterns,
    /// Native on-device OCR & token-saving downscaler for text-dense images.
    pub vision_ocr: crate::vision_ocr::VisionOcr,
    /// The compiled secret shapes bash-mode output is masked with.
    pub secret_patterns: crate::secrets::SecretPatterns,
    /// The repo-search handle and PR land locks every session's agent shares.
    pub services: crate::terminal_commands::SharedServices,
    /// Signalled by `POST /api/daemon/shutdown`.
    pub daemon_shutdown: Arc<tokio::sync::Notify>,
    /// Set once shutdown starts draining: new turns are refused so the
    /// run count only ever falls.
    pub draining: std::sync::atomic::AtomicBool,
    /// Build of the running executable; clients with another build replace
    /// the daemon.
    pub build_id: String,
    /// Live turns fanned out to every client of their session.
    pub turn_hub: Arc<super::chat::TurnHub>,
    /// Each session's agent between turns.
    pub parked_agents: super::chat::ParkedAgents,
    /// In-flight session pre-warm tasks spawned on session creation.
    pub session_prewarms: super::chat::SessionPrewarms,
    /// Serializes concurrent `repo_gate` runs for the same session so pre-warm
    /// and the first turn never duplicate forge API lookups.
    pub repo_gate_locks:
        std::sync::Mutex<std::collections::HashMap<String, Arc<async_lock::Mutex<()>>>>,
    /// Each session agent's fork source and model, working or idle, for
    /// `btw` questions from other agents.
    pub agent_forks: crate::agent_link::ForkHosts,
    /// Each session's in-flight aux task (suggestion, recap), if any.
    pub aux_tasks: super::chat::AuxTasks,
    /// Each session's armed idle-recap timer, if any.
    pub recap_schedules: super::chat::RecapSchedules,
    /// Each session's background jobs and subagent runs, across turns.
    pub session_background: super::chat::SessionBackgrounds,
    /// The live checks of every in-flight GitHub PR, as the poller last read
    /// them.
    pub pr_checks: super::pr_checks::PrChecksCache,
    pub tunnels: Arc<super::tunnel::TunnelRegistry>,
    /// The machine's reachability and sleep/wake state: the one source every
    /// agent and terminal the daemon builds watches, and the turn runner
    /// waits on after a network failure.
    pub connectivity: aither_connectivity::Connectivity,
    /// Epoch of the session-event cursors, fixed at daemon start (ns): a
    /// cursor another daemon minted fails its epoch check and resyncs.
    pub session_events_epoch: u64,
    /// Producers' side of the session-change channel; the dispatcher on the
    /// receiver recomputes and diffs each piece.
    pub session_change_tx: super::session_events::SessionChangeSender,
    pub session_change_rx: super::session_events::SessionChangeReceiver,
    /// The dispatcher spawns once, when the daemon starts or the first
    /// session stream attaches.
    pub session_publisher_once: std::sync::Once,
}

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonState").finish_non_exhaustive()
    }
}

impl DaemonState {
    pub fn mcp(&self) -> McpService {
        self.mcp_service.clone()
    }

    pub fn db(&self) -> crate::db::ChatDb {
        self.db.clone()
    }

    /// The context window of `selector`: what its provider reports in the
    /// fetched model list, else the `aither_models` table. The list is read
    /// from the cache at any age; when the cache holds no list for that
    /// provider and the static table cannot answer, only the selector's own
    /// provider is queried (coalesced and persisted back into the cache) so
    /// unrelated slow providers never stall turn startup.
    pub async fn model_context_window(&self, selector: &crate::ModelSelector) -> Option<u32> {
        let provider_id = selector.provider_id().to_string();
        let model_id = selector.model_id().to_string();
        if let Some(cached) = self.fetched_models_cache.latest()
            && cached.iter().any(|model| model.provider_id == provider_id)
        {
            return crate::models::resolve_context_window(&cached, &provider_id, &model_id);
        }
        if let Some(window) = crate::models::resolve_context_window(&[], &provider_id, &model_id) {
            return Some(window);
        }
        let config = self.config_snapshot();
        let instance = config.provider.get(&provider_id).cloned()?;
        self.fetched_models_cache
            .update_with(|current| async move {
                if let Some(cached) = current.as_ref()
                    && cached.iter().any(|model| model.provider_id == provider_id)
                {
                    return (
                        None,
                        crate::models::resolve_context_window(cached, &provider_id, &model_id),
                    );
                }
                let result =
                    crate::models::fetch_models_for_configured_provider(&provider_id, &instance)
                        .await;
                let window =
                    crate::models::resolve_context_window(&result.models, &provider_id, &model_id);
                if result.models.is_empty() {
                    return (None, window);
                }
                let mut merged = current.unwrap_or_default();
                merged.retain(|model| model.provider_id != provider_id);
                merged.extend(result.models);
                (Some(merged), window)
            })
            .await
    }

    pub fn repo_gate_lock(&self, session_id: &str) -> Arc<async_lock::Mutex<()>> {
        let mut locks = self
            .repo_gate_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(async_lock::Mutex::new(()))),
        )
    }

    pub fn config_snapshot(&self) -> Arc<MayConfig> {
        self.config.load_full()
    }

    /// The tool distribution's `heel`, prepared in the home's tools
    /// directory, which runs the sandboxes of ACP probes and trials.
    pub async fn heel(&self) -> Result<heel::HeelBinary, crate::tools::ToolRuntimeError> {
        crate::tools::prepare_heel(
            &self.tool_distribution,
            &crate::session::MayHomePaths::new(self.may_dir.clone()).tools_dir(),
        )
        .await
    }

    pub fn replace_config(&self, config: MayConfig) {
        self.config.store(Arc::new(config));
    }

    pub async fn save_and_update_config(
        &self,
        update: impl FnOnce(&mut MayConfig),
    ) -> Result<MayConfig, crate::config::ConfigError> {
        let mut config = (*self.config.load_full()).clone();
        update(&mut config);
        self.save_config(&config).await?;
        self.config.store(Arc::new(config.clone()));
        self.forge_auth.update(&config);
        Ok(config)
    }

    /// Writes `config` to this daemon's `config.toml`, in its own May
    /// directory.
    ///
    /// # Errors
    /// Fails when the configuration does not validate or the write fails.
    pub async fn save_config(&self, config: &MayConfig) -> Result<(), crate::config::ConfigError> {
        config.save_to(&self.may_dir.join("config.toml")).await
    }

    pub fn browser_manager(&self) -> Option<Arc<crate::container::browser::BrowserManager>> {
        self.browser_manager.load_full()
    }

    pub fn browser_owner_session_id(&self) -> Option<String> {
        self.browser_owner_session_id
            .load_full()
            .map(|session_id| session_id.as_ref().clone())
    }

    pub fn set_browser_owner_session_id(&self, session_id: Option<String>) {
        self.browser_owner_session_id
            .store(session_id.map(Arc::new));
    }

    pub fn set_browser_manager(
        &self,
        browser_manager: Option<Arc<crate::container::browser::BrowserManager>>,
    ) {
        self.browser_manager.store(browser_manager);
    }

    /// Wires active-turn hold detection and idle-stop cleanup onto `manager`.
    ///
    /// `[browser] idle_timeout` shuts down Chrome once no active turn, WebRTC
    /// viewer, or user takeover holds it.
    pub fn attach_browser_idle_hooks(
        self: &Arc<Self>,
        manager: &crate::container::browser::BrowserManager,
    ) {
        let active_weak = Arc::downgrade(self);
        let stop_weak = Arc::downgrade(self);
        manager.set_idle_hooks(crate::container::browser::BrowserIdleHooks {
            is_turn_active: Some(Arc::new(move || {
                active_weak.upgrade().is_some_and(|state| {
                    state
                        .browser_owner_session_id()
                        .is_some_and(|owner| state.runtime_service.runs().is_held(&owner))
                })
            })),
            on_idle_stop: Some(Arc::new(move || {
                if let Some(state) = stop_weak.upgrade() {
                    state.set_browser_owner_session_id(None);
                    state.notify_ui_events(UiEventTopics::BROWSER_STATUS);
                }
            })),
        });
    }

    /// Resets the managed browser's idle timer when `session_id` currently owns it.
    pub fn touch_browser_if_owned(&self, session_id: &str) {
        if self.browser_owner_session_id().as_deref() == Some(session_id)
            && let Some(manager) = self.browser_manager()
        {
            manager.touch();
        }
    }

    /// Returns a live-resource browser reader scoped to `session_id`.
    pub fn browser_live_reader(
        self: &Arc<Self>,
        session_id: String,
    ) -> crate::agent::AnyBrowserLiveReader {
        crate::agent::AnyBrowserLiveReader::new(DaemonBrowserLiveReader {
            state: Arc::downgrade(self),
            session_id,
        })
    }

    /// Tears down all live in-memory resources owned by `session_id`.
    ///
    /// Cancels pre-warm tasks, evicts any parked agent, terminates active debug
    /// sessions, kills session background jobs and open `tui` sessions, cancels
    /// subagents, and releases unheld managed browser ownership.
    pub async fn teardown_session_resources(&self, session_id: &str) {
        self.session_prewarms.cancel(session_id);
        self.parked_agents.evict_and_wait(session_id).await;
        self.services.debug.teardown_session(session_id).await;
        let stopped = self.session_background.teardown_session(session_id).await;
        if stopped > 0 {
            tracing::info!(
                session_id,
                stopped,
                "stopped live resources for torn-down session"
            );
        }
        self.release_unheld_browser_for_session(session_id).await;
    }

    /// Tears down every session's active debug sessions, live jobs, `tui`
    /// sessions, subagents, parked agents, and running managed browser during
    /// daemon shutdown.
    pub async fn teardown_all_resources(&self) {
        self.session_prewarms.cancel_all();
        self.parked_agents.evict_all_and_wait().await;
        self.bot_service.shutdown().await;
        self.services.debug.shutdown_all().await;
        let stopped = self.session_background.teardown_all().await;
        if stopped > 0 {
            tracing::info!(
                stopped,
                "stopped remaining live session resources on daemon shutdown"
            );
        }
        if let Some(manager) = self.browser_manager()
            && manager.is_running()
            && let Err(error) = manager.stop().await
        {
            tracing::warn!(%error, "failed to stop managed browser on daemon shutdown");
        }
        self.set_browser_owner_session_id(None);
    }

    async fn release_unheld_browser_for_session(&self, session_id: &str) {
        if self.browser_owner_session_id().as_deref() != Some(session_id) {
            return;
        }
        let Some(manager) = self.browser_manager() else {
            self.set_browser_owner_session_id(None);
            return;
        };
        let takeover_held = manager.has_takeover()
            || self
                .runtime_service
                .find_browser_takeover_session()
                .await
                .ok()
                .flatten()
                .is_some();
        if manager.active_peer_count() > 0 || takeover_held {
            self.set_browser_owner_session_id(None);
            self.notify_ui_events(UiEventTopics::BROWSER_STATUS);
            return;
        }
        if manager.is_running()
            && let Err(error) = manager.stop().await
        {
            tracing::warn!(
                session_id,
                %error,
                "failed to stop managed browser on session teardown"
            );
        }
        self.set_browser_owner_session_id(None);
        self.notify_ui_events(UiEventTopics::BROWSER_STATUS);
    }

    pub fn browser_lifecycle_subscribe(&self) -> broadcast::Receiver<BrowserLifecycleEvent> {
        self.browser_lifecycle_tx.subscribe()
    }

    /// Subscribes to live `bot_*` events multiplexed onto `/api/events`.
    pub fn bot_events_subscribe(&self) -> broadcast::Receiver<may_protocol::bot::BotSseEvent> {
        self.bot_events_tx.subscribe()
    }

    /// Queues `kind` for the session's diff publisher, which recomputes
    /// the piece and emits `*.upserted` / `*.removed` / `*.changed` frames
    /// on `/api/sessions/{id}/events` for the real differences.
    pub fn publish_session_change(
        &self,
        session_id: &str,
        kind: super::session_events::SessionChange,
    ) {
        super::session_events::notify_change(&self.session_change_tx, session_id, kind);
    }

    /// Starts the session-change dispatcher when it has not started yet.
    pub fn ensure_session_publisher(self: &Arc<Self>) {
        self.session_publisher_once.call_once(|| {
            super::session_events::spawn(self.clone(), self.session_change_rx.clone());
        });
    }

    /// The cursor `GET /api/sessions/{id}` reports: epoch plus the index a
    /// subscriber naming it as `after` continues from.
    pub fn session_cursor(&self, session_id: &str) -> String {
        super::session_events::format_cursor(
            self.session_events_epoch,
            self.turn_hub.cursor_index(session_id),
        )
    }
}

struct DaemonBrowserLiveReader {
    state: std::sync::Weak<DaemonState>,
    session_id: String,
}

impl crate::agent::BrowserLiveReader for DaemonBrowserLiveReader {
    fn status(&self) -> Option<crate::agent::LiveBrowserStatus> {
        let state = self.state.upgrade()?;
        if state.browser_owner_session_id().as_deref() != Some(self.session_id.as_str()) {
            return None;
        }
        let manager = state.browser_manager()?;
        if !manager.is_running() {
            return None;
        }
        Some(crate::agent::LiveBrowserStatus {
            running: true,
            cdp_endpoint: manager.cdp_endpoint(),
            active_peers: manager.active_peer_count(),
            user_takeover: manager.has_takeover(),
        })
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

/// If `answers` selected `"Always allow for session"`, returns the question
/// prompt text so a resumed run records the session-wide SSH grant.
fn session_allow_question<'a>(
    request: &'a PersistedAskUserRequest,
    answers: &[QuestionAnswer],
) -> Option<&'a str> {
    let question = request.questions.first()?;
    let choice = PermissionChoice::from_options(&question.option)?;
    choice
        .allows_for_session(answers)
        .then_some(question.question.as_str())
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
    pub fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_service: None,
            secrets: None,
        }
    }

    #[must_use]
    pub fn with_secret_store(mut self, secrets: SecretStore) -> Self {
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
    pub async fn respond(
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
    pub fn with_runtime_service(
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

    pub async fn enqueue(&self, request: AskUserRequest, session_id: Option<String>) {
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

        if request.is_closed() {
            if let Some(session_id) = request_session_id.as_deref() {
                self.interrupt(session_id, &request_id).await;
            } else {
                self.live_responses.take(&request_id);
            }
            return;
        }

        // The wait ends on an answer, on the registry closing (the server is
        // going down), or on the requester itself going away — a stopped turn
        // or a lost client can never answer, and its persisted request is
        // cleared the way a drain clears it, or it would pin the session in
        // waiting_ask_user forever.
        let answers = {
            let recv = rx.recv();
            let gone = request.closed();
            futures::pin_mut!(recv, gone);
            match futures::future::select(recv, gone).await {
                futures::future::Either::Left((result, _)) => result.ok(),
                futures::future::Either::Right(_) => None,
            }
        };
        if let Some(answers) = answers {
            let _ = request.respond(answers);
        } else if let Some(session_id) = request_session_id.as_deref() {
            self.interrupt(session_id, &request_id).await;
        } else {
            self.live_responses.take(&request_id);
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
    pub async fn resolve(
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
            if let Some(question) = session_allow_question(&committed.request, &answers) {
                runtime
                    .db()
                    .append_granted_permission(
                        &committed.session_id,
                        crate::PermissionHistoryMode::Ssh,
                        question,
                        None,
                    )
                    .await?;
            }
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
    pub async fn interrupt(&self, session_id: &str, request_id: &str) {
        self.live_responses.take(request_id);
        if let Some(runtime) = &self.runtime_service
            && let Err(error) = runtime
                .clear_waiting_ask_user_request(session_id, request_id)
                .await
        {
            tracing::warn!(%error, "failed to clear an interrupted ask-user request");
        }
    }

    /// Interrupt any blocking pending question for `session_id`: closes its
    /// live response waiter and clears the persisted blocking request.
    pub async fn interrupt_session(&self, session_id: &str) {
        let Some(runtime) = &self.runtime_service else {
            return;
        };
        match runtime.get(session_id).await {
            Ok(summary) => {
                if let Some(pending) = summary.waiting_ask_user {
                    self.interrupt(session_id, &pending.id).await;
                }
            }
            Err(error) => {
                tracing::warn!(
                    session_id,
                    %error,
                    "failed to load runtime while interrupting session ask-user"
                );
            }
        }
    }
}

impl WorkspaceApprover {
    pub fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            runtime_service: None,
            may_dir: None,
        }
    }

    #[must_use]
    pub fn with_runtime_service(
        mut self,
        runtime_service: Arc<crate::session::SessionRuntimeService>,
    ) -> Self {
        self.runtime_service = Some(runtime_service);
        self
    }

    #[must_use]
    pub fn with_may_dir(mut self, may_dir: PathBuf) -> Self {
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

    pub async fn request_and_wait(
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
    pub async fn resolve(
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
    pub fn notify_ui_events(&self, topics: UiEventTopics) {
        self.ui_event_bus.publish(topics);
    }

    pub async fn resolve_container_manager(
        &self,
        allow_managed_podman_download: bool,
    ) -> Result<Arc<crate::container::lifecycle::ContainerManager>, crate::container::ContainerError>
    {
        if let Some(manager) = self.container_manager.load_full() {
            let healthy = futures_lite::future::or(
                async { manager.runtime().client.ping().await.is_ok() },
                async {
                    async_io::Timer::after(Duration::from_secs(2)).await;
                    false
                },
            )
            .await;
            if healthy {
                return Ok(manager);
            }
            tracing::warn!("cached container runtime no longer responds; re-detecting runtime");
            self.container_manager.store(None);
        }

        let runtime = crate::container::ContainerRuntime::detect_with_preference_and_options(
            aither_sandbox::RuntimePreference::default(),
            crate::container::RuntimeDetectOptions {
                allow_managed_podman_download,
            },
        )
        .await?;
        let manager = Arc::new(
            crate::container::lifecycle::ContainerManager::new(
                Arc::new(runtime),
                self.may_dir.clone(),
            )
            .with_live_config(self.config.clone()),
        );
        self.container_manager.store(Some(manager.clone()));
        Ok(manager)
    }

    pub async fn try_begin_container_prepare(&self, profile_id: &str) -> bool {
        self.container_prepare.try_begin(profile_id).await
    }

    pub fn finish_container_prepare(&self, profile_id: &str) {
        self.container_prepare.finish(profile_id);
    }

    pub fn set_container_prepare_status(&self, status: ContainerPrepareStatus) {
        self.container_prepare.set_status(status);
    }

    pub fn get_container_prepare_status(&self, profile_id: &str) -> Option<ContainerPrepareStatus> {
        self.container_prepare.get_status(profile_id)
    }

    pub fn clear_container_prepare_status(&self, profile_id: &str) {
        self.container_prepare.clear_status(profile_id);
    }

    pub fn snapshot_container_prepare_statuses(&self) -> ContainerPrepareStatusesPayload {
        self.container_prepare.snapshot_payload()
    }
}

#[cfg(test)]
impl DaemonState {
    /// A daemon state for tests: a real `ChatDb` and tool machinery on a
    /// temporary may home, with background services absent.
    pub async fn for_test(may_dir: &std::path::Path, config: MayConfig) -> anyhow::Result<Self> {
        Self::for_test_with_connectivity(
            may_dir,
            config,
            aither_connectivity::Connectivity::manual(true).0,
        )
        .await
    }

    /// Mutates the live config in memory only; tests that need the edit on
    /// disk use [`Self::save_and_update_config`].
    pub fn update_config<R>(&self, update: impl FnOnce(&mut MayConfig) -> R) -> MayConfig {
        let mut config = (*self.config.load_full()).clone();
        let _ = update(&mut config);
        self.config.store(Arc::new(config.clone()));
        config
    }

    /// [`Self::for_test`] on a given connectivity source.
    pub async fn for_test_with_connectivity(
        may_dir: &std::path::Path,
        config: MayConfig,
        connectivity: aither_connectivity::Connectivity,
    ) -> anyhow::Result<Self> {
        crate::test_support::install_global_executor();
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
        let forge_auth = crate::forge::LiveForgeAuth::new(&config);
        let config = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(config));
        let sleep = std::sync::Arc::new(crate::sleep::SleepState::load(
            may_dir,
            ui_event_bus.clone(),
            db.clone(),
        ));
        let pushed_providers = crate::cloud::access::PushedProviders::default();
        let services = crate::terminal_commands::SharedServices::new(may_dir)
            .with_forge_auth(forge_auth.clone());
        let (bot_events_tx, _) = tokio::sync::broadcast::channel(64);
        let bot_model_factory = std::sync::Arc::new(super::bots::DaemonBotModelFactory::new(
            config.clone(),
            pushed_providers.clone(),
            may_dir.to_path_buf(),
        ));
        let bot_curator_factory = std::sync::Arc::new(super::bots::DaemonCuratorFactory::new(
            db.clone(),
            may_dir.to_path_buf(),
            tool_distribution.clone(),
            (*bot_model_factory).clone(),
        ));
        let bot_delegate_runner = super::bots::build_daemon_delegate_runner(
            db.clone(),
            may_dir.to_path_buf(),
            tool_distribution.clone(),
            (*bot_model_factory).clone(),
        );
        let bot_service = crate::bot::BotService::new(crate::bot::BotServiceDeps {
            db: db.clone(),
            may_dir: may_dir.to_path_buf(),
            model_factory: bot_model_factory,
            curator_factory: bot_curator_factory,
            delegate_runner: bot_delegate_runner,
            search_backend: crate::repo_search::RepoSearchCollections::new(
                services.repo_search.clone(),
            ),
            sessions: std::sync::Arc::new(crate::bot::NoopSessionBridge),
            runtime_service: Some(runtime_service.clone()),
            notifications: Some(notification_service.clone()),
            sleep: Some(sleep.clone()),
            ui_event_bus: ui_event_bus.clone(),
            publish_event: {
                let tx = bot_events_tx.clone();
                std::sync::Arc::new(move |event| {
                    let _ = tx.send(event);
                })
            },
        });
        services.set_bot_approvals(bot_service.approvals());
        Ok(Self {
            tcp_listeners: super::TcpListeners::new(),
            acp_host: std::sync::Arc::new(crate::agent::acp_subagent::AcpHosts::new(
                config.clone(),
                crate::session::MayHomePaths::new(may_dir.to_path_buf()),
                tokio::runtime::Handle::current(),
            )),
            tokio_handle: tokio::runtime::Handle::current(),
            sleep,
            db: db.clone(),
            github_api: crate::pr::PrContext::default_github_api(forge_auth.clone()),
            forge_auth: forge_auth.clone(),
            forge_device_flows: std::sync::Mutex::default(),
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
            config: config.clone(),
            live_limits,
            ui_event_bus: ui_event_bus.clone(),
            container_prepare: ContainerPrepareTracker::new(ui_event_bus.clone()),
            container_manager: arc_swap::ArcSwapOption::empty(),
            pending_transcripts: super::disk_exhaustion::PendingTranscriptStore::default(),
            browser_manager: arc_swap::ArcSwapOption::empty(),
            browser_owner_session_id: arc_swap::ArcSwapOption::empty(),
            browser_lifecycle_tx,
            bot_service,
            bot_events_tx,
            computer: std::sync::Arc::new(crate::computer::ComputerAgent::detached()),
            remote_access_manager: std::sync::Arc::new(
                super::remote_access::RemoteAccessManager::new(ui_event_bus.clone()),
            ),
            cloud_sessions: std::sync::Arc::new(
                super::cloud_sessions::CloudSessions::load(
                    db.clone(),
                    super::cloud_sessions::GitHubCloud::new(forge_auth.clone()),
                    config.clone(),
                    ui_event_bus.clone(),
                )
                .await?,
            ),
            pushed_providers,
            cloud_home_bridge: super::cloud_home::CloudHomeBridge::default(),
            swarm_mirrors: super::swarm_peers::SwarmMirrorState::default(),
            buffered_cloud_notices: super::swarm_peers::BufferedCloudNotices::new(db.clone()),
            notification_service,
            memory_processor: None,
            local_url_port_mapper: std::sync::Arc::new(
                super::local_url_mapper::LocalUrlPortMapper::new(),
            ),
            title_generation_queue,
            location_provider: crate::location::LocationProvider::new(),
            session_prompts: crate::task_wake::SessionPrompts::new(
                runtime_service.clone(),
                async_channel::unbounded().0,
            ),
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
            services,
            quota_patterns: crate::quota::QuotaPatterns::default(),
            vision_ocr: crate::vision_ocr::VisionOcr::new(),
            secret_patterns: crate::secrets::SecretPatterns::default(),
            search_index_wake_tx,
            search_index_wake,
            search_index_status: crate::repo_search::indexer::IndexerStatus::default(),
            daemon_shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            draining: std::sync::atomic::AtomicBool::new(false),
            build_id: "test".to_string(),
            turn_hub: std::sync::Arc::new(super::chat::TurnHub::default()),
            parked_agents: super::chat::ParkedAgents::default(),
            session_prewarms: super::chat::SessionPrewarms::default(),
            repo_gate_locks: std::sync::Mutex::default(),
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
