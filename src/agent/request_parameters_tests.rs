//! What reaches the provider from an agent May builds: aither's own request
//! fields (Claude cache breakpoints, the structured output of a compaction,
//! the prompt-cache key) must survive the host's parameters.
//!
//! The agents come from May's own construction ([`MayAgent::session_config`]
//! and [`MayAgent::new`]); only the HTTP servers on the other end are stubs
//! that record the request bodies.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;

use super::idle_compaction_tests::{
    SESSION_MODEL, Session, SessionParts, StubModel, run_turn, saved_config,
};
use super::may::MayAgent;
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::model_selector::ModelSelector;

const CLAUDE_MODEL: &str = "claude-sonnet-4-5-20250929";
const CLAUDE_SELECTOR: &str = "claude::claude-sonnet-4-5-20250929";
/// Opening words of aither's compaction instruction.
const COMPACTION_MARKER: &str = "Your context window is being compacted";

/// Stub Anthropic server: a one-line text answer, request bodies recorded.
struct ClaudeStub {
    base_url: String,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

impl ClaudeStub {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub Anthropic server");
        let addr = listener.local_addr().expect("stub server address");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let served = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let served = served.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| claude_answer(request, served.clone()));
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

async fn claude_answer(
    request: Request<Incoming>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() != Method::POST || !request.uri().path().ends_with("/v1/messages") {
        let unexpected = format!("{} {}", request.method(), request.uri());
        return Ok(sse(StatusCode::NOT_FOUND, "text/plain", unexpected));
    }
    let body = request
        .into_body()
        .collect()
        .await
        .expect("stub request body")
        .to_bytes();
    requests
        .lock()
        .await
        .push(serde_json::from_slice(&body).expect("stub request is JSON"));
    let event = |name: &str, data: Value| format!("event: {name}\ndata: {data}\n\n");
    let stream = [
        event(
            "message_start",
            serde_json::json!({"message": {"id": "msg_1", "model": CLAUDE_MODEL,
                "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        ),
        event(
            "content_block_start",
            serde_json::json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        event(
            "content_block_delta",
            serde_json::json!({"index": 0,
                "delta": {"type": "text_delta", "text": "Hello there."}}),
        ),
        event("content_block_stop", serde_json::json!({"index": 0})),
        event(
            "message_delta",
            serde_json::json!({"delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 3}}),
        ),
        event("message_stop", serde_json::json!({"type": "message_stop"})),
    ]
    .concat();
    Ok(sse(StatusCode::OK, "text/event-stream", stream))
}

fn sse(status: StatusCode, content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn claude_config(stub: &ClaudeStub) -> MayConfig {
    let mut config = MayConfig::default();
    config.provider.insert(
        "claude".to_string(),
        ProviderInstance::new(ProviderType::Anthropic)
            .with_api_key("test-key")
            .with_base_url(stub.base_url.clone()),
    );
    config.models.advanced = vec![ModelSelector::new("claude", CLAUDE_MODEL)];
    config.last_model = Some(ModelSelector::new("claude", CLAUDE_MODEL));
    config
}

/// Request bodies a custom-provider stub received, parsed.
async fn bodies(stub: &StubModel) -> Vec<Value> {
    stub.requests
        .lock()
        .await
        .iter()
        .map(|text| serde_json::from_str(text).expect("stub request is JSON"))
        .collect()
}

/// A Claude turn carries aither's cache breakpoints: May's request
/// parameters must not replace them.
#[tokio::test]
async fn a_claude_turn_carries_cache_breakpoints() {
    let stub = ClaudeStub::spawn().await;
    let parts = SessionParts::new(tempfile::tempdir().expect("tempdir").keep()).await;
    let mut input = parts.input();
    input.model = CLAUDE_SELECTOR;
    let mut agent = Box::pin(MayAgent::from_config_with_model_and_permissions(
        &claude_config(&stub),
        input,
    ))
    .await
    .expect("the agent is built");

    run_turn(&mut agent).await;

    let bodies: Vec<String> = stub
        .requests
        .lock()
        .await
        .iter()
        .map(Value::to_string)
        .collect();
    assert_eq!(bodies.len(), 1, "one model request for the turn");
    let body = &bodies[0];
    assert!(
        body.contains(r#""cache_control":{"ttl":"1h","type":"ephemeral"}"#)
            || body.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#),
        "the Claude request carries one-hour cache breakpoints: {body}"
    );
}

/// A compaction request carries aither's structured output; a turn does not.
#[tokio::test]
async fn a_compaction_request_carries_structured_outputs() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start(&saved_config(&stub, None))).await;

    run_turn(&mut session.agent).await;
    session
        .agent
        .compact(None)
        .await
        .expect("/compact succeeds")
        .expect("there is a conversation to compact");

    let requests = bodies(&stub).await;
    let (compactions, turns): (Vec<_>, Vec<_>) = requests
        .iter()
        .partition(|body| body.to_string().contains(COMPACTION_MARKER));
    assert_eq!(compactions.len(), 1);
    assert_eq!(turns.len(), 1);
    assert_eq!(
        compactions[0]["response_format"]["type"], "json_object",
        "the compaction asks for structured output: {}",
        compactions[0]
    );
    assert!(
        turns[0].get("response_format").is_none(),
        "a turn asks for no schema: {}",
        turns[0]
    );
}

/// An agent rebuilt with the previous agent's cache epoch asks the provider
/// for the cache shard the first one warmed, and sets no key of May's own.
#[tokio::test]
async fn a_rebuilt_agent_continues_the_prompt_cache_key() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let config = saved_config(&stub, None);
    let parts = SessionParts::new(tempfile::tempdir().expect("tempdir").keep()).await;
    let mut input = parts.input();
    input.prompt_cache_epoch = 3;
    let mut agent = Box::pin(MayAgent::from_config_with_model_and_permissions(
        &config, input,
    ))
    .await
    .expect("the agent is built");

    run_turn(&mut agent).await;

    let requests = bodies(&stub).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["prompt_cache_key"],
        format!("{}:3", parts.session_id.as_str()),
        "the key continues the replaced agent's epoch: {}",
        requests[0]
    );
}
