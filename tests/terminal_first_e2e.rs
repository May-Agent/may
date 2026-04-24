use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use may_agent::MayConfig;
use may_agent::config::{ProviderInstance, ProviderType};
use may_agent::model_selector::ModelSelector;
use may_agent::session::{SandboxResourceLayout, SessionId, initialize_session_persistence};
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};

const TEST_PROVIDER_ID: &str = "mock";
const TEST_MODEL: &str = "test-model";
const TEST_SELECTOR: &str = "mock::test-model";
const TEST_API_KEY: &str = "test-api-key";

#[derive(Clone)]
enum MockChatScenario {
    SlowText {
        chunks: Vec<String>,
        delay: Duration,
    },
    TerminalRoundTrip,
}

struct MockOpenAiServer {
    addr: SocketAddr,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockOpenAiServer {
    async fn spawn(scenario: MockChatScenario) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock OpenAI listener should bind");
        let addr = listener
            .local_addr()
            .expect("mock OpenAI listener should expose local addr");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let state = Arc::new(tokio::sync::Mutex::new(MockScenarioState::new(scenario)));
        let requests_for_task = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let io = TokioIo::new(stream);
                let state = state.clone();
                let requests = requests_for_task.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        handle_mock_openai_request(request, state.clone(), requests.clone())
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });
        Self {
            addr,
            requests,
            handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }
}

impl Drop for MockOpenAiServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct MockScenarioState {
    scenario: MockChatScenario,
    request_index: usize,
    task_id: Option<String>,
}

impl MockScenarioState {
    const fn new(scenario: MockChatScenario) -> Self {
        Self {
            scenario,
            request_index: 0,
            task_id: None,
        }
    }

    fn next_response(&mut self, request: &Value) -> Result<MockHttpResponse, String> {
        let response = match &self.scenario {
            MockChatScenario::SlowText { chunks, delay } => {
                if self.request_index != 0 {
                    return Err(format!(
                        "unexpected extra request for slow-text scenario: {}",
                        self.request_index
                    ));
                }
                MockHttpResponse::sse(text_stream_frames(chunks.clone(), *delay))
            }
            MockChatScenario::TerminalRoundTrip => {
                let response = match self.request_index {
                    0 => MockHttpResponse::sse(vec![
                        delayed_sse_frame(
                            tool_call_chunk(
                                "call-terminal",
                                "terminal",
                                &serde_json::json!({
                                    "script": "printf 'name? '; read name; printf 'hello %s\\n' \"$name\"",
                                    "mode": "sandboxed",
                                    "timeout": 0,
                                    "max_lines": 50
                                }),
                            ),
                            Duration::from_millis(10),
                        ),
                        done_frame(Duration::from_millis(10)),
                    ]),
                    1 => {
                        let task_id = terminal_task_id_from_latest_tool_output(request)
                            .ok_or_else(|| {
                                "terminal tool result did not include a task_id".to_string()
                            })?;
                        self.task_id = Some(task_id.clone());
                        MockHttpResponse::sse(vec![
                            delayed_sse_frame(
                                tool_call_chunk(
                                    "call-terminal-input",
                                    "terminal_input",
                                    &serde_json::json!({
                                        "task_id": task_id,
                                        "input": "lexo",
                                        "append_newline": true
                                    }),
                                ),
                                Duration::from_millis(350),
                            ),
                            done_frame(Duration::from_millis(10)),
                        ])
                    }
                    2 => {
                        let task_id = self
                            .task_id
                            .clone()
                            .ok_or_else(|| "terminal task_id was not captured".to_string())?;
                        MockHttpResponse::sse(vec![
                            delayed_sse_frame(
                                tool_call_chunk(
                                    "call-terminal-read",
                                    "terminal_read",
                                    &serde_json::json!({
                                        "task_id": task_id,
                                        "cursor": 0,
                                        "max_bytes": 4096
                                    }),
                                ),
                                Duration::from_millis(250),
                            ),
                            done_frame(Duration::from_millis(10)),
                        ])
                    }
                    3 => {
                        let tool_output = latest_tool_output(request)
                            .ok_or_else(|| "terminal_read tool result missing".to_string())?;
                        let text = tool_output
                            .get("delta")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                "terminal_read tool result missing textual delta".to_string()
                            })?;
                        if !text.contains("hello lexo") {
                            return Err(format!(
                                "terminal_read output did not contain terminal echo: {text}"
                            ));
                        }
                        MockHttpResponse::sse(vec![
                            delayed_sse_frame(
                                text_chunk("terminal flow complete", None),
                                Duration::from_millis(10),
                            ),
                            delayed_sse_frame(
                                text_chunk("", Some("stop")),
                                Duration::from_millis(10),
                            ),
                            done_frame(Duration::from_millis(10)),
                        ])
                    }
                    other => {
                        return Err(format!(
                            "unexpected request index {other} for terminal round trip"
                        ));
                    }
                };
                response
            }
        };
        self.request_index += 1;
        Ok(response)
    }
}

enum MockHttpResponse {
    Sse(Vec<MockSseFrame>),
    Json(StatusCode, Value),
}

impl MockHttpResponse {
    const fn sse(frames: Vec<MockSseFrame>) -> Self {
        Self::Sse(frames)
    }
}

struct MockSseFrame {
    delay: Duration,
    data: String,
}

fn delayed_sse_frame(data: String, delay: Duration) -> MockSseFrame {
    MockSseFrame { delay, data }
}

fn done_frame(delay: Duration) -> MockSseFrame {
    delayed_sse_frame("data: [DONE]\n\n".to_string(), delay)
}

fn text_stream_frames(chunks: Vec<String>, delay: Duration) -> Vec<MockSseFrame> {
    let mut frames = Vec::with_capacity(chunks.len() + 2);
    for chunk in chunks {
        frames.push(delayed_sse_frame(text_chunk(&chunk, None), delay));
    }
    frames.push(delayed_sse_frame(text_chunk("", Some("stop")), delay));
    frames.push(done_frame(delay));
    frames
}

fn text_chunk(content: &str, finish_reason: Option<&str>) -> String {
    let chunk = serde_json::json!({
        "choices": [{
            "delta": {
                "content": content
            },
            "finish_reason": finish_reason
        }]
    });
    format!("data: {chunk}\n\n")
}

fn tool_call_chunk(call_id: &str, name: &str, arguments: &Value) -> String {
    let chunk = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {chunk}\n\n")
}

async fn handle_mock_openai_request(
    request: Request<Incoming>,
    state: Arc<tokio::sync::Mutex<MockScenarioState>>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
) -> Result<Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>, Infallible> {
    if request.method() == Method::GET && request.uri().path() == "/v1/models" {
        return Ok(json_response(
            StatusCode::OK,
            serde_json::json!({
                "data": [{
                    "id": TEST_MODEL,
                    "owned_by": "mock",
                    "context_length": 128000
                }]
            }),
        ));
    }

    if request.method() != Method::POST || request.uri().path() != "/v1/chat/completions" {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "error": "not found" }),
        ));
    }

    let body = request
        .into_body()
        .collect()
        .await
        .expect("mock request body should collect")
        .to_bytes();
    let payload: Value =
        serde_json::from_slice(&body).expect("mock OpenAI request should be valid JSON");
    requests.lock().await.push(payload.clone());

    let response = {
        let mut state = state.lock().await;
        match state.next_response(&payload) {
            Ok(response) => response,
            Err(error) => {
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": { "message": error } }),
                ));
            }
        }
    };

    Ok(match response {
        MockHttpResponse::Sse(frames) => sse_response(frames),
        MockHttpResponse::Json(status, value) => json_response(status, value),
    })
}

fn json_response(
    status: StatusCode,
    value: Value,
) -> Response<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(Full::new(Bytes::from(value.to_string())).boxed());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn sse_response(
    frames: Vec<MockSseFrame>,
) -> Response<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    let stream = async_stream::stream! {
        for frame in frames {
            sleep(frame.delay).await;
            yield Ok::<Frame<Bytes>, Infallible>(Frame::data(Bytes::from(frame.data)));
        }
    };
    let body = StreamBody::new(stream).boxed();
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
}

struct TestHarness {
    home: TempDir,
    may_dir: PathBuf,
}

impl TestHarness {
    async fn new(config_base_url: &str) -> Self {
        let home = tempfile::tempdir().expect("test home tempdir should be created");
        let may_dir = home.path().join(".may");
        tokio::fs::create_dir_all(&may_dir)
            .await
            .expect("test may dir should be created");

        let mut config = MayConfig::default();
        config.provider.insert(
            TEST_PROVIDER_ID.to_string(),
            ProviderInstance::new(ProviderType::Custom)
                .with_api_key(TEST_API_KEY)
                .with_base_url(config_base_url),
        );
        config.models.advanced = vec![ModelSelector::new(TEST_PROVIDER_ID, TEST_MODEL)];
        config.last_model = Some(ModelSelector::new(TEST_PROVIDER_ID, TEST_MODEL));
        config
            .save_to(&may_dir.join("config.toml"))
            .await
            .expect("test config should be written");

        Self { home, may_dir }
    }

    async fn create_session(&self) -> TestSession {
        let (store, db, default_profile_id) = initialize_session_persistence(&self.may_dir)
            .await
            .expect("session persistence should initialize");
        let session_id = SessionId::new().to_string();
        store
            .create_session_dirs(
                &session_id,
                Some(default_profile_id.as_str()),
                SandboxResourceLayout::Symlinked,
            )
            .await
            .expect("session directory layout should be created");
        db.create_session_in_profile(
            &session_id,
            None,
            Some(TEST_SELECTOR),
            Some(default_profile_id.as_str()),
        )
        .await
        .expect("session row should be created");
        let sandbox_dir = store.sandbox_dir(&session_id, Some(default_profile_id.as_str()));
        TestSession {
            id: session_id,
            profile_id: default_profile_id,
            sandbox_dir,
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }
}

struct TestSession {
    id: String,
    profile_id: String,
    sandbox_dir: PathBuf,
}

struct RunningUiServer {
    child: Child,
    base_url: String,
    log_path: PathBuf,
}

impl RunningUiServer {
    async fn spawn(home: &Path, extra_env: &[(&str, &str)]) -> Self {
        let port = reserve_tcp_port();
        let binary = may_binary_path();
        let log_path = home.join("ui-server.log");
        let log_file = std::fs::File::create(&log_path).expect("ui server log file should open");
        let log_file_err = log_file
            .try_clone()
            .expect("ui server log file clone should open");
        let mut command = Command::new(binary);
        command
            .arg("rest")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .current_dir(home)
            .env("HOME", home)
            .stdin(Stdio::null())
            .stdout(StdStdio::from(log_file))
            .stderr(StdStdio::from(log_file_err));
        if let Some(heel_bin) = optional_heel_bin() {
            command.env("HEEL_BIN", heel_bin);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("may rest should spawn");
        let base_url = format!("http://127.0.0.1:{port}");
        wait_for_http_ok(&format!("{base_url}/api/health"), Duration::from_secs(15)).await;
        Self {
            child,
            base_url,
            log_path,
        }
    }

    async fn logs(&self) -> String {
        tokio::fs::read_to_string(&self.log_path)
            .await
            .unwrap_or_else(|error| {
                format!(
                    "failed to read ui server logs at {}: {error}",
                    self.log_path.display()
                )
            })
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

fn may_binary_path() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_may") {
        return PathBuf::from(path);
    }

    let current_exe = std::env::current_exe().expect("current test executable path should exist");
    let target_dir = current_exe
        .parent()
        .and_then(Path::parent)
        .expect("integration test executable should live in target/debug/deps");
    let binary_name = if cfg!(windows) { "may.exe" } else { "may" };
    let candidate = target_dir.join(binary_name);
    assert!(
        candidate.exists(),
        "May binary should exist at {}",
        candidate.display()
    );
    candidate
}

fn optional_heel_bin() -> Option<String> {
    if let Ok(path) = std::env::var("HEEL_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Some(candidate.display().to_string());
        }
    }

    let known_paths = [
        "/tmp/heel-release/heel-aarch64-apple-darwin",
        "/Users/lexoliu/.may/container/heel/bin/heel",
    ];
    known_paths.iter().find_map(|path| {
        let candidate = PathBuf::from(path);
        candidate.exists().then(|| candidate.display().to_string())
    })
}

fn required_heel_bin() -> String {
    optional_heel_bin().expect("prebuilt heel binary must be available for terminal E2E tests")
}

impl Drop for RunningUiServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct SseReader {
    response: reqwest::Response,
    buffer: String,
}

impl SseReader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            buffer: String::new(),
        }
    }

    async fn next_event(&mut self) -> Option<TestSseEvent> {
        loop {
            if let Some((event, remainder)) = split_sse_block(&self.buffer) {
                self.buffer = remainder;
                if event.event.is_empty() && event.data.is_empty() {
                    continue;
                }
                return Some(event);
            }

            let chunk = self
                .response
                .chunk()
                .await
                .expect("SSE response chunk should be readable");
            match chunk {
                Some(bytes) => self.buffer.push_str(&String::from_utf8_lossy(&bytes)),
                None => {
                    if self.buffer.trim().is_empty() {
                        return None;
                    }
                    let event = parse_sse_block(self.buffer.as_str());
                    self.buffer.clear();
                    return Some(event);
                }
            }
        }
    }
}

struct TestSseEvent {
    event: String,
    data: String,
}

fn split_sse_block(buffer: &str) -> Option<(TestSseEvent, String)> {
    let delimiter = if let Some(position) = buffer.find("\n\n") {
        (position, 2)
    } else if let Some(position) = buffer.find("\r\n\r\n") {
        (position, 4)
    } else {
        return None;
    };
    let (position, width) = delimiter;
    let event = parse_sse_block(&buffer[..position]);
    Some((event, buffer[position + width..].to_string()))
}

fn parse_sse_block(block: &str) -> TestSseEvent {
    let mut event = String::new();
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("event: ") {
            event = value.to_string();
        } else if let Some(value) = line.strip_prefix("data: ") {
            data_lines.push(value.to_string());
        }
    }
    TestSseEvent {
        event,
        data: data_lines.join("\n"),
    }
}

fn reserve_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port bind should succeed")
        .local_addr()
        .expect("ephemeral listener should expose local addr")
        .port()
}

async fn wait_for_http_ok(url: &str, timeout_duration: Duration) {
    let client = reqwest::Client::new();
    wait_for_condition(timeout_duration, || {
        let client = client.clone();
        let url = url.to_string();
        async move {
            match client.get(url).send().await {
                Ok(response) => response.status().is_success(),
                Err(_) => false,
            }
        }
    })
    .await;
}

async fn wait_for_condition<F, Fut>(timeout_duration: Duration, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout_duration;
    loop {
        if predicate().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition was not satisfied within {:?}",
            timeout_duration
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_runtime_state_clear(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
    server: &RunningUiServer,
    timeout_duration: Duration,
) {
    let deadline = Instant::now() + timeout_duration;
    loop {
        let last_runtime = get_runtime(client, base_url, session_id).await;
        if last_runtime.get("state").is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "runtime state did not clear within {:?}; runtime: {}; server logs:\n{}",
            timeout_duration,
            last_runtime,
            server.logs().await
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_runtime_condition<F>(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
    server: &RunningUiServer,
    timeout_duration: Duration,
    description: &str,
    mut predicate: F,
) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + timeout_duration;
    loop {
        let runtime = get_runtime(client, base_url, session_id).await;
        if predicate(&runtime) {
            return runtime;
        }
        assert!(
            Instant::now() < deadline,
            "runtime condition `{description}` was not satisfied within {:?}; runtime: {}; server logs:\n{}",
            timeout_duration,
            runtime,
            server.logs().await
        );
        sleep(Duration::from_millis(50)).await;
    }
}

fn latest_tool_output(request: &Value) -> Option<Value> {
    let messages = request.get("messages")?.as_array()?;
    let content = messages.iter().rev().find_map(|message| {
        let role = message.get("role")?.as_str()?;
        if role != "tool" {
            return None;
        }
        message.get("content")?.as_str().map(ToOwned::to_owned)
    })?;
    serde_json::from_str(&content).ok()
}

fn terminal_task_id_from_latest_tool_output(request: &Value) -> Option<String> {
    latest_tool_output(request)?
        .get("task_id")?
        .as_str()
        .map(ToOwned::to_owned)
}

async fn create_session_via_api(client: &reqwest::Client, base_url: &str) -> String {
    let response = client
        .post(format!("{base_url}/api/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("create session request should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response
        .json()
        .await
        .expect("create session response should be valid JSON");
    value
        .get("id")
        .and_then(Value::as_str)
        .expect("create session response should include id")
        .to_string()
}

async fn get_runtime(client: &reqwest::Client, base_url: &str, session_id: &str) -> Value {
    let response = client
        .get(format!("{base_url}/api/sessions/{session_id}/runtime"))
        .send()
        .await
        .expect("runtime request should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    response
        .json()
        .await
        .expect("runtime response should be valid JSON")
}

fn extract_assistant_messages(session_payload: &Value) -> Vec<String> {
    session_payload
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| {
            let role = message.get("role")?.as_str()?;
            if role != "assistant" {
                return None;
            }
            message.get("content")?.as_str().map(ToOwned::to_owned)
        })
        .collect()
}

fn extract_stored_output_path(text: &str) -> Option<PathBuf> {
    let marker = "absolute path: ";
    let start = text.find(marker)?;
    let remainder = &text[start + marker.len()..];
    let end = remainder.find("; output was truncated in-chat]")?;
    Some(PathBuf::from(&remainder[..end]))
}

#[tokio::test]
#[serial]
async fn exec_uses_session_directory_and_exposes_ipc_command_help() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let session = harness.create_session().await;
    let binary = may_binary_path();
    let heel_bin = required_heel_bin();

    let pwd_output = Command::new(&binary)
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("sandboxed")
        .arg("--")
        .arg("pwd")
        .env("HOME", harness.home_path())
        .env("HEEL_BIN", &heel_bin)
        .output()
        .await
        .expect("may exec pwd should run");
    assert!(
        pwd_output.status.success(),
        "pwd command failed: {}",
        String::from_utf8_lossy(&pwd_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&pwd_output.stdout).trim(),
        "/workspace"
    );

    let layout_output = Command::new(&binary)
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("sandboxed")
        .arg("--")
        .arg(
            "for d in tmp data files outputs artifacts; do test -d \"$d\" || exit 10; done; test -e skills; test -e subagents; session --help >/dev/null; ask --help >/dev/null; search --help >/dev/null; webfetch --help >/dev/null; websearch --help >/dev/null; task --help >/dev/null; notify --help >/dev/null; printf ready",
        )
        .env("HOME", harness.home_path())
        .env("HEEL_BIN", &heel_bin)
        .output()
        .await
        .expect("may exec layout/help probe should run");
    assert!(
        layout_output.status.success(),
        "layout/help probe failed: {}",
        String::from_utf8_lossy(&layout_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&layout_output.stdout).trim(),
        "ready"
    );
}

#[tokio::test]
#[serial]
async fn exec_large_output_is_previewed_and_saved_to_outputs_file() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let session = harness.create_session().await;
    let binary = may_binary_path();
    let heel_bin = required_heel_bin();

    let output = Command::new(binary)
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("sandboxed")
        .arg("--")
        .arg("perl -e 'print \"x\" x 6000'")
        .env("HOME", harness.home_path())
        .env("HEEL_BIN", &heel_bin)
        .output()
        .await
        .expect("may exec large output probe should run");

    assert!(
        output.status.success(),
        "large output probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[full content at outputs/"),
        "expected preview output with stored file reference, got: {stdout}"
    );
    let stored_path = extract_stored_output_path(&stdout)
        .expect("preview output should include absolute stored path");
    assert!(
        stored_path.starts_with(session.sandbox_dir.join("outputs")),
        "stored output path should live in session outputs dir: {}",
        stored_path.display()
    );
    let stored_contents = tokio::fs::read_to_string(&stored_path)
        .await
        .expect("stored output file should be readable");
    assert_eq!(stored_contents.len(), 6000);
    assert!(stored_contents.chars().all(|ch| ch == 'x'));
}

#[tokio::test]
#[serial]
async fn ui_session_crud_round_trip_matches_api_contract() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();

    let create_response = client
        .post(format!("{}/api/sessions", server.base_url))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("create session request should succeed");
    assert_eq!(create_response.status(), StatusCode::OK);
    let created: Value = create_response
        .json()
        .await
        .expect("create session response should be valid JSON");
    let session_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("created session should include id")
        .to_string();
    assert_eq!(
        created.get("model").and_then(Value::as_str),
        Some(TEST_SELECTOR)
    );

    let list_response = client
        .get(format!("{}/api/sessions", server.base_url))
        .send()
        .await
        .expect("list sessions request should succeed");
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed: Value = list_response
        .json()
        .await
        .expect("list sessions response should be valid JSON");
    let ids = listed
        .as_array()
        .expect("list sessions response should be an array")
        .iter()
        .filter_map(|session| session.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(ids.contains(&session_id.as_str()));

    let update_response = client
        .put(format!("{}/api/sessions/{session_id}", server.base_url))
        .json(&serde_json::json!({
            "title": "Terminal first test",
            "model": TEST_SELECTOR
        }))
        .send()
        .await
        .expect("update session request should succeed");
    assert_eq!(update_response.status(), StatusCode::OK);
    let updated: Value = update_response
        .json()
        .await
        .expect("update session response should be valid JSON");
    assert_eq!(
        updated.get("title").and_then(Value::as_str),
        Some("Terminal first test")
    );

    let get_response = client
        .get(format!("{}/api/sessions/{session_id}", server.base_url))
        .send()
        .await
        .expect("get session request should succeed");
    assert_eq!(get_response.status(), StatusCode::OK);
    let fetched: Value = get_response
        .json()
        .await
        .expect("get session response should be valid JSON");
    assert_eq!(
        fetched.get("title").and_then(Value::as_str),
        Some("Terminal first test")
    );
    assert_eq!(
        fetched
            .get("runtime")
            .and_then(Value::as_object)
            .is_none_or(|runtime| runtime.is_empty()),
        true
    );

    let delete_response = client
        .delete(format!("{}/api/sessions/{session_id}", server.base_url))
        .send()
        .await
        .expect("delete session request should succeed");
    assert_eq!(delete_response.status(), StatusCode::OK);

    let missing_response = client
        .get(format!("{}/api/sessions/{session_id}", server.base_url))
        .send()
        .await
        .expect("missing session request should succeed");
    assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);

    server.stop().await;
}

#[tokio::test]
#[serial]
async fn ui_stream_stop_mid_response_persists_partial_assistant_text() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec![
            "alpha ".to_string(),
            "beta ".to_string(),
            "gamma ".to_string(),
        ],
        delay: Duration::from_millis(180),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let session_id = create_session_via_api(&client, &server.base_url).await;

    let chat_task = tokio::spawn({
        let client = client.clone();
        let base_url = server.base_url.clone();
        let session_id = session_id.clone();
        async move {
            let response = client
                .post(format!("{base_url}/api/chat"))
                .json(&serde_json::json!({
                    "sessionId": session_id,
                    "content": "Say several chunks slowly."
                }))
                .send()
                .await
                .expect("chat request should succeed");
            let status = response.status();
            let payload: Value = response
                .json()
                .await
                .expect("chat cancellation response should be valid JSON");
            (status, payload)
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if mock.request_count().await >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "LLM request did not reach mock server within 10s; server logs:\n{}",
            server.logs().await
        );
        sleep(Duration::from_millis(50)).await;
    }
    sleep(Duration::from_millis(450)).await;

    let stop_response = client
        .post(format!(
            "{}/api/sessions/{}/stop",
            server.base_url, session_id
        ))
        .send()
        .await
        .expect("stop session request should succeed");
    assert_eq!(stop_response.status(), StatusCode::OK);

    let (chat_status, chat_payload) = chat_task.await.expect("chat task should join");
    assert_eq!(
        chat_status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "cancelled synchronous chat should surface cancellation, got payload: {chat_payload}"
    );

    wait_for_runtime_state_clear(
        &client,
        &server.base_url,
        &session_id,
        &server,
        Duration::from_secs(10),
    )
    .await;

    let session_response = client
        .get(format!("{}/api/sessions/{}", server.base_url, session_id))
        .send()
        .await
        .expect("get session after cancel should succeed");
    assert_eq!(session_response.status(), StatusCode::OK);
    let session_payload: Value = session_response
        .json()
        .await
        .expect("session payload after cancel should be valid JSON");
    let assistant_messages = extract_assistant_messages(&session_payload);
    assert!(
        assistant_messages
            .iter()
            .any(|content| content.contains("alpha")),
        "partial assistant text should be persisted after cancellation: {:?}",
        assistant_messages
    );

    server.stop().await;
}

#[tokio::test]
#[serial]
async fn ui_chat_executes_terminal_and_terminal_input_round_trip() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::TerminalRoundTrip).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let session_id = create_session_via_api(&client, &server.base_url).await;

    let chat_task = tokio::spawn({
        let client = client.clone();
        let base_url = server.base_url.clone();
        let session_id = session_id.clone();
        async move {
            let response = client
                .post(format!("{base_url}/api/chat"))
                .json(&serde_json::json!({
                    "sessionId": session_id,
                    "content": "Please ask the terminal for my name and answer with the result."
                }))
                .send()
                .await
                .expect("chat request should succeed");
            let status = response.status();
            let payload: Value = response
                .json()
                .await
                .expect("chat response should be valid JSON");
            (status, payload)
        }
    });

    let runtime_during_background = wait_for_runtime_condition(
        &client,
        &server.base_url,
        &session_id,
        &server,
        Duration::from_secs(10),
        "terminal runtime to expose taskId and outputFile",
        |runtime| {
            let terminal = runtime
                .get("interactions")
                .and_then(|interactions| interactions.get("terminal"))
                .unwrap_or(&Value::Null);
            terminal.get("taskId").and_then(Value::as_str).is_some()
                && terminal.get("outputFile").and_then(Value::as_str).is_some()
        },
    )
    .await;
    let task_id = runtime_during_background
        .get("interactions")
        .and_then(|interactions| interactions.get("terminal"))
        .and_then(|terminal| terminal.get("taskId"))
        .and_then(Value::as_str)
        .expect("runtime should expose active terminal task id")
        .to_string();
    let output_file = runtime_during_background
        .get("interactions")
        .and_then(|interactions| interactions.get("terminal"))
        .and_then(|terminal| terminal.get("outputFile"))
        .and_then(Value::as_str)
        .expect("runtime should expose background output file")
        .to_string();
    assert!(
        output_file.starts_with("outputs/"),
        "background output file should point into outputs dir: {output_file}"
    );

    let (status, chat_payload) = chat_task.await.expect("chat task should join");
    let server_logs = server.logs().await;
    assert_eq!(
        status,
        StatusCode::OK,
        "terminal round-trip chat failed with payload: {chat_payload}; server logs:\n{server_logs}"
    );
    assert_eq!(
        chat_payload
            .get("assistant")
            .and_then(|assistant| assistant.get("content"))
            .and_then(Value::as_str),
        Some("terminal flow complete")
    );

    let executions = chat_payload
        .get("assistant")
        .and_then(|assistant| assistant.get("terminalExecutions"))
        .and_then(Value::as_array)
        .expect("chat response should include terminal executions");
    assert!(
        executions.iter().any(|execution| {
            execution
                .get("input")
                .and_then(Value::as_str)
                .is_some_and(|input| input.contains("read name"))
        }),
        "chat response should record the terminal execution"
    );

    wait_for_runtime_condition(
        &client,
        &server.base_url,
        &session_id,
        &server,
        Duration::from_secs(10),
        "terminal runtime to clear after background completion",
        |runtime| {
            runtime.get("state").is_none()
                && runtime
                    .get("interactions")
                    .and_then(|interactions| interactions.get("terminal"))
                    .is_none()
        },
    )
    .await;

    assert!(
        mock.request_count().await >= 4,
        "expected terminal round trip to make multiple LLM requests"
    );
    assert!(
        !task_id.trim().is_empty(),
        "terminal task id should not be empty"
    );

    server.stop().await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
#[serial]
async fn ui_linux_runtime_has_heel_env_for_sandbox_stdin_detection() {
    let heel_bin = std::env::var("HEEL_BIN")
        .expect("Linux sandbox stdin-detection UI test requires HEEL_BIN to be set");
    assert!(
        Path::new(&heel_bin).exists(),
        "HEEL_BIN must point to an existing prebuilt heel binary"
    );

    let mock = MockOpenAiServer::spawn(MockChatScenario::TerminalRoundTrip).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[("HEEL_BIN", &heel_bin)]).await;

    server.stop().await;
}
