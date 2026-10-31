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
use std::time::Duration;

use aither_agent::ManualClock;
use bytes::Bytes;
use futures_lite::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use super::live_settings::AgentLiveSettings;
use super::may::{AgentEnvironment, MayAgent, MayAgentConfig, MayAgentSessionInput};
use super::tool_environment_cache::ToolEnvironmentCache;
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::model_selector::ModelSelector;
use crate::session::{SessionId, SessionPlanState};

const SESSION_PROVIDER: &str = "session";
const COMPACTION_PROVIDER: &str = "compaction";
pub(super) const SESSION_MODEL: &str = "session-model";
const COMPACTION_MODEL: &str = "compaction-model";
/// Opening words of aither's compaction instruction.
const COMPACTION_MARKER: &str = "Your context window is being compacted";

/// What a stub model server received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Traffic {
    turns: usize,
    compactions: usize,
}

/// Stub `OpenAI` server: plain text for a turn, a handoff for a compaction.
pub(super) struct StubModel {
    pub(super) base_url: String,
    pub(super) requests: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl StubModel {
    pub(super) async fn spawn(model: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub model server");
        let addr = listener.local_addr().expect("stub model server address");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let served = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let served = served.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| answer(request, model, served.clone()));
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

async fn answer(
    request: Request<Incoming>,
    model: &'static str,
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
    let content = if text.contains(COMPACTION_MARKER) {
        serde_json::json!({
            "current_task": "Greet the user.",
            "key_decisions": [],
            "errors_fixes": [],
            "pending_work": [],
            "recovery_hints": [],
        })
        .to_string()
    } else {
        "Hello there.".to_string()
    };
    requests.lock().await.push(text);
    let chunk = |delta: &serde_json::Value, finish: serde_json::Value| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": delta, "finish_reason": finish}]})
        )
    };
    let stream = [
        chunk(
            &serde_json::json!({"content": content}),
            serde_json::Value::Null,
        ),
        chunk(&serde_json::json!({}), serde_json::Value::from("stop")),
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

/// What building a session agent needs besides the config.
pub(super) struct SessionParts {
    dir: std::path::PathBuf,
    pub(super) db: crate::db::ChatDb,
    session_id: SessionId,
    /// The runtime the session persists its interactions through, when it has one.
    pub(super) runtime: Option<Arc<crate::session::SessionRuntimeService>>,
    workspace_broker: crate::workspace_request::WorkspaceRequestBroker,
    ask_user_broker: crate::ask_user::AskUserBroker,
    tool_environments: ToolEnvironmentCache,
    // The brokers' receiving ends stay open for the life of the session.
    _queues: (
        crate::workspace_request::WorkspaceRequestQueue,
        crate::ask_user::AskUserQueue,
    ),
}

impl SessionParts {
    pub(super) async fn new(dir: std::path::PathBuf) -> Self {
        // The baseline tools are downloaded once into the build directory's
        // `tmp/may-tools`, which the terminal E2E suite shares.
        let shared_tools = std::env::current_exe()
            .expect("test binary path")
            .ancestors()
            .nth(3)
            .expect("test binary sits in the target directory")
            .join("tmp/may-tools");
        std::fs::create_dir_all(&shared_tools).expect("create the shared tool cache");
        std::fs::create_dir_all(dir.join(".may")).expect("create the may dir");
        std::os::unix::fs::symlink(
            &shared_tools,
            crate::session::MayHomePaths::new(dir.join(".may")).tools_dir(),
        )
        .expect("link the shared tool cache");
        let db = crate::db::ChatDb::open(&dir.join("history.db"))
            .await
            .expect("open test db");
        let (workspace_broker, workspace_queue) = crate::workspace_request::channel();
        let (ask_user_broker, ask_user_queue) = crate::ask_user::channel();
        Self {
            dir,
            db,
            session_id: SessionId::new(),
            runtime: None,
            workspace_broker,
            ask_user_broker,
            tool_environments: ToolEnvironmentCache::spawn(),
            _queues: (workspace_queue, ask_user_queue),
        }
    }

    fn input(&self) -> MayAgentSessionInput<'static> {
        let cwd = self.dir.join("cwd");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        MayAgentSessionInput {
            sandbox_dir: self.dir.join("sandbox"),
            may_dir: self.dir.join(".may"),
            cwd,
            session_id: self.session_id.clone(),
            workspace_request_broker: self.workspace_broker.clone(),
            ask_user_broker: self.ask_user_broker.clone(),
            db: self.db.clone(),
            model: "session::session-model",
            reasoning_effort: None,
            profile_id: None,
            permission_approver: self.runtime.as_ref().map(|runtime| {
                crate::agent::PermissionApprover::new()
                    .with_runtime_service(runtime.clone())
                    .for_session(self.session_id.as_str())
            }),
            profile_persona: None,
            profile_memories: None,
            plan_state: SessionPlanState::default(),
            location: None,
            task_broker: None,
            notify_broker: None,
            browser_broker: None,
            register_skills_broker: None,
            register_subagents_broker: None,
            environment: AgentEnvironment::Local,
            tool_distribution: Arc::new(
                crate::tools::embedded_distribution().expect("embedded distribution validates"),
            ),
            tool_environments: self.tool_environments.clone(),
            provider_attempt: 0,
            subagents: None,
            background_jobs: aither_agent::BackgroundJobPolicy::Await,
        }
    }
}

/// A session agent built from a saved config that points at stub servers.
pub(super) struct Session {
    pub(super) agent: MayAgent,
    clock: ManualClock,
    pub(super) parts: SessionParts,
    _temp: tempfile::TempDir,
}

fn stub_provider(server: &StubModel) -> ProviderInstance {
    ProviderInstance::new(ProviderType::Custom)
        .with_api_key("test-key")
        .with_base_url(server.base_url.clone())
}

/// A saved config whose session provider is `session` and, when given, whose
/// compaction model lives on `compaction`.
pub(super) fn saved_config(session: &StubModel, compaction: Option<&StubModel>) -> MayConfig {
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
    pub(super) async fn start(config: &MayConfig) -> Self {
        Self::start_with(config, false).await
    }

    /// Starts a session that persists its interactions through a runtime
    /// service over its database, as the web server's sessions do.
    pub(super) async fn start_with_runtime(config: &MayConfig) -> Self {
        Self::start_with(config, true).await
    }

    async fn start_with(config: &MayConfig, with_runtime: bool) -> Self {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let temp = tempfile::tempdir().expect("tempdir");
        let mut parts = SessionParts::new(temp.path().to_path_buf()).await;
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
            )));
        }
        let clock = ManualClock::new();
        let agent = Box::pin(Self::build(config, &parts, &clock)).await;
        Self {
            agent,
            clock,
            parts,
            _temp: temp,
        }
    }

    /// Builds the agent the way the TUI does: the config resolved by
    /// [`MayAgent::session_config`], then [`MayAgent::new`], with only the
    /// clock substituted.
    async fn build(config: &MayConfig, parts: &SessionParts, clock: &ManualClock) -> MayAgent {
        let mut agent_config: MayAgentConfig = MayAgent::session_config(config, parts.input())
            .await
            .expect("the session config resolves");
        agent_config.clock = Some(clock.clone());
        Box::pin(MayAgent::new(agent_config))
            .await
            .expect("the agent is built")
    }

    /// Replaces the agent with one built from `config`, as the TUI does when
    /// a saved change reaches the running session.
    async fn rebuild(&mut self, config: &MayConfig) {
        self.agent = Box::pin(Self::build(config, &self.parts, &self.clock)).await;
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

/// Runs one user turn to completion.
pub(super) async fn run_turn(agent: &mut MayAgent) {
    let mut stream = agent.run("Say hello.", []);
    while let Some(event) = stream.next().await {
        event.expect("the turn runs");
    }
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
