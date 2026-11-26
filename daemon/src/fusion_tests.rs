//! End-to-end tests for Fusion mode (`ExecutionMode::Fusion`):
//! - REST persistence, validation, live toggle, and orthogonality with `SessionPlanState`
//! - Hidden companion session lifecycle (omitted from session lists, cascading archive/delete)
//! - Shared `SessionRepos` & workspace grants with isolated sandbox/`TMPDIR`/`$MAY_TASKS` and
//!   withheld `main_only`/`main_exclusive` commands
//! - Blocking, async, and mid-run steered `sidekick` turns with incremental working-tree diffs,
//!   surviving background jobs across handoffs, and recovery across simulated daemon restarts
//! - Exclusive `WorkspaceWriteLease` during async `sidekick` runs and once-per-turn direct
//!   lead edit reminders (never misfiring on read-only lead `terminal` calls)
//! - Per-role usage & context rollups vs unchanged `Standard` mode behavior.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode as HyperStatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use skyzen::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

use super::auth::SocketOwner;
use super::daemon_fusion::DaemonSidekickExecutor;
use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::command_policy::WorkspaceWriteLeasePolicy;
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::db::RequestLogParams;
use crate::fusion::{
    FusionService, SIDEKICK_ROLE, SidekickOutcome, SidekickRequest, WorkspaceEditTracker,
};
use crate::model_selector::ModelSelector;
use crate::session::{ExecutionMode, MayHomePaths, SessionPaths, SessionStore};
use aither_sandbox::{
    Interpreter, LaunchLease, SandboxMode, ScriptCheck, ScriptDecision, ScriptDir, ScriptOrigin,
    ScriptPolicy as _, ShellScript,
};

const PROVIDER_ID: &str = "stub";
const LEAD_MODEL: &str = "lead-model";
const SIDEKICK_MODEL: &str = "sidekick-model";

#[derive(Debug)]
enum ScriptedReply {
    Text(String),
    ToolCall {
        name: &'static str,
        arguments: Value,
    },
}

type AsyncReplyHandler = Arc<
    dyn Fn(
            String,
            usize,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ScriptedReply> + Send>>
        + Send
        + Sync,
>;

struct ScriptedOpenAiServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedOpenAiServer {
    async fn spawn(handler: AsyncReplyHandler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().expect("server addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicUsize::new(0));
        let reqs = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let reqs = reqs.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let handler = handler.clone();
                        let reqs = reqs.clone();
                        let counter = counter.clone();
                        async move { handle_openai_request(request, handler, reqs, counter).await }
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
}

async fn handle_openai_request(
    request: Request<Incoming>,
    handler: AsyncReplyHandler,
    requests: Arc<Mutex<Vec<String>>>,
    counter: Arc<AtomicUsize>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() == Method::GET && request.uri().path().ends_with("/models") {
        let models = json!({
            "data": [
                {"id": LEAD_MODEL, "owned_by": "stub", "context_length": 128_000},
                {"id": SIDEKICK_MODEL, "owned_by": "stub", "context_length": 128_000}
            ]
        });
        return Ok(http_reply(
            HyperStatusCode::OK,
            "application/json",
            models.to_string(),
        ));
    }
    if request.method() != Method::POST || !request.uri().path().ends_with("/chat/completions") {
        return Ok(http_reply(
            HyperStatusCode::NOT_FOUND,
            "text/plain",
            "not found".to_string(),
        ));
    }
    let body = request
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("utf8 request");
    let call_index = counter.fetch_add(1, Ordering::SeqCst);
    requests.lock().await.push(text.clone());

    let reply = handler(text, call_index).await;
    let chunk = |delta: &Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": delta, "finish_reason": finish}]})
        )
    };
    let (delta, finish) = match reply {
        ScriptedReply::Text(content) => (json!({"content": content}), "stop"),
        ScriptedReply::ToolCall { name, arguments } => (
            json!({"tool_calls": [{
                "index": 0,
                "id": format!("call-{call_index}"),
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            }]}),
            "tool_calls",
        ),
    };
    let stream = [
        chunk(&delta, Value::Null),
        chunk(&json!({}), Value::from(finish)),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Ok(http_reply(HyperStatusCode::OK, "text/event-stream", stream))
}

fn http_reply(
    status: HyperStatusCode,
    content_type: &'static str,
    body: String,
) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn config_with_stub(base_url: &str) -> MayConfig {
    let mut config = MayConfig::default();
    let provider = ProviderInstance::new(ProviderType::Custom)
        .with_api_key("test-key")
        .with_base_url(base_url.to_string());
    config.provider.insert(PROVIDER_ID.to_string(), provider);
    config.models.advanced = vec![ModelSelector::new(PROVIDER_ID, LEAD_MODEL)];
    config.models.balanced = vec![ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL)];
    config.last_model = Some(ModelSelector::new(PROVIDER_ID, LEAD_MODEL));
    config
}

async fn daemon_state_with(
    config: MayConfig,
) -> (tempfile::TempDir, Arc<DaemonState>, FusionService) {
    crate::test_support::install_global_executor();
    let dir = tempfile::tempdir().expect("temp dir");
    let state = Arc::new(
        DaemonState::for_test(dir.path(), config)
            .await
            .expect("daemon state"),
    );
    let fusion = DaemonSidekickExecutor::service(Arc::clone(&state));
    state.services.set_fusion(fusion.clone());
    if state
        .db()
        .get_profile(&state.default_profile_id)
        .await
        .expect("profile lookup")
        .is_none()
    {
        state
            .db()
            .create_profile(crate::db::CreateProfileParams {
                id: &state.default_profile_id,
                name: "Test",
                environment: crate::db::ProfileEnvironmentKind::Local,
                slug: None,
                icon: None,
                persona: None,
                preferences: None,
                location_aware: false,
            })
            .await
            .expect("create default profile");
    }
    (dir, state, fusion)
}

async fn call_api(
    router: &skyzen::routing::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    builder = builder.header("content-type", "application/json");
    let body = body.map_or_else(skyzen::Body::empty, |value| {
        skyzen::Body::from(serde_json::to_vec(&value).expect("serialize body"))
    });
    let response = match router.go(builder.body(body).expect("request")).await {
        Ok(response) => response,
        Err(error) => skyzen::error_response(&*error),
    };
    let status = response.status();
    let text = response.into_body().into_string().await.expect("body text");
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn eventually(limit: Duration, what: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !condition().await {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn check_write_lease_script(
    policy: &WorkspaceWriteLeasePolicy,
    sandbox: &std::path::Path,
    script: &str,
) -> ScriptDecision {
    let parsed = ShellScript::parse(script);
    let dir = ScriptDir::Host(sandbox.to_path_buf());
    let check = ScriptCheck {
        launch: LaunchLease::fresh().id(),
        script,
        interpreter: Interpreter::Shell,
        shell: Some(parsed.as_ref()),
        origin: ScriptOrigin::Foreground,
        sandbox: SandboxMode::Sandboxed,
        location: None,
        start: &dir,
        call: None,
    };
    policy.check(&check, &()).await
}

#[tokio::test]
async fn fusion_mode_rest_persistence_validation_and_plan_orthogonality() {
    let server = ScriptedOpenAiServer::spawn(Arc::new(|_, _| {
        Box::pin(async { ScriptedReply::Text("ok".to_string()) })
    }))
    .await;
    let (_dir, state, _fusion) = daemon_state_with(config_with_stub(&server.base_url)).await;
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);

    // Config validation requires fusion_sidekick_model when default mode is fusion.
    let mut bad_cfg = MayConfig::default();
    bad_cfg.experimental.execution_mode = crate::config::DefaultExecutionMode::Fusion;
    assert!(
        bad_cfg.validate().is_err(),
        "fusion default mode without fusion_sidekick_model must fail validation"
    );
    bad_cfg.experimental.fusion_sidekick_model =
        Some(ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL));
    assert!(
        bad_cfg.validate().is_ok(),
        "fusion default mode with fusion_sidekick_model passes validation"
    );

    // POST /api/sessions rejects unknown fusion sidekick model with 400.
    let (status, _) = call_api(
        &router,
        "POST",
        "/api/sessions",
        Some(json!({
            "model": format!("{PROVIDER_ID}::{LEAD_MODEL}"),
            "executionMode": {"mode": "fusion", "sidekickModel": "missing::nope"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // POST /api/sessions persists ExecutionMode::Fusion.
    let sidekick_selector = format!("{PROVIDER_ID}::{SIDEKICK_MODEL}");
    let (status, created) = call_api(
        &router,
        "POST",
        "/api/sessions",
        Some(json!({
            "model": format!("{PROVIDER_ID}::{LEAD_MODEL}"),
            "executionMode": {"mode": "fusion", "sidekickModel": sidekick_selector}
        })),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "create response ({status}): {created}"
    );
    let session_id = created["id"].as_str().expect("session id").to_string();
    assert_eq!(
        created["runtime"]["executionMode"]["mode"].as_str(),
        Some("fusion")
    );
    assert_eq!(
        created["runtime"]["executionMode"]["sidekickModel"].as_str(),
        Some(sidekick_selector.as_str())
    );

    let persisted = state
        .db()
        .session_execution_mode(&session_id)
        .await
        .expect("read persisted mode");
    assert_eq!(
        persisted,
        ExecutionMode::Fusion {
            sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
        }
    );

    // Plan state and execution mode toggle orthogonally.
    let (status, patched) = call_api(
        &router,
        "PATCH",
        &format!("/api/sessions/{session_id}"),
        Some(json!({"planState": "draft"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch plan: {patched}");
    assert_eq!(patched["runtime"]["planState"].as_str(), Some("draft"));
    assert_eq!(
        patched["runtime"]["executionMode"]["mode"].as_str(),
        Some("fusion")
    );

    let (status, patched) = call_api(
        &router,
        "PATCH",
        &format!("/api/sessions/{session_id}"),
        Some(json!({"executionMode": {"mode": "standard"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch standard: {patched}");
    assert_eq!(patched["runtime"]["planState"].as_str(), Some("draft"));
    assert!(
        patched["runtime"].get("executionMode").is_none()
            || patched["runtime"]["executionMode"]["mode"].as_str() == Some("standard"),
        "standard mode omits or labels executionMode: {patched}"
    );

    let (status, patched) = call_api(
        &router,
        "PATCH",
        &format!("/api/sessions/{session_id}"),
        Some(json!({
            "executionMode": {"mode": "fusion", "sidekickModel": sidekick_selector}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch fusion: {patched}");
    assert_eq!(patched["runtime"]["planState"].as_str(), Some("draft"));
    assert_eq!(
        patched["runtime"]["executionMode"]["mode"].as_str(),
        Some("fusion")
    );
}

#[tokio::test]
async fn fusion_companion_is_hidden_isolated_and_cascades_archive_and_delete() {
    let server = ScriptedOpenAiServer::spawn(Arc::new(|_, _| {
        Box::pin(async { ScriptedReply::Text("Companion ready.".to_string()) })
    }))
    .await;
    let (_dir, state, fusion) = daemon_state_with(config_with_stub(&server.base_url)).await;
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);

    let parent_id = "fusion-lifecycle-parent";
    state
        .db()
        .create_session_in_profile(
            parent_id,
            Some("Parent"),
            Some(&format!("{PROVIDER_ID}::{LEAD_MODEL}")),
            Some(&state.default_profile_id),
        )
        .await
        .expect("create parent");
    state
        .db()
        .set_session_execution_mode(
            parent_id,
            &ExecutionMode::Fusion {
                sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            },
        )
        .await
        .expect("set fusion mode");
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .create_session_dirs(
            parent_id,
            Some(&state.default_profile_id),
            crate::session::SandboxResourceLayout::Symlinked,
        )
        .await
        .expect("parent dirs");

    let outcome = fusion
        .execute(SidekickRequest {
            parent_session_id: parent_id.to_string(),
            sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            prompt: "Say hi".to_string(),
            async_run: false,
        })
        .await
        .expect("blocking sidekick call succeeds");
    assert!(matches!(outcome, SidekickOutcome::Completed { .. }));

    let companion_id = state
        .db()
        .companion_session_for(parent_id, SIDEKICK_ROLE)
        .await
        .expect("lookup companion")
        .expect("companion linked");
    assert_eq!(
        state
            .db()
            .parent_session_of(&companion_id)
            .await
            .expect("parent lookup")
            .map(|row| row.parent_session_id)
            .as_deref(),
        Some(parent_id)
    );

    // Companion is hidden from user-facing lists (DB and REST).
    let listed = state.db().list_sessions().await.expect("list_sessions");
    assert!(listed.iter().any(|s| s.id == parent_id));
    assert!(
        !listed.iter().any(|s| s.id == companion_id),
        "companion must be omitted from list_sessions"
    );
    let profile_listed = state
        .db()
        .list_sessions_for_profile(&state.default_profile_id)
        .await
        .expect("list_sessions_for_profile");
    assert!(
        !profile_listed.iter().any(|s| s.id == companion_id),
        "companion must be omitted from list_sessions_for_profile"
    );
    let (status, rest_sessions) = call_api(&router, "GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = rest_sessions.as_array().expect("sessions array");
    assert!(arr.iter().any(|s| s["id"].as_str() == Some(parent_id)));
    assert!(
        !arr.iter()
            .any(|s| s["id"].as_str() == Some(companion_id.as_str())),
        "companion must be omitted from GET /api/sessions"
    );

    // Inspect parked companion agent for isolation & withheld commands.
    let companion_sandbox = store.sandbox_dir(&companion_id, Some(&state.default_profile_id));
    let parent_sandbox = store.sandbox_dir(parent_id, Some(&state.default_profile_id));
    assert_ne!(companion_sandbox, parent_sandbox);
    {
        let parked = state
            .parked_agents
            .take(&companion_id)
            .await
            .expect("companion is parked after turn");
        let commands = parked.named_commands();
        for forbidden in [
            "ask_user",
            "request_workspace",
            "task",
            "sidekick",
            "subagent",
            "pr",
            "tunnel",
            "register-skills",
        ] {
            assert!(
                !commands.iter().any(|cmd| cmd == forbidden),
                "sidekick must withhold `{forbidden}`, got: {commands:?}"
            );
        }
        // Re-park companion before testing archive cascade.
        state.parked_agents.park(
            &state,
            &companion_id,
            parked,
            crate::session::RunReleased::already_released(),
        );
    }

    // Archive and unarchive cascade to companion and evict parked agent.
    state
        .db()
        .archive_session(parent_id)
        .await
        .expect("archive parent");
    let companion_row = state
        .db()
        .get_session(&companion_id)
        .await
        .expect("get companion")
        .expect("companion exists");
    assert!(
        companion_row.archived,
        "archiving parent cascades to companion"
    );
    state
        .db()
        .unarchive_session(parent_id)
        .await
        .expect("unarchive parent");
    let companion_row = state
        .db()
        .get_session(&companion_id)
        .await
        .expect("get companion")
        .expect("companion exists");
    assert!(
        !companion_row.archived,
        "unarchiving parent cascades to companion"
    );

    // Deleting parent via REST cascades to companion DB row and sandbox directory.
    let (status, _) = call_api(
        &router,
        "DELETE",
        &format!("/api/sessions/{parent_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        state
            .db()
            .get_session(&companion_id)
            .await
            .expect("check companion deleted")
            .is_none(),
        "deleting parent removes companion row"
    );
    assert!(
        !companion_sandbox.exists(),
        "deleting parent removes companion sandbox directory"
    );
}

#[tokio::test]
async fn fusion_sidekick_blocking_restart_recovery_async_steer_lease_and_usage_rollups() {
    let barrier = Arc::new(Notify::new());
    let async_entered = Arc::new(Notify::new());
    let shared_repo_holder: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

    let b = barrier.clone();
    let entered = async_entered.clone();
    let repo_holder = shared_repo_holder.clone();

    let server = ScriptedOpenAiServer::spawn(Arc::new(move |body, _call_idx| {
        let b = b.clone();
        let entered = entered.clone();
        let repo_holder = repo_holder.clone();
        Box::pin(async move {
            let repo = repo_holder
                .lock()
                .await
                .clone()
                .expect("repo path configured");
            let readme = repo.join("README.md");
            let hello = repo.join("hello.txt");
            let steered = repo.join("steered.txt");

            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let messages = parsed["messages"].as_array().cloned().unwrap_or_default();
            // Find the last user message and count how many tool results followed it.
            let last_user_idx = messages
                .iter()
                .rposition(|msg| msg["role"].as_str() == Some("user"));
            let Some(user_idx) = last_user_idx else {
                return ScriptedReply::Text("ok".to_string());
            };
            let user_text = match &messages[user_idx]["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => other.to_string(),
            };
            let tools_after_user = messages[user_idx + 1..]
                .iter()
                .filter(|msg| msg["role"].as_str() == Some("tool"))
                .count();

            if user_text.contains("Edit README.md, create hello.txt, and start background sleep") {
                match tools_after_user {
                    0 => ScriptedReply::ToolCall {
                        name: "terminal",
                        arguments: json!({
                            "description": "edit shared repo",
                            "timeout": 60,
                            "script": format!(
                                "printf 'sidekick-edit\\n' >> '{}' && printf 'hello-v1\\n' > '{}'",
                                readme.display(),
                                hello.display()
                            )
                        }),
                    },
                    1 => ScriptedReply::ToolCall {
                        name: "terminal",
                        arguments: json!({
                            "description": "start persistent dev helper",
                            "timeout": 300,
                            "script": "sleep 120",
                            "background": true
                        }),
                    },
                    _ => ScriptedReply::Text("Blocking work done.".to_string()),
                }
            } else if user_text.contains("Update hello.txt to v2") {
                if tools_after_user == 0 {
                    ScriptedReply::ToolCall {
                        name: "terminal",
                        arguments: json!({
                            "description": "update hello after restart",
                            "timeout": 60,
                            "script": format!("printf 'hello-v2\\n' > '{}'", hello.display())
                        }),
                    }
                } else {
                    ScriptedReply::Text("Recovered after restart.".to_string())
                }
            } else if user_text.contains("Also write steered.txt") {
                if tools_after_user == 0 {
                    ScriptedReply::ToolCall {
                        name: "terminal",
                        arguments: json!({
                            "description": "apply steered edit",
                            "timeout": 60,
                            "script": format!("printf 'steered-ok\\n' > '{}'", steered.display())
                        }),
                    }
                } else {
                    ScriptedReply::Text("Async + steer done.".to_string())
                }
            } else if user_text.contains("Run async task") {
                if tools_after_user == 0 {
                    entered.notify_one();
                    b.notified().await;
                    ScriptedReply::ToolCall {
                        name: "terminal",
                        arguments: json!({
                            "description": "initial async step",
                            "timeout": 60,
                            "script": format!(
                                "printf 'async-step\\n' > '{}'",
                                repo.join("async.txt").display()
                            )
                        }),
                    }
                } else {
                    ScriptedReply::Text("Initial async finished.".to_string())
                }
            } else {
                // Auxiliary background calls (suggestions, recaps, titles) return immediately.
                ScriptedReply::Text("ok".to_string())
            }
        })
    }))
    .await;

    let (_dir, state, fusion) = daemon_state_with(config_with_stub(&server.base_url)).await;
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);

    let parent_id = "fusion-e2e-parent";
    state
        .db()
        .create_session_in_profile(
            parent_id,
            Some("Fusion Parent"),
            Some(&format!("{PROVIDER_ID}::{LEAD_MODEL}")),
            Some(&state.default_profile_id),
        )
        .await
        .expect("create parent");
    state
        .db()
        .set_session_execution_mode(
            parent_id,
            &ExecutionMode::Fusion {
                sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            },
        )
        .await
        .expect("set fusion mode");

    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .create_session_dirs(
            parent_id,
            Some(&state.default_profile_id),
            crate::session::SandboxResourceLayout::Symlinked,
        )
        .await
        .expect("parent dirs");
    let parent_sandbox = store.sandbox_dir(parent_id, Some(&state.default_profile_id));
    let shared_repo = parent_sandbox.join("repos/demo");
    std::fs::create_dir_all(&shared_repo).expect("create shared repo dir");
    std::fs::write(shared_repo.join("README.md"), "baseline\npreexisting\n")
        .expect("seed dirty baseline");
    *shared_repo_holder.lock().await = Some(shared_repo.clone());

    // --- Step A: Blocking sidekick with incremental diff & surviving background job ---
    let outcome = fusion
        .execute(SidekickRequest {
            parent_session_id: parent_id.to_string(),
            sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            prompt: "Edit README.md, create hello.txt, and start background sleep".to_string(),
            async_run: false,
        })
        .await
        .expect("blocking sidekick #1");
    let SidekickOutcome::Completed { report, diff } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(report.contains("Blocking work done."), "report: {report}");
    assert!(
        diff.contains("+sidekick-edit"),
        "diff: {diff}\nrequests: {:#?}",
        server.requests.lock().await
    );
    assert!(diff.contains("+hello-v1"), "diff: {diff}");
    assert!(
        !diff.contains("+preexisting"),
        "pre-existing baseline line must not appear as added:\n{diff}"
    );

    let companion_id = state
        .db()
        .companion_session_for(parent_id, SIDEKICK_ROLE)
        .await
        .expect("lookup companion")
        .expect("companion linked");

    // Verify background job survived handoff inside the parked companion agent.
    {
        let parked = state
            .parked_agents
            .take(&companion_id)
            .await
            .expect("companion parked");
        let jobs = parked.shell().jobs().list().await;
        assert!(
            jobs.iter()
                .any(|job| matches!(job.status, aither_sandbox::JobStatus::Running)),
            "companion background job must survive handoff: {jobs:?}"
        );
        // Kill the sleep job before simulating restart so test leaves no orphan process.
        let _ = parked.kill_running_terminal_jobs().await;
    }

    // --- Step B: Simulated daemon restart recovery from history.db ---
    // Companion was evicted from parked_agents above; verify DB has transcript + snapshot.
    assert!(
        state
            .db()
            .get_session_context_snapshot(&companion_id)
            .await
            .expect("load snapshot")
            .is_some(),
        "companion PersistedContextSnapshot persisted to history.db"
    );
    let req_count_before_restart = server.requests.lock().await.len();
    let outcome2 = fusion
        .execute(SidekickRequest {
            parent_session_id: parent_id.to_string(),
            sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            prompt: "Update hello.txt to v2".to_string(),
            async_run: false,
        })
        .await
        .expect("blocking sidekick #2 after restart");
    let SidekickOutcome::Completed { report, diff } = outcome2 else {
        panic!("expected Completed after restart, got {outcome2:?}");
    };
    assert!(
        report.contains("Recovered after restart."),
        "report: {report}"
    );
    assert!(diff.contains("-hello-v1"), "diff: {diff}");
    assert!(diff.contains("+hello-v2"), "diff: {diff}");
    assert!(
        !diff.contains("+sidekick-edit"),
        "incremental diff must exclude earlier turn's changes:\n{diff}"
    );
    {
        let recovered =
            server.requests.lock().await[req_count_before_restart].contains("Blocking work done.");
        assert!(
            recovered,
            "rebuilt companion must recover prior conversation context from history.db"
        );
    }

    // --- Step C: Async sidekick, exclusive write lease, mid-run steer, hidden completion ---
    let started = fusion
        .execute(SidekickRequest {
            parent_session_id: parent_id.to_string(),
            sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            prompt: "Run async task".to_string(),
            async_run: true,
        })
        .await
        .expect("async sidekick starts");
    assert!(
        matches!(started, SidekickOutcome::StartedAsync),
        "expected StartedAsync, got {started:?}"
    );

    // Wait until the sidekick turn is actively in flight inside the scripted server.
    tokio::time::timeout(Duration::from_secs(10), async_entered.notified())
        .await
        .expect("async sidekick entered model call");

    let pair = fusion.pairs().for_parent(parent_id);
    let tasks_path = SessionPaths::new(parent_sandbox.clone()).tasks_path();
    let lease = pair.write_lease(&parent_sandbox, &tasks_path);
    assert!(
        lease.is_held(),
        "workspace write lease must be held while async sidekick runs"
    );

    // Exclusive write lease refuses lead workspace repo writes, allows reads/scratch/tasks.
    let policy = WorkspaceWriteLeasePolicy::new(lease.clone());
    let write_cmd = format!(
        "echo clobber > '{}'",
        shared_repo.join("README.md").display()
    );
    assert!(
        matches!(
            check_write_lease_script(&policy, &parent_sandbox, &write_cmd).await,
            ScriptDecision::Refuse(_)
        ),
        "lead workspace repo write must be refused while async sidekick holds lease"
    );
    let read_cmd = format!("cat '{}'", shared_repo.join("README.md").display());
    assert!(
        matches!(
            check_write_lease_script(&policy, &parent_sandbox, &read_cmd).await,
            ScriptDecision::Pass
        ),
        "read-only lead command must be allowed while async sidekick holds lease"
    );
    let scratch_cmd = format!(
        "echo note > '{}'",
        parent_sandbox.join("tmp/note.txt").display()
    );
    assert!(
        matches!(
            check_write_lease_script(&policy, &parent_sandbox, &scratch_cmd).await,
            ScriptDecision::Pass
        ),
        "lead <session>/tmp scratch write must be allowed while lease is held"
    );
    assert!(
        matches!(
            check_write_lease_script(&policy, &parent_sandbox, "tasks show").await,
            ScriptDecision::Pass
        ),
        "lead $MAY_TASKS command must be allowed while lease is held"
    );

    // Mid-run steer while async sidekick is running.
    let steered_outcome = fusion
        .execute(SidekickRequest {
            parent_session_id: parent_id.to_string(),
            sidekick_model: ModelSelector::new(PROVIDER_ID, SIDEKICK_MODEL),
            prompt: "Also write steered.txt".to_string(),
            async_run: true,
        })
        .await
        .expect("steer running sidekick");
    assert!(
        matches!(steered_outcome, SidekickOutcome::Steered),
        "expected Steered, got {steered_outcome:?}"
    );

    // Release barrier and wait for async turn + steer to finish and release lease.
    barrier.notify_one();
    eventually(
        Duration::from_secs(15),
        "async sidekick completes and releases lease",
        async || !lease.is_held(),
    )
    .await;

    // Hidden completion delivery landed on parent session's pending queue with report + diff.
    eventually(
        Duration::from_secs(5),
        "hidden sidekick completion queued on parent",
        async || {
            let summary = state
                .db()
                .load_session_runtime_summary(parent_id)
                .await
                .expect("runtime summary");
            summary
                .pending_immediate
                .iter()
                .chain(summary.queued_messages.iter())
                .any(|msg| {
                    msg.hidden_context_xml
                        .as_deref()
                        .is_some_and(|xml| xml.contains("sidekick_completed"))
                })
        },
    )
    .await;
    let summary = state
        .db()
        .load_session_runtime_summary(parent_id)
        .await
        .expect("runtime summary");
    let completion_xml = summary
        .pending_immediate
        .iter()
        .chain(summary.queued_messages.iter())
        .find_map(|msg| {
            msg.hidden_context_xml
                .as_deref()
                .filter(|xml| xml.contains("sidekick_completed"))
        })
        .expect("sidekick_completed hidden prompt");
    assert!(
        completion_xml.contains("Async + steer done."),
        "completion xml: {completion_xml}"
    );
    assert!(
        completion_xml.contains("+steered-ok"),
        "steered file diff missing from completion xml:\n{completion_xml}"
    );

    // --- Step D: Once-per-turn workspace-edit alert on direct lead edits ---
    let tracker =
        WorkspaceEditTracker::new(parent_sandbox.clone(), tasks_path, lease.clone(), true);
    let call_id = |name: &str| {
        serde_json::from_value::<aither_core::llm::ToolCallId>(json!(name)).expect("call id")
    };
    // Read-only lead terminal call produces zero alert.
    let c1 = call_id("lead-read");
    tracker.before_terminal(c1.clone()).await;
    assert_eq!(tracker.after_terminal(&c1).await, None);

    // First direct lead workspace edit emits the alert once.
    let c2 = call_id("lead-edit-1");
    tracker.before_terminal(c2.clone()).await;
    std::fs::write(shared_repo.join("README.md"), "direct edit 1\n").expect("direct edit 1");
    let alert1 = tracker.after_terminal(&c2).await;
    assert!(
        alert1
            .as_deref()
            .is_some_and(|text| text.contains("<fusion_workspace_edit_reminder>")),
        "first direct lead edit must emit alert: {alert1:?}"
    );

    // Second direct lead edit in the same turn does not repeat the alert.
    let c3 = call_id("lead-edit-2");
    tracker.before_terminal(c3.clone()).await;
    std::fs::write(shared_repo.join("README.md"), "direct edit 2\n").expect("direct edit 2");
    assert_eq!(tracker.after_terminal(&c3).await, None);

    // Calling sidekick resets the per-turn alert so a later direct edit alerts again.
    tracker.mark_sidekick_call();
    let c4 = call_id("lead-edit-3");
    tracker.before_terminal(c4.clone()).await;
    std::fs::write(shared_repo.join("README.md"), "direct edit 3\n").expect("direct edit 3");
    assert!(
        tracker.after_terminal(&c4).await.is_some(),
        "alert resets after sidekick call"
    );

    // --- Step E: Per-role usage & context rollups vs unchanged Standard mode ---
    let now = jiff::Timestamp::now().to_string();
    state
        .db()
        .append_request_log(RequestLogParams {
            session_id: parent_id,
            timestamp: &now,
            model: &format!("{PROVIDER_ID}::{LEAD_MODEL}"),
            provider_id: PROVIDER_ID,
            prompt_tokens: Some(1_000),
            completion_tokens: Some(200),
            total_tokens: Some(1_200),
            cache_read_tokens: Some(800),
            cache_write_tokens: Some(100),
            context_window: Some(128_000),
            elapsed_ms: 500,
            ok: true,
            error: None,
            keep_warm: false,
        })
        .await
        .expect("log lead request");
    state
        .db()
        .append_request_log(RequestLogParams {
            session_id: &companion_id,
            timestamp: &now,
            model: &format!("{PROVIDER_ID}::{SIDEKICK_MODEL}"),
            provider_id: PROVIDER_ID,
            prompt_tokens: Some(400),
            completion_tokens: Some(10),
            total_tokens: Some(410),
            cache_read_tokens: Some(350),
            cache_write_tokens: Some(0),
            context_window: Some(128_000),
            elapsed_ms: 100,
            ok: true,
            error: None,
            keep_warm: true,
        })
        .await
        .expect("log sidekick keep_warm request");

    let (status, usage) = call_api(
        &router,
        "GET",
        &format!("/api/sessions/{parent_id}/usage"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "usage body: {usage}");
    let roles = usage["roles"].as_array().expect("roles breakdown present");
    assert_eq!(roles.len(), 2, "lead + sidekick roles: {roles:?}");
    assert_eq!(roles[0]["role"].as_str(), Some("lead"));
    assert_eq!(roles[1]["role"].as_str(), Some("sidekick"));
    assert!(
        roles[1]["keepWarmRequests"].as_u64().unwrap_or(0) >= 1,
        "sidekick keep_warm requests counted: {roles:?}"
    );
    let lead_reqs = roles[0]["totalRequests"].as_u64().expect("lead reqs");
    let sidekick_reqs = roles[1]["totalRequests"].as_u64().expect("sidekick reqs");
    assert_eq!(
        usage["totalRequests"].as_u64(),
        Some(lead_reqs + sidekick_reqs),
        "session total aggregates lead + sidekick"
    );

    // Non-Fusion Standard session omits `roles` breakdown.
    let std_id = "standard-unchanged";
    state
        .db()
        .create_session_in_profile(
            std_id,
            Some("Standard"),
            Some(&format!("{PROVIDER_ID}::{LEAD_MODEL}")),
            Some(&state.default_profile_id),
        )
        .await
        .expect("create standard session");
    let (status, std_usage) = call_api(
        &router,
        "GET",
        &format!("/api/sessions/{std_id}/usage"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        std_usage.get("roles").is_none(),
        "standard mode omits roles breakdown: {std_usage}"
    );
}
