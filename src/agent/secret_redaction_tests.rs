//! Secret redaction on the agent's path, against a stub model server.
//!
//! The agent is the real aither agent and its model the real `OpenAI` client
//! behind [`RedactedModel`]; only the HTTP server on the other end is a stub.
//! The stub asks for the `leak` tool, which returns a stored secret's value
//! the way `echo $NAME` does, then answers; the requests it received are what
//! the provider would have seen.

use std::borrow::Cow;
use std::convert::Infallible;
use std::ops::ControlFlow;
use std::sync::Arc;

use aither_agent::{Agent, AgentEvent};
use aither_core::llm::{Tool, ToolResult};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpListener;

use super::hooks::SecretRedactionHook;
use super::provider::{ProviderLlm, RedactedModel};
use crate::secrets::SecretRedactor;

const MODEL: &str = "test-model";
const SECRET_NAME: &str = "DEPLOY_TOKEN";
const SECRET_VALUE: &str = "hunter2-correct-horse";

/// Prints the stored token.
#[derive(Deserialize, schemars::JsonSchema)]
struct NoArguments {}

/// A tool that prints the secret, as a terminal command echoing its
/// environment variable would.
struct LeakTool;

impl Tool for LeakTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("leak")
    }

    type Arguments = NoArguments;
    type Res = ToolResult;

    fn call(
        &self,
        _args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> impl std::future::Future<Output = aither_core::Result<Self::Res>> + Send {
        std::future::ready(Ok(ToolResult::text(format!("token={SECRET_VALUE}"))))
    }
}

/// Stub server: the first chat request calls `leak`, later ones answer.
async fn spawn_stub() -> (String, Arc<tokio::sync::Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub address");
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
    (format!("http://{addr}"), requests)
}

async fn answer(
    request: Request<Incoming>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() == Method::GET && request.uri().path().ends_with("/models") {
        let models = serde_json::json!({
            "data": [{"id": MODEL, "owned_by": "stub", "context_length": 128_000}]
        });
        return Ok(reply("application/json", models.to_string()));
    }
    assert_eq!(request.method(), Method::POST);
    let body = request
        .into_body()
        .collect()
        .await
        .expect("stub request body")
        .to_bytes();
    let payload: Value = serde_json::from_slice(&body).expect("stub request is JSON");
    let first = {
        let mut seen = requests.lock().await;
        seen.push(payload);
        seen.len() == 1
    };
    let chunk = |delta: &Value, finish: Value| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": delta, "finish_reason": finish}]})
        )
    };
    let stream = if first {
        [
            chunk(
                &serde_json::json!({"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "leak", "arguments": "{}"},
                }]}),
                Value::Null,
            ),
            chunk(&serde_json::json!({}), Value::from("tool_calls")),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat()
    } else {
        [
            chunk(&serde_json::json!({"content": "done"}), Value::Null),
            chunk(&serde_json::json!({}), Value::from("stop")),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat()
    };
    Ok(reply("text/event-stream", stream))
}

fn reply(content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn model(base_url: &str, redactor: SecretRedactor) -> ProviderLlm {
    RedactedModel::new(
        aither_cloud::CloudProvider::OpenAI(
            aither_cloud::openai::OpenAI::builder("test-key")
                .base_url(base_url)
                .model(MODEL)
                .no_retry()
                .build()
                .with_chat_completions_api(),
        ),
        redactor,
    )
}

/// Every request the stub received, as text.
async fn provider_traffic(requests: &tokio::sync::Mutex<Vec<Value>>) -> Vec<String> {
    requests.lock().await.iter().map(Value::to_string).collect()
}

fn assert_name_not_value(traffic: &[String]) {
    assert!(
        traffic.len() >= 2,
        "the tool result must reach the provider"
    );
    for request in traffic {
        assert!(
            !request.contains(SECRET_VALUE),
            "value reached provider: {request}"
        );
    }
    assert!(
        traffic
            .last()
            .is_some_and(|request| request.contains(&format!("token={SECRET_NAME}"))),
        "the tool result carries the name: {traffic:?}"
    );
}

/// The main agent: the tool result is stored, published and sent with the
/// name.
#[tokio::test]
async fn main_agent_tool_result_carries_the_name_not_the_value() {
    crate::test_support::install_global_executor();
    let (base_url, requests) = spawn_stub().await;
    let redactor = SecretRedactor::fixed(&[(SECRET_NAME, SECRET_VALUE)]);
    let mut agent = Agent::builder(model(&base_url, redactor.clone()))
        .tool(LeakTool)
        .hook(SecretRedactionHook::new(redactor))
        .build();

    let mut published = Vec::new();
    let _ = agent
        .run("print the token", std::iter::empty())
        .drive(
            &mut published,
            std::future::pending::<()>(),
            |published, timed| async move {
                if let AgentEvent::ToolCallEnd { result, .. } = &timed.event {
                    published.push(format!("{result:?}"));
                }
                ControlFlow::Continue(published)
            },
        )
        .await
        .expect("the run finishes");

    assert_eq!(published.len(), 1);
    assert!(
        published[0].contains(&format!("token={SECRET_NAME}")),
        "{published:?}"
    );
    assert!(!published[0].contains(SECRET_VALUE), "{published:?}");
    let history = format!("{:?}", agent.history());
    assert!(!history.contains(SECRET_VALUE), "stored context: {history}");
    assert_name_not_value(&provider_traffic(&requests).await);
}

/// An agent without the tool-result hook, as a subagent's inner agent is
/// built: its model still never sends the value.
#[tokio::test]
async fn subagent_request_carries_the_name_not_the_value() {
    crate::test_support::install_global_executor();
    let (base_url, requests) = spawn_stub().await;
    let redactor = SecretRedactor::fixed(&[(SECRET_NAME, SECRET_VALUE)]);
    let mut agent = Agent::builder(model(&base_url, redactor))
        .tool(LeakTool)
        .build();

    let _ = agent
        .run("print the token", std::iter::empty())
        .drive((), std::future::pending::<()>(), |(), _| async {
            ControlFlow::Continue(())
        })
        .await
        .expect("the run finishes");

    assert_name_not_value(&provider_traffic(&requests).await);
}
