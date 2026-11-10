//! Idle compaction and the compaction model, from a saved [`MayConfig`]
//! through May's own agent construction ([`MayAgent::session_config`] and
//! [`MayAgent::new`]) to aither, against stub model servers.
//!
//! Idleness is measured on a [`ManualClock`] handed through
//! [`MayAgentConfig`]. The TUI worker sleeps until
//! [`MayAgent::idle_compaction_due_in`] elapses and then calls
//! [`MayAgent::compact_when_idle`]; the tests advance the clock by that
//! amount instead of sleeping, so the 54 minutes under the default one-hour
//! cache lifetime pass without a real delay. `/compact` is driven with
//! `compact`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use aither_agent::ManualClock;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use super::live_settings::AgentLiveSettings;
use super::may::{
    AgentEnvironment, HostReaderSource, MayAgent, MayAgentConfig, MayAgentSessionInput,
};
use super::tool_environment_cache::ToolEnvironmentCache;
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::model_selector::ModelSelector;
use crate::session::{SessionId, SessionPlanState};

const SESSION_PROVIDER: &str = "session";
const COMPACTION_PROVIDER: &str = "compaction";
pub const SESSION_MODEL: &str = "session-model";
const COMPACTION_MODEL: &str = "compaction-model";
/// Opening words of aither's compaction instruction.
pub const COMPACTION_MARKER: &str = "Your context window is being compacted";

/// What a stub model server received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Traffic {
    turns: usize,
    compactions: usize,
}

/// Stub `OpenAI` server: plain text for a turn, a handoff for a compaction.
pub struct StubModel {
    pub base_url: String,
    pub requests: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl StubModel {
    pub async fn spawn(model: &'static str) -> Self {
        Self::spawn_replying(model, |_| None).await
    }

    /// Like [`Self::spawn`], but `reply` may answer a request body with its
    /// own text; `None` leaves the default.
    pub async fn spawn_replying(model: &'static str, reply: fn(&str) -> Option<String>) -> Self {
        Self::spawn_scripted(model, move |body| reply(body).map(StubReply::Text)).await
    }

    /// Like [`Self::spawn`], but `reply` may answer a request body with text
    /// or a tool call; `None` leaves the default text.
    pub async fn spawn_scripted(
        model: &'static str,
        reply: impl Fn(&str) -> Option<StubReply> + Send + Sync + 'static,
    ) -> Self {
        let reply: ReplyScript = Arc::new(reply);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub model server");
        let addr = listener.local_addr().expect("stub model server address");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let served = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let served = served.clone();
                let reply = reply.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        answer(request, model, reply.clone(), served.clone())
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    async fn traffic(&self) -> Traffic {
        let requests = self.requests.lock().await;
        let compactions = requests
            .iter()
            .filter(|text| text.contains(COMPACTION_MARKER))
            .count();
        Traffic {
            turns: requests.len() - compactions,
            compactions,
        }
    }
}

/// What a scripted stub model answers each request body with; `None` is the
/// default text.
type ReplyScript = Arc<dyn Fn(&str) -> Option<StubReply> + Send + Sync>;

/// What a scripted stub model answers one request with.
pub enum StubReply {
    /// Plain text.
    Text(String),
    /// One call of the tool `name` with JSON `arguments`.
    ToolCall {
        name: &'static str,
        arguments: serde_json::Value,
    },
}

async fn answer(
    request: Request<Incoming>,
    model: &'static str,
    reply_to: ReplyScript,
    requests: Arc<tokio::sync::Mutex<Vec<String>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() == Method::GET && request.uri().path().ends_with("/models") {
        let models = serde_json::json!({
            "data": [{"id": model, "owned_by": "stub", "context_length": 128_000}]
        });
        return Ok(reply(
            StatusCode::OK,
            "application/json",
            models.to_string(),
        ));
    }
    if request.method() != Method::POST || !request.uri().path().ends_with("/chat/completions") {
        let unexpected = format!("{} {}", request.method(), request.uri());
        return Ok(reply(StatusCode::NOT_FOUND, "text/plain", unexpected));
    }
    let body = request
        .into_body()
        .collect()
        .await
        .expect("stub model request body")
        .to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("stub model request is UTF-8");
    let scripted = if text.contains(COMPACTION_MARKER) {
        StubReply::Text(
            serde_json::json!({
                "current_task": "Greet the user.",
                "key_decisions": [],
                "errors_fixes": [],
                "pending_work": [],
                "recovery_hints": [],
            })
            .to_string(),
        )
    } else {
        reply_to(&text).unwrap_or_else(|| StubReply::Text("Hello there.".to_string()))
    };
    let mut served = requests.lock().await;
    let call = served.len();
    served.push(text);
    drop(served);
    let chunk = |delta: &serde_json::Value, finish: serde_json::Value| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": delta, "finish_reason": finish}]})
        )
    };
    let (delta, finish) = match scripted {
        StubReply::Text(content) => (serde_json::json!({"content": content}), "stop"),
        StubReply::ToolCall { name, arguments } => (
            serde_json::json!({"tool_calls": [{
                "index": 0,
                "id": format!("call-{call}"),
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            }]}),
            "tool_calls",
        ),
    };
    let stream = [
        chunk(&delta, serde_json::Value::Null),
        chunk(&serde_json::json!({}), serde_json::Value::from(finish)),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Ok(reply(StatusCode::OK, "text/event-stream", stream))
}

fn reply(status: StatusCode, content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// A host whose machine load the test sets; its memory pressure stays
/// normal. Sessions built in tests read it instead of the machine running
/// them, so a loaded test machine never alerts their agents.
#[derive(Clone, Default)]
pub struct ScriptedHost {
    load_bits: Arc<AtomicU64>,
    reads: Arc<AtomicUsize>,
}

impl ScriptedHost {
    /// Sets the machine load later readings return.
    pub fn set_load(&self, load: f64) {
        self.load_bits.store(load.to_bits(), Ordering::SeqCst);
    }

    /// How many machine load readings were taken.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn source(&self) -> HostReaderSource {
        let host = self.clone();
        HostReaderSource::new(move || aither_agent::AnyHostReader::new(host.clone()))
    }
}

impl aither_agent::HostReader for ScriptedHost {
    fn memory_pressure(&mut self) -> Result<aither_agent::MemoryPressure, aither_agent::ReadError> {
        Ok(aither_agent::MemoryPressure::Normal)
    }

    fn machine_load(&mut self) -> Result<aither_agent::MachineLoad, aither_agent::ReadError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(aither_agent::MachineLoad(f64::from_bits(
            self.load_bits.load(Ordering::SeqCst),
        )))
    }
}

/// What building a session agent needs besides the config.
pub struct SessionParts {
    dir: std::path::PathBuf,
    pub db: crate::db::ChatDb,
    pub(crate) session_id: SessionId,
    /// The runtime the session persists its interactions through, when it has one.
    pub runtime: Option<Arc<crate::session::SessionRuntimeService>>,
    workspace_broker: crate::workspace_request::WorkspaceRequestBroker,
    ask_user_broker: crate::ask_user::AskUserBroker,
    tool_environments: ToolEnvironmentCache,
    job_server: crate::jobserver::JobServer,
    /// The host the session's load monitor reads: calm unless a test loads it.
    pub host: ScriptedHost,
    /// The rules the session imports, when a test makes it an import session.
    pub rules_import: Option<crate::rules_import::RulesImport>,
    /// Messaging between main agents, when a test hosts several.
    pub agent_messaging: Option<crate::agent_link::AgentMessaging>,
    /// The SSH servers the session is built with.
    pub ssh_servers: Vec<crate::config::SshServerConfig>,
    // The brokers' receiving ends stay open for the life of the session.
    _queues: (
        crate::workspace_request::WorkspaceRequestQueue,
        crate::ask_user::AskUserQueue,
    ),
}

impl SessionParts {
    pub(super) async fn new(dir: std::path::PathBuf) -> Self {
        crate::test_support::install_global_executor();
        std::fs::create_dir_all(dir.join(".may")).expect("create the may dir");
        let db = crate::db::ChatDb::open(&dir.join("history.db"))
            .await
            .expect("open test db");
        let (workspace_broker, workspace_queue) = crate::workspace_request::channel();
        let (ask_user_broker, ask_user_queue) = crate::ask_user::channel();
        let job_server = crate::jobserver::JobServer::start(
            &dir.join(".may"),
            crate::jobserver::default_pool_size(),
        )
        .await
        .expect("test jobserver starts");
        Self {
            dir,
            db,
            session_id: SessionId::from_string("amber-river"),
            runtime: None,
            workspace_broker,
            ask_user_broker,
            tool_environments: ToolEnvironmentCache::spawn(),
            job_server,
            host: ScriptedHost::default(),
            rules_import: None,
            agent_messaging: None,
            ssh_servers: Vec::new(),
            _queues: (workspace_queue, ask_user_queue),
        }
    }

    pub(super) async fn input(&self) -> MayAgentSessionInput<'static> {
        let cwd = self.dir.join("cwd");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        MayAgentSessionInput {
            #[cfg(target_os = "macos")]
            computer: None,
            sandbox_dir: self.dir.join("sandbox"),
            may_dir: self.dir.join(".may"),
            cwd,
            session_id: self.session_id.clone(),
            workspace_request_broker: self.workspace_broker.clone(),
            agent_messaging: self.agent_messaging.clone(),
            ask_user_broker: self.ask_user_broker.clone(),
            db: self.db.clone(),
            model: "session::session-model",
            reasoning_effort: None,
            prompt_cache_epoch: 0,
            profile_id: None,
            permission_approver: self.runtime.as_ref().map(|runtime| {
                crate::agent::PermissionApprover::new()
                    .with_runtime_service(runtime.clone())
                    .for_session(self.session_id.as_str())
            }),
            live_config: None,
            profile_persona: None,
            profile_memories: None,
            plan_state: SessionPlanState::default(),
            location: None,
            task_broker: None,
            tunnel_broker: None,
            browser_broker: None,
            acp_host: None,
            register_skills_broker: None,
            environment: AgentEnvironment::Local,
            tool_distribution: Arc::new(
                crate::tools::fixture_distribution(&self.dir.join(".may/tool-fixtures")).await,
            ),
            tool_environments: self.tool_environments.clone(),
            job_server: self.job_server.clone(),
            provider_attempt: 0,
            subagents: None,
            background_jobs: aither_agent::BackgroundJobPolicy::Await,
            connectivity: aither_connectivity::Connectivity::manual(true).0,
            rules_import: self.rules_import.clone(),
            limits: crate::agent::LiveLimits::of(&MayConfig::default()),
            model_context_window: None,
        }
    }
}

/// A session agent built from a saved config that points at stub servers.
pub struct Session {
    pub agent: MayAgent,
    pub clock: ManualClock,
    pub parts: SessionParts,
    pub temp: tempfile::TempDir,
}

fn stub_provider(server: &StubModel) -> ProviderInstance {
    ProviderInstance::new(ProviderType::Custom)
        .with_api_key("test-key")
        .with_base_url(server.base_url.clone())
}

/// A saved config whose session provider is `session` and, when given, whose
/// compaction model lives on `compaction`.
pub fn saved_config(session: &StubModel, compaction: Option<&StubModel>) -> MayConfig {
    let mut config = MayConfig::default();
    config
        .provider
        .insert(SESSION_PROVIDER.to_string(), stub_provider(session));
    config.models.advanced = vec![ModelSelector::new(SESSION_PROVIDER, SESSION_MODEL)];
    config.last_model = Some(ModelSelector::new(SESSION_PROVIDER, SESSION_MODEL));
    if let Some(compaction) = compaction {
        config
            .provider
            .insert(COMPACTION_PROVIDER.to_string(), stub_provider(compaction));
        config.models.compaction_model =
            Some(ModelSelector::new(COMPACTION_PROVIDER, COMPACTION_MODEL));
    }
    config
}

impl Session {
    pub async fn start(config: &MayConfig) -> Self {
        Self::start_with(config, false).await
    }

    /// Starts a session whose cache entry ages on the system clock, for a
    /// test that waits on the idle worker's real timers.
    pub(super) async fn start_on_system_clock(config: &MayConfig) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let parts = SessionParts::new(temp.path().to_path_buf()).await;
        let clock = ManualClock::new();
        let agent = Box::pin(Self::build_on(config, &parts, None, 0)).await;
        Self {
            agent,
            clock,
            parts,
            temp,
        }
    }

    /// Starts a session that persists its interactions through a runtime
    /// service over its database, as the web server's sessions do.
    pub(super) async fn start_with_runtime(config: &MayConfig) -> Self {
        Self::start_with(config, true).await
    }

    /// Starts a session whose agent is one of the main agents `messaging`
    /// connects.
    pub(crate) async fn start_with_messaging(
        config: &MayConfig,
        messaging: crate::agent_link::AgentMessaging,
    ) -> Self {
        Self::start_from(config, false, Some(messaging)).await
    }

    async fn start_with(config: &MayConfig, with_runtime: bool) -> Self {
        Self::start_from(config, with_runtime, None).await
    }

    async fn start_from(
        config: &MayConfig,
        with_runtime: bool,
        agent_messaging: Option<crate::agent_link::AgentMessaging>,
    ) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut parts = SessionParts::new(temp.path().to_path_buf()).await;
        parts.agent_messaging = agent_messaging;
        if with_runtime {
            parts
                .db
                .create_session(
                    parts.session_id.as_str(),
                    Some("Session"),
                    Some("test-model"),
                )
                .await
                .expect("create the session row");
            parts.runtime = Some(Arc::new(crate::session::SessionRuntimeService::new(
                parts.db.clone(),
                crate::ui_event_bus::UiEventBus::new(),
                crate::session::MayHomePaths::new(temp.path().join("home")),
            )));
        }
        let clock = ManualClock::new();
        let agent = Box::pin(Self::build(config, &parts, &clock, 0)).await;
        Self {
            agent,
            clock,
            parts,
            temp,
        }
    }

    /// Builds the agent the way the TUI does: the config resolved by
    /// [`MayAgent::session_config`], then [`MayAgent::new`], with only the
    /// clock and the host the load monitor reads substituted.
    async fn build(
        config: &MayConfig,
        parts: &SessionParts,
        clock: &ManualClock,
        prompt_cache_epoch: u32,
    ) -> MayAgent {
        Self::build_on(config, parts, Some(clock), prompt_cache_epoch).await
    }

    /// Like [`Self::build`], on the system clock when `clock` is `None`.
    async fn build_on(
        config: &MayConfig,
        parts: &SessionParts,
        clock: Option<&ManualClock>,
        prompt_cache_epoch: u32,
    ) -> MayAgent {
        let mut input = parts.input().await;
        input.prompt_cache_epoch = prompt_cache_epoch;
        let mut agent_config: MayAgentConfig = MayAgent::session_config(config, input)
            .await
            .expect("the session config resolves");
        agent_config.clock = clock.cloned();
        agent_config.ssh_servers = parts.ssh_servers.clone();
        agent_config.host_reader = Some(parts.host.source());
        Box::pin(MayAgent::new(agent_config))
            .await
            .expect("the agent is built")
    }

    /// Replaces the agent with one built from `config`, as the TUI does when
    /// a saved change reaches the running session.
    pub(super) async fn rebuild(&mut self, config: &MayConfig) {
        self.agent = Box::pin(Self::build(config, &self.parts, &self.clock, 0)).await;
    }

    /// Replaces the agent with one that continues the prompt-cache epoch of
    /// the agent it replaces, which had reached `epoch`.
    pub(crate) async fn rebuild_at_epoch(&mut self, config: &MayConfig, epoch: u32) {
        self.agent = Box::pin(Self::build(config, &self.parts, &self.clock, epoch)).await;
    }

    /// Sleeps out the idle timer the way the worker does, then compacts if
    /// the agent says it is due. `None` when no idle compaction is pending
    /// or nothing was compacted.
    pub(super) async fn compact_when_due(&mut self) -> Option<()> {
        let due_in = self.agent.idle_compaction_due_in()?;
        self.clock.advance(due_in);
        self.agent
            .compact_when_idle()
            .await
            .expect("the idle compaction succeeds")
            .map(|_| ())
    }
}

/// Runs one user turn to completion on top of history above the policy's
/// compaction floor, so the turn starts a compaction.
pub async fn run_turn(agent: &mut MayAgent) {
    // A context under the policy's compaction floor is left alone, so the
    // turn runs on top of history above it.
    agent.push_message(aither_core::llm::Message::assistant(
        "x".repeat(4 * aither_agent::ContextPolicy::default().compaction_floor),
    ));
    run_short_turn(agent).await;
}

/// Runs one user turn to completion on the history as it is: a short session
/// stays under the compaction floor, so nothing it was told is compacted away.
pub async fn run_short_turn(agent: &mut MayAgent) {
    let _ = agent
        .run("Say hello.", [])
        .drive((), std::future::pending::<()>(), |(), _| async {
            std::ops::ControlFlow::Continue(())
        })
        .await
        .expect("the turn runs");
}

/// Under the default one-hour cache lifetime an idle session compacts once,
/// 54 minutes after its last request and not before, on the session model.
#[tokio::test]
async fn default_lifetime_compacts_the_idle_session_once_at_54_minutes() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start(&saved_config(&stub, None))).await;

    assert_eq!(
        session.agent.idle_compaction_due_in(),
        None,
        "nothing sent yet"
    );
    run_turn(&mut session.agent).await;
    assert_eq!(
        session.agent.idle_compaction_due_in(),
        Some(Duration::from_mins(54)),
        "the compaction is due 54 minutes after the turn"
    );

    session.clock.advance(Duration::from_mins(53));
    assert_eq!(
        session.agent.idle_compaction_due_in(),
        Some(Duration::from_mins(1))
    );
    assert!(
        session
            .agent
            .compact_when_idle()
            .await
            .expect("not due yet")
            .is_none()
    );
    assert_eq!(
        stub.traffic().await,
        Traffic {
            turns: 1,
            compactions: 0
        },
        "nothing compacts at 53 minutes"
    );

    session
        .compact_when_due()
        .await
        .expect("the session compacts at 54 minutes");
    assert_eq!(
        stub.traffic().await,
        Traffic {
            turns: 1,
            compactions: 1
        },
        "the idle compaction runs on the session model"
    );

    assert!(
        session.compact_when_due().await.is_none(),
        "the session compacts once"
    );
    assert_eq!(stub.traffic().await.compactions, 1);
}

/// With a compaction model set the idle session does not compact, however long
/// it sits, and `/compact` goes to that model alone.
#[tokio::test]
async fn compaction_model_disables_idle_compaction_and_serves_compact() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let compaction = StubModel::spawn(COMPACTION_MODEL).await;
    let mut session = Box::pin(Session::start(&saved_config(&stub, Some(&compaction)))).await;

    run_turn(&mut session.agent).await;
    session.clock.advance(Duration::from_mins(54));
    assert_eq!(session.agent.idle_compaction_due_in(), None);
    assert!(session.compact_when_due().await.is_none());
    assert_eq!(
        stub.traffic().await,
        Traffic {
            turns: 1,
            compactions: 0
        },
        "the idle session does not compact at 54 minutes"
    );

    session
        .agent
        .compact(None)
        .await
        .expect("/compact succeeds")
        .expect("there is a conversation to compact");
    assert_eq!(
        stub.traffic().await,
        Traffic {
            turns: 1,
            compactions: 0
        },
        "the session model sees no compaction request"
    );
    assert_eq!(
        compaction.traffic().await,
        Traffic {
            turns: 0,
            compactions: 1
        },
        "/compact runs on the compaction model"
    );
}

/// A saved cache lifetime or compaction model reaches the session once its
/// agent is rebuilt from the saved config, which is what the TUI does when
/// the config no longer matches the settings the running worker was built
/// with.
#[tokio::test]
async fn a_saved_change_reaches_the_rebuilt_session() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let compaction = StubModel::spawn(COMPACTION_MODEL).await;
    let mut config = saved_config(&stub, None);
    let mut session = Box::pin(Session::start(&config)).await;

    run_turn(&mut session.agent).await;
    assert_eq!(
        session.agent.idle_compaction_due_in(),
        Some(Duration::from_mins(54))
    );
    let built_with = AgentLiveSettings::of(&config);

    config
        .set_provider_cache_lifetime(SESSION_PROVIDER, "30m")
        .expect("a duration");
    assert_ne!(built_with, AgentLiveSettings::of(&config));
    Box::pin(session.rebuild(&config)).await;
    run_turn(&mut session.agent).await;
    assert_eq!(
        session.agent.idle_compaction_due_in(),
        Some(Duration::from_mins(24)),
        "the saved 30 minute lifetime sets the idle compaction"
    );

    let built_with = AgentLiveSettings::of(&config);
    config
        .provider
        .insert(COMPACTION_PROVIDER.to_string(), stub_provider(&compaction));
    config.models.compaction_model =
        Some(ModelSelector::new(COMPACTION_PROVIDER, COMPACTION_MODEL));
    assert_ne!(built_with, AgentLiveSettings::of(&config));
    Box::pin(session.rebuild(&config)).await;
    run_turn(&mut session.agent).await;
    assert_eq!(
        session.agent.idle_compaction_due_in(),
        None,
        "the saved compaction model takes over from idle compaction"
    );
}
