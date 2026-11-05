//! The native `compact` tool, the pre-compaction alert and the exchanges kept
//! through compaction, in May sessions built from a saved config against a
//! scripted model server.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_lite::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::idle_compaction_tests::{SESSION_MODEL, Session};
use crate::config::ContextConfig;
use crate::model_selector::ModelSelector;

/// Opening words of aither's compaction instruction.
const COMPACTION_MARKER: &str = "Your context window is being compacted";
/// The task the scripted handoff records.
const HANDOFF_TASK: &str = "HANDOFF-TASK-OF-THE-SCRIPTED-COMPACTION";
/// Bound on how long a held compaction waits for a turn request; past it the
/// scripted server fails the test instead of hanging.
const COMPACTION_HOLD_LIMIT: Duration = Duration::from_secs(60);
/// How long the scripted model takes to answer a turn request.
const STEP_DELAY: Duration = Duration::from_millis(150);

/// What the scripted model answers to the nth turn request.
enum Step {
    Text(&'static str),
    Tool(&'static str, Value),
}

fn terminal(script: &'static str) -> Step {
    Step::Tool(
        "terminal",
        json!({
            "background": false,
            "description": "Take a step",
            "script": script,
            "mode": "sandboxed",
            "raw": true,
            "timeout": 30,
            "max_lines": 20
        }),
    )
}

#[derive(Default)]
struct Traffic {
    /// Every request body, in arrival order.
    requests: Vec<String>,
    turns: usize,
    compactions: usize,
    /// A compaction request is held unanswered.
    compaction_open: bool,
    /// Turn requests that arrived while a compaction was unanswered.
    served_during_compaction: usize,
}

/// A scripted `OpenAI`-compatible server: `script` answers turn requests by
/// their index. A server built with [`ScriptedModel::spawn_holding_compaction`]
/// holds each compaction reply until a turn request has been served.
struct ScriptedModel {
    base_url: String,
    traffic: Arc<tokio::sync::Mutex<Traffic>>,
}

/// Signalled by each turn request served while a compaction is held.
type CompactionGate = Option<Arc<tokio::sync::Notify>>;

impl ScriptedModel {
    async fn spawn(script: fn(usize) -> Step) -> Self {
        Self::start(script, None).await
    }

    async fn spawn_holding_compaction(script: fn(usize) -> Step) -> Self {
        Self::start(script, Some(Arc::new(tokio::sync::Notify::new()))).await
    }

    async fn start(script: fn(usize) -> Step, gate: CompactionGate) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted model server");
        let addr = listener.local_addr().expect("scripted model address");
        let traffic = Arc::new(tokio::sync::Mutex::new(Traffic::default()));
        let shared = traffic.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = shared.clone();
                let gate = gate.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        answer(request, script, shared.clone(), gate.clone())
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            traffic,
        }
    }

    async fn last_request(&self) -> String {
        self.traffic
            .lock()
            .await
            .requests
            .last()
            .expect("a request")
            .clone()
    }
}

async fn answer(
    request: Request<Incoming>,
    script: fn(usize) -> Step,
    traffic: Arc<tokio::sync::Mutex<Traffic>>,
    gate: CompactionGate,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() == Method::GET && request.uri().path().ends_with("/models") {
        let models = json!({
            "data": [{"id": SESSION_MODEL, "owned_by": "stub", "context_length": 128_000}]
        });
        return Ok(reply(
            StatusCode::OK,
            "application/json",
            models.to_string(),
        ));
    }
    let body = request
        .into_body()
        .collect()
        .await
        .expect("scripted model request body")
        .to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("request is UTF-8");
    let (delay, frames) = plan(&mut *traffic.lock().await, script, &text);
    tokio::time::sleep(delay).await;
    if text.contains(COMPACTION_MARKER) {
        if let Some(gate) = &gate {
            tokio::time::timeout(COMPACTION_HOLD_LIMIT, gate.notified())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "no turn request was served within {COMPACTION_HOLD_LIMIT:?} \
                         while the compaction was held"
                    )
                });
        }
        traffic.lock().await.compaction_open = false;
    } else {
        let held = {
            let mut traffic = traffic.lock().await;
            let held = traffic.compaction_open;
            traffic.served_during_compaction += usize::from(held);
            held
        };
        if let Some(gate) = gate.as_ref().filter(|_| held) {
            gate.notify_one();
        }
    }
    let stream = [frames, vec!["data: [DONE]\n\n".to_string()]]
        .concat()
        .concat();
    Ok(reply(StatusCode::OK, "text/event-stream", stream))
}

/// Records `text` and decides how long the server takes and what it sends.
fn plan(traffic: &mut Traffic, script: fn(usize) -> Step, text: &str) -> (Duration, Vec<String>) {
    traffic.requests.push(text.to_string());
    {
        if text.contains(COMPACTION_MARKER) {
            traffic.compactions += 1;
            traffic.compaction_open = true;
            let handoff = json!({
                "current_task": HANDOFF_TASK,
                "key_decisions": [],
                "context_files": [],
                "errors_fixes": [],
                "pending_work": [],
                "recovery_hints": [],
                "skills": [],
            });
            (
                Duration::ZERO,
                vec![
                    chunk(&json!({"content": handoff.to_string()}), None),
                    chunk(&json!({}), Some("stop")),
                ],
            )
        } else {
            let index = traffic.turns;
            traffic.turns += 1;
            let frames = match script(index) {
                Step::Text(text) => vec![
                    chunk(&json!({"content": text}), None),
                    chunk(&json!({}), Some("stop")),
                ],
                Step::Tool(name, arguments) => vec![chunk(
                    &json!({"tool_calls": [{
                        "index": 0,
                        "id": format!("call-{index}"),
                        "function": {"name": name, "arguments": arguments.to_string()}
                    }]}),
                    Some("tool_calls"),
                )],
            };
            (STEP_DELAY, frames)
        }
    }
}

fn chunk(delta: &Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": delta, "finish_reason": finish}]})
    )
}

fn reply(status: StatusCode, content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn config_for(model: &ScriptedModel, context: ContextConfig) -> crate::config::MayConfig {
    let mut config = crate::config::MayConfig::default();
    config.provider.insert(
        "session".to_string(),
        crate::config::ProviderInstance::new(crate::config::ProviderType::Custom)
            .with_api_key("test-key")
            .with_base_url(model.base_url.clone()),
    );
    config.models.advanced = vec![ModelSelector::new("session", SESSION_MODEL)];
    config.last_model = Some(ModelSelector::new("session", SESSION_MODEL));
    config.context = context;
    config
}

/// Runs `prompt` to completion on top of `filler_chars` of history.
async fn run(session: &mut Session, filler_chars: usize, prompt: &str) {
    session
        .agent
        .push_message(aither_core::llm::Message::assistant(
            "x".repeat(filler_chars),
        ));
    let mut stream = session.agent.run(prompt, []);
    while let Some(event) = stream.next().await {
        event.expect("the turn runs");
    }
}

/// Holds the run lease the way a served run does, so checkpoints persist.
async fn lease(session: &Session) -> impl Drop + use<> {
    let runtime = session.parts.runtime.clone().expect("a runtime");
    runtime
        .runs()
        .acquire(session.agent.session_id().as_str())
        .await
        .expect("the run lease is free")
}

fn compact_then_work(index: usize) -> Step {
    match index {
        0 => terminal("echo BEFORE-COMPACT"),
        1 => Step::Tool("compact", json!({})),
        2..=5 => terminal("echo AFTER-COMPACT"),
        _ => Step::Text("done"),
    }
}

/// The model calls `compact`, the run keeps asking the model while the
/// compaction is in flight, and the handoff replaces the conversation in the
/// agent and in the persisted context snapshot.
#[tokio::test]
async fn the_compact_tool_compacts_in_the_background_and_the_handoff_is_persisted() {
    let model = ScriptedModel::spawn_holding_compaction(compact_then_work).await;
    let mut session = Box::pin(Session::start_with_runtime(&config_for(
        &model,
        ContextConfig::default(),
    )))
    .await;
    let _lease = lease(&session).await;

    run(&mut session, 8 * 20_000, "Start the work.").await;

    let (compactions, served_during) = {
        let traffic = model.traffic.lock().await;
        (traffic.compactions, traffic.served_during_compaction)
    };
    assert_eq!(compactions, 1, "the call starts one compaction");
    assert!(
        served_during >= 1,
        "the run keeps asking the model while the compaction is in flight"
    );
    let session_id = session.agent.session_id().as_str().to_string();
    let snapshot = crate::session::load_persisted_context_snapshot(&session.parts.db, &session_id)
        .await
        .expect("the snapshot loads")
        .expect("the session persisted a snapshot");
    let persisted = serde_json::to_string(&snapshot.checkpoint).expect("checkpoint serializes");
    assert!(
        persisted.contains(HANDOFF_TASK),
        "the handoff is in the persisted snapshot: {persisted}"
    );

    run(&mut session, 0, "Continue.").await;
    let resumed = model.last_request().await;
    assert!(
        resumed.contains(HANDOFF_TASK),
        "the agent runs on the handoff"
    );
    assert!(
        !resumed.contains(&"x".repeat(1_000)),
        "the conversation before the handoff is gone"
    );
}

fn say_done(_: usize) -> Step {
    Step::Text("done")
}

/// Usage that comes within the warning distance of the auto-compact point
/// raises one alert, however many requests follow.
#[tokio::test]
async fn crossing_the_warning_distance_raises_one_alert_in_a_session() {
    let model = ScriptedModel::spawn(say_done).await;
    // Auto-compact point 40k tokens, warning at 30k, background fork at 38k.
    let context = ContextConfig {
        compact_at: 0.8,
        compact_ceiling: 40_000,
        start_distance: 2_000,
        safe_buffer: 1_000,
        warn_distance: 10_000,
    };
    let mut session = Box::pin(Session::start(&config_for(&model, context))).await;

    run(&mut session, 4 * 29_000, "First.").await;
    run(&mut session, 0, "Second.").await;
    run(&mut session, 0, "Third.").await;

    let last = model.last_request().await;
    assert_eq!(
        last.matches("from automatic compaction").count(),
        1,
        "one alert in the window: {last}"
    );
    assert_eq!(
        model.traffic.lock().await.compactions,
        0,
        "the alert only asks"
    );
}

fn ask_then_work(index: usize) -> Step {
    match index {
        0 => terminal("ask_user --question 'Which cache layer?'"),
        1 => terminal("echo PLAIN-STEP-MARKER"),
        _ => Step::Text("done"),
    }
}

/// The `ask_user` exchange stays verbatim in the window after a compaction,
/// while an ordinary command's exchange is summarised away.
#[tokio::test]
async fn an_ask_user_exchange_survives_a_compaction() {
    let model = ScriptedModel::spawn(ask_then_work).await;
    let mut session = Box::pin(Session::start_with_runtime(&config_for(
        &model,
        ContextConfig::default(),
    )))
    .await;
    let _lease = lease(&session).await;

    run(&mut session, 8 * 20_000, "Ask and work.").await;
    session
        .agent
        .compact(None)
        .await
        .expect("the session compacts");
    run(&mut session, 0, "Continue.").await;

    let resumed = model.last_request().await;
    assert!(resumed.contains(HANDOFF_TASK), "the compaction landed");
    assert!(
        resumed.contains("ask_user --question"),
        "the ask_user call is kept: {resumed}"
    );
    assert!(
        !resumed.contains("PLAIN-STEP-MARKER"),
        "an ordinary exchange is not kept"
    );
}
