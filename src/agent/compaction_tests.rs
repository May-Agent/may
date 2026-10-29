//! Compaction through [`MayAgent`] against a stub model server.
//!
//! The agent is the real aither agent behind May's wrapper and its model is
//! the real `OpenAI` client; only the HTTP server on the other end is a stub.
//! The stub plays the summarising model: it writes a handoff that carries the
//! constraint only when the compaction request it received still shows that
//! constraint, so a handoff can never "remember" what the request dropped.

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

use super::may::test_agent;
use super::provider::RequestParameterizedModel;

/// A constraint the user states once, before the first compaction, and never
/// repeats.
const CONSTRAINT: &str = "vendor-freeze-7731";
/// Model id the stub serves.
const MODEL: &str = "test-model";
/// Opening words of aither's compaction instruction; they identify a
/// compaction request among the model traffic.
const COMPACTION_MARKER: &str = "Your context window is being compacted";

/// Stub `OpenAI` server that answers compaction requests with a handoff.
struct StubModel {
    base_url: String,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

impl StubModel {
    async fn spawn() -> Self {
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
                    let service = service_fn(move |request| answer(request, served.clone()));
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

    /// Chat-completion bodies received so far, oldest first.
    async fn requests(&self) -> Vec<Value> {
        self.requests.lock().await.clone()
    }
}

async fn answer(
    request: Request<Incoming>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() == Method::GET && request.uri().path().ends_with("/models") {
        let models = serde_json::json!({
            "data": [{"id": MODEL, "owned_by": "stub", "context_length": 128_000}]
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
    let payload: Value = serde_json::from_slice(&body).expect("stub model request is JSON");
    let text = request_text(&payload);
    requests.lock().await.push(payload);

    assert!(
        text.contains(COMPACTION_MARKER),
        "only compaction traffic is expected, got: {text}"
    );
    let handoff = serde_json::json!({
        "current_task": "Port the parser to the new lexer.",
        "key_decisions": if text.contains(CONSTRAINT) {
            vec![format!("Never edit files under vendor/ ({CONSTRAINT}).")]
        } else {
            Vec::new()
        },
        "errors_fixes": [],
        "pending_work": ["Continue porting."],
        "recovery_hints": [],
    });
    let chunk = |delta: &Value, finish: Value| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": delta, "finish_reason": finish}]})
        )
    };
    let stream = [
        chunk(
            &serde_json::json!({"content": handoff.to_string()}),
            Value::Null,
        ),
        chunk(&serde_json::json!({}), Value::from("stop")),
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

/// Text of one chat message, whether the content is a string or a list of
/// parts.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Text of every message of a request.
fn request_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .expect("chat request carries messages")
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Roles of the messages of `request` whose text contains `needle`.
fn roles_mentioning(request: &Value, needle: &str) -> Vec<String> {
    request["messages"]
        .as_array()
        .expect("chat request carries messages")
        .iter()
        .filter(|message| message_text(message).contains(needle))
        .map(|message| message["role"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A session compacted twice keeps a constraint that was stated only before
/// the first compaction: the second compaction request still carries the
/// first handoff, so the second handoff can name the constraint again.
#[tokio::test]
async fn second_handoff_keeps_a_constraint_stated_before_the_first_compaction() {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
    let stub = StubModel::spawn().await;
    let llm = RequestParameterizedModel::new(
        aither_cloud::CloudProvider::OpenAI(
            aither_cloud::openai::OpenAI::builder("test-key")
                .base_url(stub.base_url.clone())
                .model(MODEL)
                .no_retry()
                .build()
                .with_chat_completions_api(),
        ),
        aither_core::llm::model::Parameters::default(),
    );
    let (mut agent, _temp) = test_agent(llm, "openai", MODEL).await;

    agent.push_message(aither_core::llm::Message::user(format!(
        "Port the parser to the new lexer. Constraint for the whole job: never edit files under vendor/ ({CONSTRAINT})."
    )));
    agent.push_message(aither_core::llm::Message::assistant(
        "Understood, starting with the token definitions.",
    ));
    let first = agent
        .compact(None)
        .await
        .expect("first compaction succeeds")
        .expect("first compaction has a conversation to compact");
    assert!(
        first.summary.contains(CONSTRAINT),
        "first handoff must carry the constraint it was shown: {}",
        first.summary
    );

    // Work continues without ever restating the constraint.
    agent.push_message(aither_core::llm::Message::user("Now port the lexer."));
    agent.push_message(aither_core::llm::Message::assistant("The lexer is ported."));
    let second = agent
        .compact(None)
        .await
        .expect("second compaction succeeds")
        .expect("second compaction has a conversation to compact");
    assert!(
        second.summary.contains(CONSTRAINT),
        "second handoff lost the constraint: {}",
        second.summary
    );

    let requests = stub.requests().await;
    assert_eq!(requests.len(), 2, "one model request per compaction");
    assert_eq!(
        roles_mentioning(&requests[0], CONSTRAINT),
        ["user"],
        "the first request sees the constraint in the message that stated it"
    );
    assert_eq!(
        roles_mentioning(&requests[1], CONSTRAINT),
        ["system"],
        "the second request sees the constraint only through the first handoff"
    );

    let checkpoint = agent
        .export_checkpoint()
        .await
        .expect("checkpoint exports after compaction");
    let handoff = checkpoint
        .context
        .handoff()
        .expect("the compacted session holds a handoff")
        .render();
    assert!(handoff.contains(CONSTRAINT), "stored handoff: {handoff}");
}
