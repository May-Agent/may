//! End-to-end tests for May's terminal-first execution and REST/SSE contracts.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use aither_sandbox::ToolTarget;
use askama::Template;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use may_agent::config::CommandPriority;
use may_agent::config::{ProviderInstance, ProviderType};
use may_agent::model_selector::ModelSelector;
use may_agent::session::{
    SandboxResourceLayout, SessionId, SessionStore, initialize_session_persistence,
};
use may_agent::{MayConfig, MayHomePaths};
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
const TITLE_REQUEST_PREFIX: &str =
    "Generate a short, descriptive title (3-6 words) for this conversation.";

#[derive(Template)]
#[template(path = "test_terminal_input.sh", escape = "none")]
struct TerminalInputScript<'a> {
    task_id: &'a str,
}

#[derive(Template)]
#[template(path = "test_terminal_read.sh", escape = "none")]
struct TerminalReadScript<'a> {
    task_id: &'a str,
}

#[derive(Clone)]
enum MockChatScenario {
    SlowText {
        chunks: Vec<String>,
        delay: Duration,
    },
    TerminalRoundTrip,
    /// Answers every agent turn with the same text, so one session can run
    /// several turns against one mock server.
    TextEveryTurn(String),
    /// First turn writes `tasks.md` through the terminal tool; every later
    /// request gets a plain text reply, so follow-up turns can probe what
    /// the model side injects from the file.
    WritesTasksFile,
    /// The main agent launches a real subagent from a terminal command and a
    /// scripted child runs one probe; see [`SubagentProbe`].
    SubagentProbe(SubagentProbe),
    /// The main agent prints a secret, then forks itself with
    /// `subagent start fork`; the fork answers with [`FORK_ANSWER`].
    ForkProbe,
}

/// A scripted main agent / subagent pair sharing one mock model.
///
/// The main agent runs `setup`, launches `subagent start` with `grant_flags`
/// in a background job, polls until the launch command finished, then runs
/// `final_script`. The child runs `child_script` once and ends. A request is
/// the child's when a user or system message carries [`SUBAGENT_PROBE_MARKER`],
/// which only the subagent's prompt file does.
#[derive(Clone)]
struct SubagentProbe {
    setup: &'static str,
    grant_flags: &'static str,
    child_script: &'static str,
    final_script: &'static str,
}

const SUBAGENT_PROBE_MARKER: &str = "SUBAGENT-PROBE-TASK";
/// What only the main agent's context holds in the fork probe: printed by a
/// command, assembled by the shell so the script itself never spells it.
const FORK_SECRET: &str = "FORK-CONTEXT-SECRET-7431";
const FORK_SECRET_SCRIPT: &str = "printf '%s-%s' FORK-CONTEXT-SECRET 7431";
/// Opens the fork's prompt file; the last message of a fork's first request.
const FORK_TASK_MARKER: &str = "FORK-PROBE-TASK";
/// What the fork replies with, and the main agent reads back from `sub.out`.
const FORK_ANSWER: &str = "FORK-ANSWER-OK";
/// Requests the probe scenario may serve before the harness fails it, so a
/// launch that never finishes ends the test with a message instead of hanging.
const SUBAGENT_PROBE_MAX_REQUESTS: usize = 40;

/// Waits at most eight seconds (the foreground limit is ten) for the launch
/// job to finish, and prints what it wrote.
const SUBAGENT_POLL_SCRIPT: &str = "i=0; while [ $i -lt 8 ]; do if [ -f sub.done ]; then echo POLL_DONE; cat sub.out; exit 0; fi; sleep 1; i=$((i+1)); done; echo POLL_PENDING";

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
        let transcript = if matches!(&scenario, MockChatScenario::TerminalRoundTrip) {
            std::env::var_os("MAY_TEST_TRANSCRIPT_DIR").map(|directory| {
                PathBuf::from(directory).join("terminal-roundtrip-model-requests.json")
            })
        } else {
            None
        };
        if let Some(path) = &transcript {
            write_request_transcript(path, &requests).await;
        }
        let state = Arc::new(tokio::sync::Mutex::new(MockScenarioState::new(scenario)));
        let requests_for_task = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let io = TokioIo::new(stream);
                let state = state.clone();
                let requests = requests_for_task.clone();
                let transcript = transcript.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        handle_mock_openai_request(
                            request,
                            state.clone(),
                            requests.clone(),
                            transcript.clone(),
                        )
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

    /// The agent-turn requests received so far, in arrival order; session
    /// title requests are left out.
    async fn agent_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .await
            .iter()
            .filter(|request| !is_session_title_request(request))
            .cloned()
            .collect()
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
        if let MockChatScenario::SubagentProbe(probe) = self.scenario.clone() {
            return self.next_probe_response(&probe, request);
        }
        if matches!(self.scenario, MockChatScenario::ForkProbe) {
            return self.next_fork_response(request);
        }
        verify_native_tool_surface(request)?;
        if self.request_index == 0 {
            verify_initial_tool_contract(request)?;
        }
        let response = match &self.scenario {
            MockChatScenario::SubagentProbe(_) | MockChatScenario::ForkProbe => {
                unreachable!("handled before the match")
            }
            MockChatScenario::TextEveryTurn(text) => {
                // Every rebuilt agent must still present the prepared tools,
                // whether it prepared them or adopted them.
                verify_initial_tool_contract(request)?;
                MockHttpResponse::sse(text_stream_frames(vec![text.clone()], Duration::ZERO))
            }
            MockChatScenario::SlowText { chunks, delay } => {
                if self.request_index != 0 {
                    return Err(format!(
                        "unexpected extra request for slow-text scenario: {}",
                        self.request_index
                    ));
                }
                MockHttpResponse::sse(text_stream_frames(chunks.clone(), *delay))
            }
            MockChatScenario::TerminalRoundTrip => match self.request_index {
                // The command blocks on `read`, so it runs as a background
                // job: `terminal` foreground calls are killed at their bound,
                // never promoted.
                0 => MockHttpResponse::sse(vec![
                    delayed_sse_frame(
                        tool_call_chunk(
                            "call-terminal",
                            "terminal_background",
                            &serde_json::json!({
                                "description": "Ask the terminal for the user's name",
                                "script": "printf 'cwd=%s\\n' \"$(pwd -P)\"; printf 'name? '; read name; printf 'hello %s\\n' \"$name\"",
                                "mode": "sandboxed",
                                "raw": true,
                                "timeout": 60,
                                "max_lines": 50
                            }),
                        ),
                        Duration::from_millis(10),
                    ),
                    done_frame(Duration::from_millis(10)),
                ]),
                1 => {
                    let task_id =
                        terminal_task_id_from_latest_tool_output(request).ok_or_else(|| {
                            "terminal tool result did not include a task_id".to_string()
                        })?;
                    self.task_id = Some(task_id.clone());
                    let script = TerminalInputScript { task_id: &task_id }
                        .render()
                        .map_err(|error| error.to_string())?;
                    MockHttpResponse::sse(vec![
                        delayed_sse_frame(
                            tool_call_chunk(
                                "call-terminal-input",
                                "terminal",
                                &serde_json::json!({
                                    "description": "Send a name through terminal IPC",
                                    "script": script,
                                    "mode": "sandboxed",
                                    "raw": true,
                                    "timeout": 10,
                                    "max_lines": 50
                                }),
                            ),
                            Duration::from_millis(350),
                        ),
                        done_frame(Duration::from_millis(10)),
                    ])
                }
                2 => {
                    let input_result = latest_tool_output(request)
                        .ok_or_else(|| "terminal input command result missing".to_string())?;
                    if tool_output_reports_failure(&input_result) {
                        return Err(format!("terminal input IPC command failed: {input_result}"));
                    }
                    let task_id = self
                        .task_id
                        .clone()
                        .ok_or_else(|| "terminal task_id was not captured".to_string())?;
                    let script = TerminalReadScript { task_id: &task_id }
                        .render()
                        .map_err(|error| error.to_string())?;
                    MockHttpResponse::sse(vec![
                        delayed_sse_frame(
                            tool_call_chunk(
                                "call-terminal-read",
                                "terminal",
                                &serde_json::json!({
                                    "description": "Read the reply through terminal IPC",
                                    "script": script,
                                    "mode": "sandboxed",
                                    "raw": true,
                                    "timeout": 10,
                                    "max_lines": 50
                                }),
                            ),
                            Duration::from_millis(250),
                        ),
                        done_frame(Duration::from_millis(10)),
                    ])
                }
                3 => {
                    let tool_output = latest_tool_output(request)
                        .ok_or_else(|| "terminal read command result missing".to_string())?;
                    if tool_output_reports_failure(&tool_output) {
                        return Err(format!("terminal read IPC command failed: {tool_output}"));
                    }
                    let text = tool_output.as_str();
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
                        delayed_sse_frame(text_chunk("", Some("stop")), Duration::from_millis(10)),
                        done_frame(Duration::from_millis(10)),
                    ])
                }
                other => {
                    return Err(format!(
                        "unexpected request index {other} for terminal round trip"
                    ));
                }
            },
            MockChatScenario::WritesTasksFile => {
                // The run loop keeps the model going while `tasks.md` has
                // unchecked items, and the model side sees the file once per
                // change (whole at session start, a diff afterwards), so the
                // mock finishes the checklist whenever the newest injection
                // adds an unchecked item and replies with plain text once
                // everything is checked.
                if self.request_index >= TASKS_SCENARIO_MAX_REQUESTS {
                    return Err(format!(
                        "tasks scenario still looping after {} requests: the run loop keeps objecting to tasks.md",
                        self.request_index
                    ));
                }
                let script = match tasks_injection(request) {
                    None if self.request_index == 0 => Some(TASKS_PARTIAL_SCRIPT),
                    Some(tasks) if injection_adds_unchecked_item(tasks) => Some(TASKS_DONE_SCRIPT),
                    _ => None,
                };
                match script {
                    Some(script) => MockHttpResponse::sse(vec![
                        delayed_sse_frame(
                            tool_call_chunk(
                                &format!("call-tasks-md-{}", self.request_index),
                                "terminal",
                                &serde_json::json!({
                                    "description": "Record the plan in tasks.md",
                                    "script": script,
                                    "mode": "sandboxed",
                                    "raw": true,
                                    "timeout": 10,
                                    "max_lines": 50
                                }),
                            ),
                            Duration::from_millis(10),
                        ),
                        done_frame(Duration::from_millis(10)),
                    ]),
                    None => MockHttpResponse::sse(text_stream_frames(
                        vec!["tasks recorded".to_string()],
                        Duration::from_millis(10),
                    )),
                }
            }
        };
        self.request_index += 1;
        Ok(response)
    }

    fn next_probe_response(
        &mut self,
        probe: &SubagentProbe,
        request: &Value,
    ) -> Result<MockHttpResponse, String> {
        if self.request_index >= SUBAGENT_PROBE_MAX_REQUESTS {
            return Err(format!(
                "subagent probe still running after {} requests: the launch never finished",
                self.request_index
            ));
        }
        let is_child = is_subagent_request(request);
        if !is_child {
            verify_native_tool_surface(request)?;
        }
        let latest = latest_tool_output(request);
        let terminal_call = |name: &str, id: &str, script: &str| {
            MockHttpResponse::sse(vec![
                delayed_sse_frame(
                    tool_call_chunk(
                        id,
                        name,
                        &serde_json::json!({
                            "description": "Run the subagent authority probe",
                            "script": script,
                            "mode": "sandboxed",
                            "raw": true,
                            "timeout": 120,
                            "max_lines": 200
                        }),
                    ),
                    Duration::from_millis(10),
                ),
                done_frame(Duration::from_millis(10)),
            ])
        };
        let text = |text: String| {
            MockHttpResponse::sse(text_stream_frames(vec![text], Duration::from_millis(10)))
        };
        let id = format!("call-probe-{}", self.request_index);
        let response = if is_child {
            latest.map_or_else(
                || terminal_call("terminal", &id, probe.child_script),
                |output| text(format!("child probe output: {output}")),
            )
        } else {
            match latest {
                None => {
                    let launch = format!(
                        "( {setup}\nprintf '{SUBAGENT_PROBE_MARKER}: run the probe.\\n' > prompt.md\nsubagent start explore prompt.md --model fast {flags} ) > sub.out 2>&1; touch sub.done",
                        setup = probe.setup,
                        flags = probe.grant_flags,
                    );
                    terminal_call("terminal_background", &id, &launch)
                }
                Some(output) if output.contains("MAIN_FINAL") => text("probe complete".to_string()),
                Some(output) if output.contains("POLL_DONE") => {
                    terminal_call("terminal", &id, probe.final_script)
                }
                Some(_) => terminal_call("terminal", &id, SUBAGENT_POLL_SCRIPT),
            }
        };
        self.request_index += 1;
        Ok(response)
    }

    fn next_fork_response(&mut self, request: &Value) -> Result<MockHttpResponse, String> {
        if self.request_index >= SUBAGENT_PROBE_MAX_REQUESTS {
            return Err(format!(
                "fork probe still running after {} requests: the launch never finished",
                self.request_index
            ));
        }
        let terminal_call = |name: &str, id: &str, script: &str| {
            MockHttpResponse::sse(vec![
                delayed_sse_frame(
                    tool_call_chunk(
                        id,
                        name,
                        &serde_json::json!({
                            "description": "Run the fork probe",
                            "script": script,
                            "mode": "sandboxed",
                            "raw": true,
                            "timeout": 120,
                            "max_lines": 200
                        }),
                    ),
                    Duration::from_millis(10),
                ),
                done_frame(Duration::from_millis(10)),
            ])
        };
        let text = |text: &str| {
            MockHttpResponse::sse(text_stream_frames(
                vec![text.to_string()],
                Duration::from_millis(10),
            ))
        };
        let id = format!("call-fork-{}", self.request_index);
        let response = if is_fork_request(request) {
            text(FORK_ANSWER)
        } else {
            match latest_tool_output(request) {
                None => terminal_call("terminal", &id, FORK_SECRET_SCRIPT),
                Some(output) if output.contains("MAIN_FINAL") => text("fork probe complete"),
                Some(output) if output.contains("POLL_DONE") => {
                    terminal_call("terminal", &id, "echo MAIN_FINAL")
                }
                Some(output) if output.contains(FORK_SECRET) => {
                    let launch = format!(
                        "( printf '{FORK_TASK_MARKER}: report what you know.\\n' > prompt.md\nsubagent start fork prompt.md ) > sub.out 2>&1; touch sub.done"
                    );
                    terminal_call("terminal_background", &id, &launch)
                }
                Some(_) => terminal_call("terminal", &id, SUBAGENT_POLL_SCRIPT),
            }
        };
        self.request_index += 1;
        Ok(response)
    }
}

const TASKS_PARTIAL_SCRIPT: &str = "printf -- '- [x] write report\\n- [ ] ship it\\n' > tasks.md";
const TASKS_DONE_SCRIPT: &str = "printf -- '- [x] write report\\n- [x] ship it\\n' > tasks.md";

/// Requests the tasks scenario may serve before the harness fails it, so a
/// run loop that never settles ends the test with a message instead of hanging.
const TASKS_SCENARIO_MAX_REQUESTS: usize = 8;

/// Whether a `<tasks_document>` injection leaves an unchecked item open: the
/// whole document at session start, or a diff whose added (`+ `) lines carry
/// one. Removed lines of a diff say nothing about the current file.
fn injection_adds_unchecked_item(injection: &str) -> bool {
    injection.lines().any(|line| {
        line.starts_with("- [ ]")
            || line
                .strip_prefix("+ ")
                .is_some_and(|added| added.starts_with("- [ ]"))
    })
}

/// How long a chat turn in the tasks test may take before the test fails.
const TASKS_TURN_TIMEOUT: Duration = Duration::from_secs(60);

/// Collects a turn's events, failing with the turn and the server log when it
/// does not finish within [`TASKS_TURN_TIMEOUT`].
async fn collect_turn_events(
    response: reqwest::Response,
    turn: &str,
    log: &ServerLog,
) -> Vec<(String, Value)> {
    let collected = tokio::time::timeout(TASKS_TURN_TIMEOUT, collect_sse_events(response)).await;
    let Ok(events) = collected else {
        panic!(
            "{turn} produced no done event within {TASKS_TURN_TIMEOUT:?}; server logs:\n{}",
            log.contents().await
        );
    };
    events
}

/// The `<tasks_document>` system message the run loop injects from `tasks.md`.
fn tasks_injection(request: &Value) -> Option<&str> {
    request
        .get("messages")?
        .as_array()?
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .rfind(|content| content.starts_with("<tasks_document>"))
}

fn verify_native_tool_surface(request: &Value) -> Result<(), String> {
    let tools = request
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "initial model request has no native tool definitions".to_string())?;
    let mut names = tools
        .iter()
        .map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| format!("native tool definition without a name: {tools:?}"))?;
    names.sort_unstable();
    if names
        != [
            "terminal",
            "terminal_background",
            "terminal_input",
            "terminal_kill",
            "terminal_read",
        ]
    {
        return Err(format!(
            "expected the native terminal tool and its controls: {names:?}"
        ));
    }
    Ok(())
}

fn verify_initial_tool_contract(request: &Value) -> Result<(), String> {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "initial model request has no messages".to_string())?;
    let catalog = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .find(|content| content.contains("<prepared_tools>"))
        .ok_or_else(|| "initial model request has no prepared tool catalog".to_string())?;
    for command in ["apply_patch ", "rg ", "ast-grep ", "jq "] {
        if !catalog.contains(command) {
            return Err(format!("prepared tool catalog omits {command}"));
        }
    }
    Ok(())
}

enum MockHttpResponse {
    Sse(Vec<MockSseFrame>),
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

const fn delayed_sse_frame(data: String, delay: Duration) -> MockSseFrame {
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
    transcript: Option<PathBuf>,
) -> Result<Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>, Infallible> {
    if request.method() == Method::GET && request.uri().path() == "/v1/models" {
        return Ok(json_response(
            StatusCode::OK,
            &serde_json::json!({
                "data": [{
                    "id": TEST_MODEL,
                    "owned_by": "mock",
                    "context_length": 128_000
                }]
            }),
        ));
    }

    if request.method() != Method::POST || request.uri().path() != "/v1/chat/completions" {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "not found" }),
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
    if let Some(path) = &transcript {
        write_request_transcript(path, &requests).await;
    }

    // Title generation is a separate, tool-free request, not an agent turn.
    if is_session_title_request(&payload) {
        assert!(payload.get("tools").is_none());
        return Ok(sse_response(vec![
            delayed_sse_frame(
                text_chunk("Terminal name exchange", Some("stop")),
                Duration::ZERO,
            ),
            done_frame(Duration::ZERO),
        ]));
    }

    let response = {
        let mut state = state.lock().await;
        match state.next_response(&payload) {
            Ok(response) => response,
            Err(error) => {
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &serde_json::json!({ "error": { "message": error } }),
                ));
            }
        }
    };

    Ok(match response {
        MockHttpResponse::Sse(frames) => sse_response(frames),
    })
}

fn is_session_title_request(request: &Value) -> bool {
    request
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.len() == 1
                && messages[0].get("role").and_then(Value::as_str) == Some("user")
                && messages[0]
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.starts_with(TITLE_REQUEST_PREFIX))
        })
}

async fn write_request_transcript(path: &Path, requests: &Arc<tokio::sync::Mutex<Vec<Value>>>) {
    let transcript = {
        let requests = requests.lock().await;
        serde_json::to_vec_pretty(requests.as_slice()).expect("model requests should serialize")
    };
    async_fs::write(path, transcript)
        .await
        .expect("model request transcript should be written");
}

fn json_response(
    status: StatusCode,
    value: &Value,
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

        seed_container_heel(&may_dir).await;
        futures_lite::future::zip(
            share_and_warm_tool_cache(&may_dir),
            warm_may_binary(home.path()),
        )
        .await;

        Self { home, may_dir }
    }

    /// Plain-text tracing output May writes under the test home's log
    /// directory (the stderr copy carries ANSI styling).
    async fn file_logs(&self) -> String {
        let log_dir = self.may_dir.join("logs");
        let mut entries = tokio::fs::read_dir(&log_dir)
            .await
            .expect("May log directory should exist");
        let mut logs = String::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .expect("May log directory should be readable")
        {
            if entry.file_name().to_string_lossy().starts_with("may.log") {
                logs.push_str(
                    &tokio::fs::read_to_string(entry.path())
                        .await
                        .expect("May log file should be readable"),
                );
            }
        }
        logs
    }

    /// Session in the local profile. Terminal-first contract tests use this so
    /// they exercise Leash, the session filesystem, and IPC independently of
    /// an optional host container runtime.
    async fn create_session(&self) -> TestSession {
        self.create_session_in_profile("local").await
    }

    async fn create_session_in_profile(&self, profile_slug: &str) -> TestSession {
        let (store, db, _default_profile_id) = initialize_session_persistence(&self.may_dir)
            .await
            .expect("session persistence should initialize");
        let profile_id = db
            .get_profile_by_slug(profile_slug)
            .await
            .expect("profile lookup should succeed")
            .unwrap_or_else(|| panic!("default profile `{profile_slug}` should exist"))
            .id;
        let session_id = SessionId::new().to_string();
        store
            .create_session_dirs(
                &session_id,
                Some(profile_id.as_str()),
                SandboxResourceLayout::Symlinked,
            )
            .await
            .expect("session directory layout should be created");
        db.create_session_in_profile(
            &session_id,
            None,
            Some(TEST_SELECTOR),
            Some(profile_id.as_str()),
        )
        .await
        .expect("session row should be created");
        let sandbox_dir = store.sandbox_dir(&session_id, Some(profile_id.as_str()));
        TestSession {
            id: session_id,
            sandbox_dir,
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    /// The managed container heel's release state, as stored.
    async fn container_heel_state(&self) -> String {
        async_fs::read_to_string(self.may_dir.join("state/container-heel-release.json"))
            .await
            .expect("container heel release state should be readable")
    }
}

struct TestSession {
    id: String,
    sandbox_dir: PathBuf,
}

struct RunningUiServer {
    child: Child,
    base_url: String,
    log: ServerLog,
}

/// The file a spawned UI server's stdout and stderr go to. Cheap to clone, so
/// tasks that outlive a borrow of the server can still report its logs.
#[derive(Clone)]
struct ServerLog {
    path: PathBuf,
}

impl ServerLog {
    async fn contents(&self) -> String {
        tokio::fs::read_to_string(&self.path)
            .await
            .unwrap_or_else(|error| {
                format!(
                    "failed to read ui server logs at {}: {error}",
                    self.path.display()
                )
            })
    }
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
            // Module-level directives outrank May's baked crate-level filter,
            // keeping failed assertions diagnosable from captured logs.
            .env(
                "RUST_LOG",
                "aither_openai::client=debug,may_agent::ui_server::chat=debug,may_agent::container=debug",
            )
            .stdin(Stdio::null())
            .stdout(StdStdio::from(log_file))
            .stderr(StdStdio::from(log_file_err));
        command.env("HEEL_BIN", heel_bin());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("may rest should spawn");
        let base_url = format!("http://127.0.0.1:{port}");
        let mut server = Self {
            child,
            base_url,
            log: ServerLog { path: log_path },
        };
        server.wait_until_healthy(Duration::from_secs(15)).await;
        server
    }

    async fn wait_until_healthy(&mut self, timeout_duration: Duration) {
        let client = reqwest::Client::new();
        let url = format!("{}/api/health", self.base_url);
        let deadline = Instant::now() + timeout_duration;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "ui server exited with {status} before becoming healthy; server logs:\n{}",
                    self.log.contents().await
                );
            }
            let healthy = client
                .get(&url)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success());
            if healthy {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "ui server did not become healthy within {:?}; server logs:\n{}",
                timeout_duration,
                self.log.contents().await
            );
            sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// The first load of a freshly rebuilt (large, ad-hoc-signed) debug binary
/// can stall many seconds inside dyld/AMFI code-signature validation. Pay
/// that cost here so server spawns stay within their health-check timeout.
async fn warm_may_binary(home: &Path) {
    let output = Command::new(may_binary_path())
        .arg("--help")
        .env("HOME", home)
        .output()
        .await
        .expect("may --help should run");
    assert!(
        output.status.success(),
        "may --help should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

/// Release of heel whose Linux binary the harness stages as the container heel.
const CONTAINER_HEEL_RELEASE_TAG: &str = "heel-v0.5.0";

/// A pinned Linux heel release asset and the SHA-256 of its published bytes.
struct PinnedHeelAsset {
    name: &'static str,
    sha256: &'static str,
}

// The container heel asset for this architecture, mirroring the product's
// `linux_heel_asset_for_current_arch` (crate-private in `src/heel/release.rs`).
#[cfg(target_arch = "x86_64")]
const CONTAINER_HEEL_ASSET: PinnedHeelAsset = PinnedHeelAsset {
    name: "heel-x86_64-unknown-linux-gnu",
    sha256: "10ffff205170c4e6d9c54feb7448716e04e2ef1943ffa77bce75ee791598d50d",
};
#[cfg(target_arch = "aarch64")]
const CONTAINER_HEEL_ASSET: PinnedHeelAsset = PinnedHeelAsset {
    name: "heel-aarch64-unknown-linux-gnu",
    sha256: "0aa2ed4d91a85e312ff3608263da6da38831d0787b940bd2be23e92c6b5dd995",
};

/// Stages the pinned container heel in the test home exactly as the product's
/// managed install leaves it — `container/heel/bin/heel` plus a
/// `state/container-heel-release.json` checked just now — so the server's
/// recent-check fast path never queries the GitHub release API.
async fn seed_container_heel(may_dir: &Path) {
    let source = pinned_container_heel().await;
    let binary_path = may_dir
        .join("container")
        .join("heel")
        .join("bin")
        .join("heel");
    async_fs::create_dir_all(binary_path.parent().expect("binary path has a parent"))
        .await
        .expect("container heel bin dir should be created");
    async_fs::copy(&source, &binary_path)
        .await
        .expect("pinned container heel should copy into the test home");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        async_fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))
            .await
            .expect("container heel should be made executable");
    }
    // The product's `ManagedHeelState` is crate-private; this is its serde shape.
    let state = serde_json::json!({
        "release_tag": CONTAINER_HEEL_RELEASE_TAG,
        "asset_name": CONTAINER_HEEL_ASSET.name,
        "last_checked_at": jiff::Timestamp::now(),
    });
    let state_path = may_dir.join("state").join("container-heel-release.json");
    async_fs::create_dir_all(state_path.parent().expect("state path has a parent"))
        .await
        .expect("container heel state dir should be created");
    async_fs::write(
        &state_path,
        serde_json::to_vec(&state).expect("container heel release state should serialize"),
    )
    .await
    .expect("container heel release state should be written");
}

/// The pinned container heel in the shared test-tmp cache, downloaded from
/// its release on first use. Its SHA-256 is verified every time it is handed
/// out, whether freshly downloaded or reused.
async fn pinned_container_heel() -> PathBuf {
    let cache_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("container-heel")
        .join(CONTAINER_HEEL_RELEASE_TAG);
    let cached = cache_dir.join(CONTAINER_HEEL_ASSET.name);
    match async_fs::read(&cached).await {
        Ok(bytes) => {
            verify_container_heel(bytes, &cached.display().to_string()).await;
            return cached;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!(
            "cached container heel {} should be readable: {error}",
            cached.display()
        ),
    }

    let url = format!(
        "https://github.com/lexoliu/heel/releases/download/{CONTAINER_HEEL_RELEASE_TAG}/{}",
        CONTAINER_HEEL_ASSET.name
    );
    let bytes = reqwest::get(&url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .unwrap_or_else(|error| panic!("pinned container heel download {url} failed: {error}"))
        .bytes()
        .await
        .unwrap_or_else(|error| panic!("pinned container heel body from {url} failed: {error}"));
    let bytes = verify_container_heel(bytes.to_vec(), &url).await;

    async_fs::create_dir_all(&cache_dir)
        .await
        .expect("container heel cache dir should be created");
    // Concurrent test binaries may download at once: each writes its own
    // temporary file and renames it into place atomically.
    let staging = tempfile::NamedTempFile::new_in(&cache_dir)
        .expect("container heel staging file should be created");
    async_fs::write(staging.path(), bytes)
        .await
        .expect("container heel staging file should be written");
    staging
        .persist(&cached)
        .expect("container heel should move into the cache");
    cached
}

/// Panics unless `bytes` hash to the pinned digest; returns them on success.
async fn verify_container_heel(bytes: Vec<u8>, origin: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    let (bytes, actual) = blocking::unblock(move || {
        let digest = format!("{:x}", Sha256::digest(&bytes));
        (bytes, digest)
    })
    .await;
    assert_eq!(
        actual, CONTAINER_HEEL_ASSET.sha256,
        "container heel {} from {origin} has SHA-256 {actual}, expected {}",
        CONTAINER_HEEL_ASSET.name, CONTAINER_HEEL_ASSET.sha256
    );
    bytes
}

/// Points the test home's tool cache at one cache shared by every test run
/// and prepares the host's baseline tools in it through the embedded
/// distribution, so the first hash-verified download happens here instead of
/// inside a server's readiness deadline. Later runs reuse the cached artifacts.
async fn share_and_warm_tool_cache(may_dir: &Path) {
    let shared = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("may-tools");
    tokio::fs::create_dir_all(&shared)
        .await
        .expect("shared tool cache dir should be created");
    let home_paths = MayHomePaths::new(may_dir.to_path_buf());
    #[cfg(unix)]
    tokio::fs::symlink(&shared, home_paths.tools_dir())
        .await
        .expect("test home tool cache should link to the shared cache");
    #[cfg(windows)]
    tokio::fs::symlink_dir(&shared, home_paths.tools_dir())
        .await
        .expect("test home tool cache should link to the shared cache");
    let bundle = may_agent::tools::embedded_distribution()
        .expect("embedded distribution should validate")
        .load(
            &ToolTarget::native().expect("test host should have a tool target"),
            &home_paths.tool_artifacts_dir(),
        )
        .await
        .expect("baseline tool artifacts should download and verify");
    bundle
        .prepare_local(&home_paths.tool_runtimes_dir())
        .await
        .expect("baseline tools should prepare");
}

/// The host heel binary the E2E suite runs commands under. `HEEL_BIN` is its
/// only source; the suite never probes the developer's machine for one.
fn heel_bin() -> String {
    let path = std::env::var("HEEL_BIN").unwrap_or_else(|_| {
        panic!("HEEL_BIN must name a prebuilt host heel binary for the terminal E2E suite")
    });
    assert!(
        Path::new(&path).is_file(),
        "HEEL_BIN={path} must name an existing heel binary file"
    );
    path
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
    const fn new(response: reqwest::Response) -> Self {
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
            if let Some(bytes) = chunk {
                self.buffer.push_str(&String::from_utf8_lossy(&bytes));
            } else {
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

struct TestSseEvent {
    event: String,
    data: String,
}

/// Drain a chat SSE response into `(event kind, parsed data)` pairs until the
/// turn finishes (`done`/`paused`/`error`) or the server closes the stream.
async fn collect_sse_events(response: reqwest::Response) -> Vec<(String, Value)> {
    let mut reader = SseReader::new(response);
    let mut events = Vec::new();
    while let Some(event) = reader.next_event().await {
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);
        let kind = event.event;
        let finished = matches!(kind.as_str(), "done" | "paused" | "error");
        events.push((kind, data));
        if finished {
            break;
        }
    }
    events
}

fn split_sse_block(buffer: &str) -> Option<(TestSseEvent, String)> {
    let delimiter = if let Some(position) = buffer.find("\n\n") {
        (position, 2)
    } else {
        let position = buffer.find("\r\n\r\n")?;
        (position, 4)
    };
    let (position, width) = delimiter;
    let event = parse_sse_block(&buffer[..position]);
    Some((event, buffer[position + width..].to_string()))
}

fn parse_sse_block(block: &str) -> TestSseEvent {
    let mut event = String::new();
    let mut data_lines = Vec::new();
    for line in block.lines() {
        // Per the SSE spec, a single space after the field colon is optional.
        if let Some(value) = line.strip_prefix("event:") {
            event = value.strip_prefix(' ').unwrap_or(value).to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.strip_prefix(' ').unwrap_or(value).to_string());
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

fn runtime_is_idle(runtime: &Value) -> bool {
    let state_idle = runtime
        .get("state")
        .is_none_or(|state| state.as_str() == Some("idle"));
    let terminal_clear = runtime
        .get("interactions")
        .and_then(|interactions| interactions.get("terminal"))
        .and_then(|terminal| terminal.get("taskId"))
        .is_none_or(Value::is_null);
    state_idle && terminal_clear
}

async fn wait_for_runtime_state_clear(
    client: &reqwest::Client,
    server: &RunningUiServer,
    session_id: &str,
    timeout_duration: Duration,
) {
    let deadline = Instant::now() + timeout_duration;
    loop {
        let last_runtime = get_runtime(client, server, session_id).await;
        if runtime_is_idle(&last_runtime) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "runtime state did not clear within {:?}; runtime: {}; server logs:\n{}",
            timeout_duration,
            last_runtime,
            server.log.contents().await
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_runtime_condition<F>(
    client: &reqwest::Client,
    server: &RunningUiServer,
    session_id: &str,
    timeout_duration: Duration,
    description: &str,
    mut predicate: F,
) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + timeout_duration;
    loop {
        let runtime = get_runtime(client, server, session_id).await;
        if predicate(&runtime) {
            return runtime;
        }
        assert!(
            Instant::now() < deadline,
            "runtime condition `{description}` was not satisfied within {:?}; runtime: {}; server logs:\n{}",
            timeout_duration,
            runtime,
            server.log.contents().await
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// The text of the latest tool message the model was shown.
fn latest_tool_output(request: &Value) -> Option<String> {
    let messages = request.get("messages")?.as_array()?;
    messages.iter().rev().find_map(|message| {
        let role = message.get("role")?.as_str()?;
        if role != "tool" {
            return None;
        }
        message.get("content")?.as_str().map(ToOwned::to_owned)
    })
}

/// Whether a command result shown to the model reports a non-zero exit code.
fn tool_output_reports_failure(output: &str) -> bool {
    output.contains("Exit code: ")
}

fn terminal_task_id_from_latest_tool_output(request: &Value) -> Option<String> {
    let output = latest_tool_output(request)?;
    let marker = "Started in background as task '";
    let start = output.find(marker)? + marker.len();
    let end = output[start..].find('\'')?;
    Some(output[start..start + end].to_string())
}
/// Return `response` when it carries `expected`, otherwise fail with its body
/// and the server's logs, which carry the cause of a redacted 5xx body.
async fn expect_status(
    log: &ServerLog,
    response: reqwest::Response,
    expected: StatusCode,
) -> reqwest::Response {
    let status = response.status();
    if status == expected {
        return response;
    }
    let url = response.url().clone();
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    panic!(
        "{url} returned {status}, expected {expected}; body: {body}; server logs:\n{}",
        log.contents().await
    );
}

/// The profile environment a chat round-trip test runs its session in.
#[derive(Debug, Clone, Copy)]
enum ChatEnvironment {
    /// The `local` profile: commands run on the host under heel.
    Local,
    /// The `work` profile: commands run inside the May workspace container.
    /// It is container-backed like `daily` but not location-aware, so a turn
    /// never performs a geolocation lookup.
    ///
    /// Its tests expect a container runtime on Linux (Docker on the CI
    /// runners), where a missing runtime fails them; elsewhere they are
    /// ignored unless requested.
    Container,
}

/// Upper bound on building the May workspace image from the repository's
/// container context on a cold runtime.
const CONTAINER_IMAGE_BUILD_TIMEOUT: Duration = Duration::from_mins(20);

/// Extra time a container session's first turn gets for starting its
/// container and preparing the container's tool bundle before the model is
/// first asked.
const CONTAINER_FIRST_TURN_ALLOWANCE: Duration = Duration::from_secs(120);

impl ChatEnvironment {
    const fn profile_slug(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Container => "work",
        }
    }

    /// Resolves the environment's profile on `server` and makes it ready for
    /// chat the way the web UI does before opening a profile.
    async fn prepare(
        self,
        harness: &TestHarness,
        client: &reqwest::Client,
        server: &RunningUiServer,
    ) -> PreparedEnvironment {
        let profile_id = profile_id_via_api(client, server, self.profile_slug()).await;
        match self {
            Self::Local => PreparedEnvironment::Local { profile_id },
            Self::Container => {
                let runtime = may_agent::container::ContainerRuntime::detect()
                    .await
                    .unwrap_or_else(|error| {
                        panic!(
                            "container-profile E2E tests need a reachable Docker-compatible \
                             runtime (Docker at /var/run/docker.sock, rootless Podman, or \
                             OrbStack): {error}"
                        )
                    });
                // Listing a container profile's sessions builds its image
                // from the bundled container context first.
                let Ok(response) = tokio::time::timeout(
                    CONTAINER_IMAGE_BUILD_TIMEOUT,
                    client
                        .get(format!(
                            "{}/api/profiles/{profile_id}/sessions",
                            server.base_url
                        ))
                        .send(),
                )
                .await
                else {
                    panic!(
                        "container profile image was not ready within \
                         {CONTAINER_IMAGE_BUILD_TIMEOUT:?}; server logs:\n{}",
                        server.log.contents().await
                    );
                };
                let response = match response {
                    Ok(response) => response,
                    Err(error) => panic!(
                        "container profile preparation request failed: {error}; server logs:\n{}",
                        server.log.contents().await
                    ),
                };
                expect_status(&server.log, response, StatusCode::OK).await;
                let tool_distribution = may_agent::tools::embedded_distribution()
                    .expect("embedded distribution should validate");
                PreparedEnvironment::Container {
                    profile_id,
                    manager: may_agent::container::lifecycle::ContainerManager::new(
                        Arc::new(runtime),
                        harness.may_dir.clone(),
                        Arc::new(tool_distribution),
                    ),
                }
            }
        }
    }
}

/// A [`ChatEnvironment`] whose profile is ready to host chat sessions.
enum PreparedEnvironment {
    Local {
        profile_id: String,
    },
    Container {
        profile_id: String,
        manager: may_agent::container::lifecycle::ContainerManager,
    },
}

impl PreparedEnvironment {
    fn profile_id(&self) -> &str {
        match self {
            Self::Local { profile_id } | Self::Container { profile_id, .. } => profile_id,
        }
    }

    /// Extra time the first turn of a session gets before the model is asked.
    const fn first_turn_allowance(&self) -> Duration {
        match self {
            Self::Local { .. } => Duration::ZERO,
            Self::Container { .. } => CONTAINER_FIRST_TURN_ALLOWANCE,
        }
    }

    /// The first line of the environment context May's system prompt carries
    /// for this environment.
    const fn context_heading(&self) -> &'static str {
        match self {
            Self::Local { .. } => "Environment: local (user's real machine)",
            Self::Container { .. } => {
                "Environment: container (managed runtime, not the user's host machine)"
            }
        }
    }

    /// The physical directory a session's terminal commands start in: the
    /// session sandbox on the host, or its `/workspace` mount in a container.
    async fn expected_workdir(&self, harness: &TestHarness, session_id: &str) -> String {
        match self {
            Self::Local { profile_id } => {
                let sandbox = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
                    .sandbox_dir(session_id, Some(profile_id));
                tokio::fs::canonicalize(&sandbox)
                    .await
                    .expect("session sandbox directory should canonicalize")
                    .display()
                    .to_string()
            }
            Self::Container { .. } => "/workspace".to_string(),
        }
    }

    /// Releases what the session left outside its test home: a container
    /// session's container outlives the server that started it.
    async fn release(self, harness: &TestHarness, session_id: &str) {
        match self {
            Self::Local { .. } => {}
            Self::Container {
                profile_id,
                manager,
            } => {
                let (_store, db, _default_profile_id) =
                    initialize_session_persistence(&harness.may_dir)
                        .await
                        .expect("session persistence should initialize");
                let profile = db
                    .get_profile(&profile_id)
                    .await
                    .expect("profile lookup should succeed")
                    .expect("container profile should exist");
                let name = may_agent::container::profile::container_name_for_profile_session(
                    &profile,
                    &SessionId::from_string(session_id),
                );
                let container_id = manager
                    .resolve_container_id_by_name(&name)
                    .await
                    .expect("session container lookup should succeed")
                    .unwrap_or_else(|| panic!("session container {name} should exist"));
                manager
                    .remove(&container_id)
                    .await
                    .expect("session container should be removed");
            }
        }
    }
}

/// The concatenated system messages of a model request.
fn system_text(request: &Value) -> String {
    request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn create_session_via_api(
    client: &reqwest::Client,
    server: &RunningUiServer,
    profile_id: &str,
) -> String {
    let response = client
        .post(format!("{}/api/sessions", server.base_url))
        .json(&serde_json::json!({ "profileId": profile_id }))
        .send()
        .await
        .expect("create session request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
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

async fn profile_id_via_api(
    client: &reqwest::Client,
    server: &RunningUiServer,
    slug: &str,
) -> String {
    let response = client
        .get(format!("{}/api/profiles", server.base_url))
        .send()
        .await
        .expect("list profiles request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let profiles: Value = response
        .json()
        .await
        .expect("profiles response should be valid JSON");
    profiles
        .as_array()
        .expect("profiles response should be an array")
        .iter()
        .find(|profile| profile.get("slug").and_then(Value::as_str) == Some(slug))
        .and_then(|profile| profile.get("id").and_then(Value::as_str))
        .unwrap_or_else(|| panic!("default profile `{slug}` should exist"))
        .to_string()
}

async fn get_runtime(
    client: &reqwest::Client,
    server: &RunningUiServer,
    session_id: &str,
) -> Value {
    let response = client
        .get(format!(
            "{}/api/sessions/{session_id}/runtime",
            server.base_url
        ))
        .send()
        .await
        .expect("runtime request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
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
    let end = remainder.find(").")?;
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
    let heel_bin = heel_bin();

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
    let expected_workdir = tokio::fs::canonicalize(&session.sandbox_dir)
        .await
        .expect("session sandbox directory should canonicalize");
    assert_eq!(
        String::from_utf8_lossy(&pwd_output.stdout).trim(),
        expected_workdir.display().to_string()
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
    let heel_bin = heel_bin();

    let output = Command::new(binary)
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("sandboxed")
        .arg("--")
        .arg("perl -e 'print \"x\" x 60000'")
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
        stdout.contains("the full output is saved to outputs/"),
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
    assert_eq!(stored_contents.len(), 60000);
    assert!(stored_contents.chars().all(|ch| ch == 'x'));
}

#[tokio::test]
#[serial]
async fn ui_session_crud_round_trip_matches_api_contract() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let seeded_heel_state = harness.container_heel_state().await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();

    let create_response = client
        .post(format!("{}/api/sessions", server.base_url))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("create session request should succeed");
    let create_response = expect_status(&server.log, create_response, StatusCode::OK).await;
    // Any release-API check rewrites the state's timestamp; an untouched
    // state proves the default container profile took the seeded fast path.
    assert_eq!(
        harness.container_heel_state().await,
        seeded_heel_state,
        "session creation must reuse the seeded container heel without a release check"
    );
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
    let list_response = expect_status(&server.log, list_response, StatusCode::OK).await;
    let listed: Value = list_response
        .json()
        .await
        .expect("list sessions response should be valid JSON");
    let contains = listed
        .as_array()
        .expect("list sessions response should be an array")
        .iter()
        .filter_map(|session| session.get("id").and_then(Value::as_str))
        .any(|id| id == session_id.as_str());
    assert!(contains);

    let update_response = client
        .put(format!("{}/api/sessions/{session_id}", server.base_url))
        .json(&serde_json::json!({
            "title": "Terminal first test",
            "model": TEST_SELECTOR
        }))
        .send()
        .await
        .expect("update session request should succeed");
    let update_response = expect_status(&server.log, update_response, StatusCode::OK).await;
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
    let get_response = expect_status(&server.log, get_response, StatusCode::OK).await;
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
            .and_then(|runtime| runtime.get("state"))
            .and_then(Value::as_str),
        Some("idle"),
        "session without an active run should expose an idle runtime: {fetched}"
    );

    let delete_response = client
        .delete(format!("{}/api/sessions/{session_id}", server.base_url))
        .send()
        .await
        .expect("delete session request should succeed");
    expect_status(&server.log, delete_response, StatusCode::OK).await;

    let missing_response = client
        .get(format!("{}/api/sessions/{session_id}", server.base_url))
        .send()
        .await
        .expect("missing session request should succeed");
    expect_status(&server.log, missing_response, StatusCode::NOT_FOUND).await;

    server.stop().await;
}

/// `tasks.md` in the session directory is the single source of truth: a
/// model-side write streams a `todo_update` and is served by `GET /tasks`,
/// while `PUT /tasks` lands on disk where the next model request's tasks
/// injection reads it.
#[tokio::test]
#[serial]
async fn ui_chat_tasks_file_is_the_canonical_todo_document() {
    const MODEL_WRITTEN: &str = "- [x] write report\n- [x] ship it\n";
    const REWRITTEN: &str = "- [ ] rewritten plan\n";

    let mock = MockOpenAiServer::spawn(MockChatScenario::WritesTasksFile).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let sandbox_dir = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&session_id, Some(environment.profile_id()));

    let response = client
        .post(format!("{}/api/chat/stream", server.base_url))
        .json(&serde_json::json!({
            "sessionId": session_id,
            "content": "Plan the work in tasks.md."
        }))
        .send()
        .await
        .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = collect_turn_events(response, "the first tasks turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "chat stream should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );

    // The tool-end tasks check parsed the freshly written file and pushed
    // its checklist down the stream.
    let todo_update = events
        .iter()
        .find(|(kind, _)| kind == "todo_update")
        .expect("a tool call writing tasks.md should emit todo_update");
    assert_eq!(
        todo_update.1.get("items"),
        Some(&serde_json::json!([
            { "text": "write report", "checked": true },
            { "text": "ship it", "checked": false },
        ])),
        "todo_update should carry the parsed tasks.md items: {todo_update:?}"
    );

    // GET serves the file the model wrote.
    let tasks_url = format!("{}/api/sessions/{session_id}/tasks", server.base_url);
    let response = client
        .get(&tasks_url)
        .send()
        .await
        .expect("get tasks request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let payload: Value = response
        .json()
        .await
        .expect("tasks payload should be valid JSON");
    assert_eq!(
        payload.get("content").and_then(Value::as_str),
        Some(MODEL_WRITTEN)
    );

    // PUT lands on disk, where the next model request's injection reads it.
    let response = client
        .put(&tasks_url)
        .json(&serde_json::json!({ "content": REWRITTEN, "planState": "disabled" }))
        .send()
        .await
        .expect("put tasks request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let payload: Value = response
        .json()
        .await
        .expect("put tasks response should be valid JSON");
    assert_eq!(
        payload.get("content").and_then(Value::as_str),
        Some(REWRITTEN)
    );
    assert_eq!(
        payload.get("planState").and_then(Value::as_str),
        Some("disabled")
    );
    let on_disk = tokio::fs::read_to_string(sandbox_dir.join("tasks.md"))
        .await
        .expect("PUT should have written the canonical tasks.md");
    assert_eq!(on_disk, REWRITTEN);

    // A follow-up turn proves the injection reads the file: the rewritten
    // content shows up in the next model request.
    let response = client
        .post(format!("{}/api/chat/stream", server.base_url))
        .json(&serde_json::json!({
            "sessionId": session_id,
            "content": "Continue."
        }))
        .send()
        .await
        .expect("second chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = collect_turn_events(response, "the follow-up tasks turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "second chat stream should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );
    let requests = mock.agent_requests().await;
    let injected = requests
        .iter()
        .filter_map(tasks_injection)
        .any(|tasks| tasks.contains("rewritten plan"));
    assert!(
        injected,
        "the next model request should inject the PUT-written tasks.md"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

const PROBE_USER_TURN: &str = "Run the subagent authority probe.";

/// The session root's regular files, read before the session is released.
struct ProbeFiles(std::collections::BTreeMap<String, String>);

impl ProbeFiles {
    fn read(sandbox_dir: &Path) -> Self {
        let entries = std::fs::read_dir(sandbox_dir)
            .unwrap_or_else(|error| panic!("the session root should be readable: {error}"));
        Self(
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let text = std::fs::read_to_string(entry.path()).ok()?;
                    Some((name, text))
                })
                .collect(),
        )
    }

    fn get(&self, name: &str) -> &str {
        self.0.get(name).unwrap_or_else(|| {
            panic!(
                "the probe should have written {name}; session root files: {:?}",
                self.0.keys().collect::<Vec<_>>()
            )
        })
    }
}

/// Runs one chat turn of `probe` on the host path and returns the session
/// root's files, read before the session is released, with the mock.
async fn run_subagent_probe(probe: SubagentProbe) -> (ProbeFiles, MockOpenAiServer) {
    run_probe_turn(MockChatScenario::SubagentProbe(probe), PROBE_USER_TURN).await
}

/// Runs one chat turn of `scenario` with `content` as the user message.
async fn run_probe_turn(
    scenario: MockChatScenario,
    content: &str,
) -> (ProbeFiles, MockOpenAiServer) {
    let mock = MockOpenAiServer::spawn(scenario).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let sandbox_dir = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&session_id, Some(environment.profile_id()));

    let response = client
        .post(format!("{}/api/chat/stream", server.base_url))
        .json(&serde_json::json!({
            "sessionId": session_id,
            "content": content
        }))
        .send()
        .await
        .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = collect_turn_events(response, "the subagent probe turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the probe turn should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );
    let files = ProbeFiles::read(&sandbox_dir);
    server.stop().await;
    environment.release(&harness, &session_id).await;
    (files, mock)
}

/// Whether a model request is the subagent's: its first user message is the
/// prompt file, which carries [`SUBAGENT_PROBE_MARKER`]. Notices the main agent
/// receives later may quote the marker, so only the first user message counts.
fn is_subagent_request(request: &Value) -> bool {
    request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .is_some_and(|content| content.contains(SUBAGENT_PROBE_MARKER))
}

/// The latest tool output of every subagent request: what its command printed.
async fn child_probe_outputs(mock: &MockOpenAiServer) -> Vec<String> {
    mock.agent_requests()
        .await
        .iter()
        .filter(|request| is_subagent_request(request))
        .filter_map(latest_tool_output)
        .collect()
}

fn child_exit_code(outputs: &[String]) -> i32 {
    outputs
        .iter()
        .find_map(|output| {
            let start = output.find("CHILD_RC=")? + "CHILD_RC=".len();
            output[start..]
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or_else(|| panic!("the subagent's command printed no CHILD_RC: {outputs:?}"))
}

/// Whether a model request is a fork's first: the main agent's context
/// followed by the fork prompt as the last, user, message.
fn is_fork_request(request: &Value) -> bool {
    request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .is_some_and(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains(FORK_TASK_MARKER))
        })
}

/// A user turn long enough to put the main agent's context between the
/// refusal point (90% of the 60% compaction point) and the compaction point.
fn near_compaction_user_turn() -> String {
    format!(
        "Run the fork probe. {}",
        "lorem ipsum ".repeat(FORK_FILLER_REPEATS)
    )
}

const FORK_FILLER_REPEATS: usize = 24_000;

#[tokio::test]
#[serial]
async fn a_fork_continues_the_main_agents_context() {
    let (files, mock) = run_probe_turn(MockChatScenario::ForkProbe, "Run the fork probe.").await;

    let requests = mock.agent_requests().await;
    let forks: Vec<&Value> = requests.iter().filter(|r| is_fork_request(r)).collect();
    assert_eq!(
        forks.len(),
        1,
        "exactly one fork request expected: {requests:?}"
    );
    let fork = forks[0];
    let messages = fork["messages"].as_array().expect("fork request messages");
    let saw_secret = messages.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("tool")
            && message
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains(FORK_SECRET))
    });
    assert!(
        saw_secret,
        "the fork's first request must carry the command output only the main agent saw: {messages:?}"
    );
    let launch = requests
        .iter()
        .rev()
        .find(|r| {
            !is_fork_request(r) && latest_tool_output(r).is_some_and(|o| o.contains(FORK_SECRET))
        })
        .expect("the main agent's request that launched the fork");
    let launch_messages = launch["messages"].as_array().expect("launch messages");
    assert_eq!(
        &messages[..launch_messages.len()],
        launch_messages.as_slice(),
        "the fork must start from the main agent's last request"
    );
    assert!(
        files.get("sub.out").contains(FORK_ANSWER),
        "the fork's answer must come back as the command's output: {}",
        files.get("sub.out")
    );
}

#[tokio::test]
#[serial]
async fn a_fork_near_the_compaction_point_returns_the_compact_first_error() {
    let (files, mock) =
        run_probe_turn(MockChatScenario::ForkProbe, &near_compaction_user_turn()).await;

    let requests = mock.agent_requests().await;
    assert!(
        !requests.iter().any(is_fork_request),
        "no fork request may be sent when the fork is refused"
    );
    let result = files.get("sub.out");
    assert!(
        result.contains("Forking is refused") && result.contains("Run `compact` first"),
        "the compact-first refusal must be the command's error: {result}"
    );
    let main_saw_refusal = requests.iter().any(|request| {
        latest_tool_output(request).is_some_and(|output| {
            output.contains("POLL_DONE") && output.contains("Run `compact` first")
        })
    });
    assert!(
        main_saw_refusal,
        "the main agent's request must carry the refusal as the command's output"
    );
}

const WORKTREE_SETUP: &str = "git init -q -b main repo && git -C repo -c user.name=m -c user.email=m@e commit -q --allow-empty -m init && git -C repo worktree add -q -b subagent/s1/work \"$PWD/wt\" && git -C repo rev-parse subagent/s1/work > tip.before";
const WORKTREE_FINAL: &str = "git -C repo rev-parse subagent/s1/work > tip.after; echo MAIN_FINAL";
const CHILD_COMMIT: &str = "cd ../../wt && echo x > f.txt && echo WROTE_FILE && git add f.txt && git -c user.name=c -c user.email=c@e commit -q -m 'child commit'; echo CHILD_RC=$?";

#[tokio::test]
#[serial]
async fn subagent_cannot_write_tasks_md_but_the_main_agent_can() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: "printf -- '- [x] original\\n' > tasks.md",
        grant_flags: "",
        child_script: "printf -- '- [x] hacked\\n' >> ../../tasks.md; echo CHILD_RC=$?",
        final_script: "cp tasks.md tasks.before; printf -- '- [x] original\\n- [x] main wrote\\n' > tasks.md; echo MAIN_FINAL",
    })
    .await;

    let outputs = child_probe_outputs(&mock).await;
    assert_ne!(
        child_exit_code(&outputs),
        0,
        "the subagent's write to tasks.md must fail: {outputs:?}"
    );
    assert_eq!(
        files.get("tasks.before"),
        "- [x] original\n",
        "tasks.md changed while only the subagent had run"
    );
    assert_eq!(
        files.get("tasks.md"),
        "- [x] original\n- [x] main wrote\n",
        "the main agent's write in the same session must succeed"
    );
}

#[tokio::test]
#[serial]
async fn subagent_without_a_commit_grant_cannot_commit_in_a_worktree() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: WORKTREE_SETUP,
        grant_flags: "--worktrees \"$PWD/wt\"",
        child_script: CHILD_COMMIT,
        final_script: WORKTREE_FINAL,
    })
    .await;

    let outputs = child_probe_outputs(&mock).await;
    assert_ne!(
        child_exit_code(&outputs),
        0,
        "an unauthorized git commit must fail: {outputs:?}"
    );
    assert!(
        outputs.iter().any(|output| output.contains("WROTE_FILE")),
        "the worktree is writable, so the failure is the commit's: {outputs:?}"
    );
    assert_eq!(
        files.get("tip.before"),
        files.get("tip.after"),
        "the branch moved although the subagent had no commit grant"
    );
    let result = files.get("sub.out");
    assert!(
        !result.contains("Commits made in the granted worktrees"),
        "a subagent without a commit grant reports no commits: {result}"
    );
}

#[tokio::test]
#[serial]
async fn subagent_with_a_commit_grant_commits_and_the_main_agent_gets_the_listing() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: WORKTREE_SETUP,
        grant_flags: "--commit-worktrees \"$PWD/wt\"",
        child_script: CHILD_COMMIT,
        final_script: WORKTREE_FINAL,
    })
    .await;

    let outputs = child_probe_outputs(&mock).await;
    assert_eq!(
        child_exit_code(&outputs),
        0,
        "the authorized git commit must succeed: {outputs:?}"
    );
    let before = files.get("tip.before");
    let after = files.get("tip.after");
    assert_ne!(before, after, "the branch should have moved");

    let result = files.get("sub.out");
    assert!(
        result.contains("Commits made in the granted worktrees:")
            && result.contains("child commit")
            && result.contains(&after.trim()[..7]),
        "the subagent result lists the commit: {result}"
    );
    // The main agent saw that listing in the tool output it was given.
    let main_saw_listing = mock.agent_requests().await.iter().any(|request| {
        latest_tool_output(request).is_some_and(|output| {
            output.contains("POLL_DONE")
                && output.contains("Commits made in the granted worktrees:")
        })
    });
    assert!(
        main_saw_listing,
        "no main-agent request carried the commit listing"
    );
}

/// A 500's response body is redacted, so the server log is the only place its
/// cause survives: the failing request must be logged at error level with the
/// underlying error.
#[tokio::test]
#[serial]
async fn ui_server_error_logs_its_cause() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let profile_id =
        profile_id_via_api(&client, &server, ChatEnvironment::Local.profile_slug()).await;

    // A regular file where the profile's sessions directory belongs makes
    // session directory creation fail with an I/O error.
    let sessions_dir = MayHomePaths::new(harness.may_dir.clone())
        .profile_home(&profile_id)
        .sessions_dir();
    if tokio::fs::try_exists(&sessions_dir)
        .await
        .expect("sessions dir existence should be checkable")
    {
        tokio::fs::remove_dir_all(&sessions_dir)
            .await
            .expect("existing sessions dir should be removable");
    }
    tokio::fs::write(&sessions_dir, b"not a directory")
        .await
        .expect("sessions dir placeholder file should be written");

    let response = client
        .post(format!("{}/api/sessions", server.base_url))
        .json(&serde_json::json!({ "profileId": profile_id }))
        .send()
        .await
        .expect("create session request should succeed");
    let response = expect_status(&server.log, response, StatusCode::INTERNAL_SERVER_ERROR).await;
    let body: Value = response
        .json()
        .await
        .expect("error response should be valid JSON");
    assert_eq!(
        body,
        serde_json::json!({ "error": "Internal server error" })
    );

    let logs = harness.file_logs().await;
    let logged = logs.lines().any(|line| {
        line.contains("ERROR")
            && line.contains("internal server error")
            && line.contains("path=\"/api/sessions\"")
            && line.contains("status=500")
            && line.contains("IO error:")
    });
    assert!(
        logged,
        "the 500 should be logged with its cause; logs:\n{logs}"
    );

    server.stop().await;
}

#[tokio::test]
#[serial]
async fn ui_stream_stop_mid_response_persists_partial_assistant_text() {
    stream_stop_mid_response_persists_partial_assistant_text(ChatEnvironment::Local).await;
}

#[tokio::test]
#[serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn ui_stream_stop_mid_response_persists_partial_assistant_text_in_container() {
    stream_stop_mid_response_persists_partial_assistant_text(ChatEnvironment::Container).await;
}

async fn stream_stop_mid_response_persists_partial_assistant_text(environment: ChatEnvironment) {
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
    let environment = environment.prepare(&harness, &client, &server).await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let chat_task = tokio::spawn({
        let client = client.clone();
        let base_url = server.base_url.clone();
        let log = server.log.clone();
        let session_id = session_id.clone();
        async move {
            let response = client
                .post(format!("{base_url}/api/chat/stream"))
                .json(&serde_json::json!({
                    "sessionId": session_id,
                    "content": "Say several chunks slowly."
                }))
                .send()
                .await
                .expect("chat stream request should succeed");
            let response = expect_status(&log, response, StatusCode::OK).await;
            collect_sse_events(response).await
        }
    });

    // The session title request can arrive first, while the agent turn is
    // still setting up its environment; the stop must land mid-turn.
    let agent_request_timeout = Duration::from_secs(10) + environment.first_turn_allowance();
    let deadline = Instant::now() + agent_request_timeout;
    loop {
        if !mock.agent_requests().await.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "agent turn request did not reach mock server within {agent_request_timeout:?}; server logs:\n{}",
            server.log.contents().await
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
    expect_status(&server.log, stop_response, StatusCode::OK).await;

    let events = tokio::time::timeout(Duration::from_secs(15), chat_task)
        .await
        .expect("cancelled chat stream should terminate promptly")
        .expect("chat task should join");
    assert!(
        events.iter().any(|(kind, _)| kind == "text"),
        "stream should deliver partial text before cancellation; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );

    wait_for_runtime_state_clear(&client, &server, &session_id, Duration::from_secs(10)).await;

    let session_response = client
        .get(format!("{}/api/sessions/{}", server.base_url, session_id))
        .send()
        .await
        .expect("get session after cancel should succeed");
    let session_response = expect_status(&server.log, session_response, StatusCode::OK).await;
    let session_payload: Value = session_response
        .json()
        .await
        .expect("session payload after cancel should be valid JSON");
    let assistant_messages = extract_assistant_messages(&session_payload);
    assert!(
        assistant_messages
            .iter()
            .any(|content| content.contains("alpha")),
        "partial assistant text should be persisted after cancellation: {assistant_messages:?}"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

#[tokio::test]
#[serial]
async fn ui_chat_executes_terminal_and_terminal_input_round_trip() {
    terminal_and_terminal_input_round_trip(ChatEnvironment::Local).await;
}

/// The round trip runs its commands inside the session's container — they
/// start in the `/workspace` mount and reach host IPC commands through the
/// container's bridge route — and the model sees the container environment
/// context.
#[tokio::test]
#[serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn ui_chat_executes_terminal_and_terminal_input_round_trip_in_container() {
    terminal_and_terminal_input_round_trip(ChatEnvironment::Container).await;
}

async fn terminal_and_terminal_input_round_trip(environment: ChatEnvironment) {
    let mock = MockOpenAiServer::spawn(MockChatScenario::TerminalRoundTrip).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = environment.prepare(&harness, &client, &server).await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let chat_task = tokio::spawn({
        let client = client.clone();
        let base_url = server.base_url.clone();
        let log = server.log.clone();
        let session_id = session_id.clone();
        async move {
            let response = client
                .post(format!("{base_url}/api/chat/stream"))
                .json(&serde_json::json!({
                    "sessionId": session_id,
                    "content": "Please ask the terminal for my name and answer with the result."
                }))
                .send()
                .await
                .expect("chat stream request should succeed");
            let response = expect_status(&log, response, StatusCode::OK).await;
            collect_sse_events(response).await
        }
    });

    let runtime_during_background = wait_for_runtime_condition(
        &client,
        &server,
        &session_id,
        Duration::from_secs(10) + environment.first_turn_allowance(),
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

    let events = tokio::time::timeout(
        Duration::from_secs(60) + environment.first_turn_allowance(),
        chat_task,
    )
    .await
    .expect("chat stream should finish before timeout")
    .expect("chat task should join");
    let server_logs = server.log.contents().await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "terminal round-trip chat should complete with a done event; events: {events:?}; server logs:\n{server_logs}"
    );

    let assistant_text: String = events
        .iter()
        .filter(|(kind, _)| kind == "text")
        .filter_map(|(_, data)| data.get("chunk").and_then(Value::as_str))
        .collect();
    assert!(
        assistant_text.contains("terminal flow complete"),
        "assistant text should include the final answer: {assistant_text:?}"
    );

    assert!(
        events.iter().any(|(kind, data)| {
            kind == "tool_start"
                && data
                    .get("arguments")
                    .and_then(Value::as_str)
                    .is_some_and(|arguments| arguments.contains("read name"))
        }),
        "chat stream should record the terminal execution; events: {events:?}"
    );

    wait_for_runtime_condition(
        &client,
        &server,
        &session_id,
        Duration::from_secs(10),
        "terminal runtime to clear after background completion",
        runtime_is_idle,
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

    let agent_requests = mock.agent_requests().await;
    let context_heading = environment.context_heading();
    let first_request_system = system_text(
        agent_requests
            .first()
            .expect("the turn should have asked the model"),
    );
    assert!(
        first_request_system.contains(context_heading),
        "the model request should carry the `{context_heading}` environment context; system text:\n{first_request_system}"
    );
    let terminal_output = agent_requests
        .last()
        .and_then(latest_tool_output)
        .expect("the final model request should carry the terminal read output");
    let expected_workdir = environment.expected_workdir(&harness, &session_id).await;
    let expected_line = format!("cwd={expected_workdir}");
    assert!(
        terminal_output.lines().any(|line| line == expected_line),
        "terminal commands should start in {expected_workdir}; terminal output: {terminal_output:?}"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

#[tokio::test]
#[serial]
async fn ui_chat_turns_on_one_session_prepare_tools_once() {
    chat_turns_on_one_session_prepare_tools_once(ChatEnvironment::Local).await;
}

#[tokio::test]
#[serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn ui_chat_turns_on_one_session_prepare_tools_once_in_container() {
    chat_turns_on_one_session_prepare_tools_once(ChatEnvironment::Container).await;
}

async fn chat_turns_on_one_session_prepare_tools_once(environment: ChatEnvironment) {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::TextEveryTurn("turn answered".to_string())).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = environment.prepare(&harness, &client, &server).await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    for (content, turn_timeout) in [
        (
            "First turn.",
            Duration::from_secs(60) + environment.first_turn_allowance(),
        ),
        ("Second turn.", Duration::from_secs(60)),
    ] {
        let response = client
            .post(format!("{}/api/chat/stream", server.base_url))
            .json(&serde_json::json!({
                "sessionId": session_id,
                "content": content
            }))
            .send()
            .await
            .expect("chat stream request should succeed");
        let status = response.status();
        assert!(
            status == StatusCode::OK,
            "{content:?} was refused with {status}: {}; server logs:\n{}",
            response
                .text()
                .await
                .expect("refusal body should be readable"),
            server.log.contents().await
        );
        let events = tokio::time::timeout(turn_timeout, collect_sse_events(response))
            .await
            .expect("chat stream should finish before timeout");
        assert!(
            events.iter().any(|(kind, _)| kind == "done"),
            "turn should complete with a done event; events: {events:?}; server logs:\n{}",
            server.log.contents().await
        );
        wait_for_runtime_condition(
            &client,
            &server,
            &session_id,
            Duration::from_secs(10),
            "session runtime to settle after the turn",
            runtime_is_idle,
        )
        .await;
    }

    let logs = harness.file_logs().await;
    let ready_lines = logs
        .lines()
        .filter(|line| line.contains("session tool environments ready"))
        .filter(|line| line.contains(session_id.as_str()))
        .collect::<Vec<_>>();
    let prepared = ready_lines
        .iter()
        .filter(|line| line.contains("outcome=\"prepared\""))
        .count();
    let adopted = ready_lines
        .iter()
        .filter(|line| line.contains("outcome=\"adopted\""))
        .count();
    assert!(
        prepared == 1 && adopted >= 1,
        "only the first turn should prepare tools and later turns adopt them; readiness logs: {ready_lines:#?}"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
#[serial]
async fn ui_linux_runtime_has_heel_env_for_sandbox_stdin_detection() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::TerminalRoundTrip).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;

    server.stop().await;
}

/// Reads the kernel's view of a process on macOS: prints `parentNice
/// parentIO childNice childIO` where the child is an exec'd copy of itself, so
/// a run shows both the ambient priority and whether descendants inherit it.
#[cfg(target_os = "macos")]
const PRIORITY_PROBE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/priority_probe.rs"
);

/// The Linux counterpart of the macOS probe, printing the same four values.
/// It is a shell script over system tools because the Linux sandbox executes
/// only from system directories, never a binary the test compiled.
#[cfg(target_os = "linux")]
const PRIORITY_PROBE_SCRIPT: &str = include_str!("fixtures/priority_probe.sh");

/// The shell command that prints `parentNice parentIO childNice childIO` for
/// the process it runs as and for a child of it, the I/O value in the
/// platform's kernel encoding. The macOS probe reads the disk I/O policy,
/// which no system tool exposes, so it is compiled into the shared test-tmp
/// root: ordinary read-only executable storage to the macOS sandbox, unlike
/// the session-writable roots where exec is explicitly denied.
#[cfg(target_os = "macos")]
async fn priority_probe_command() -> String {
    let probe_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("priority-probe");
    async_fs::create_dir_all(&probe_dir)
        .await
        .expect("probe directory should be created");
    let probe_binary = probe_dir.join("priority-probe");
    let rustc_output = probe_binary.clone();
    let compile = blocking::unblock(move || {
        std::process::Command::new("rustc")
            .arg("--edition")
            .arg("2024")
            .arg("-O")
            .arg(PRIORITY_PROBE_PATH)
            .arg("-o")
            .arg(rustc_output)
            .output()
    })
    .await
    .expect("rustc should run");
    assert!(
        compile.status.success(),
        "probe compile failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    shell_escape::escape(probe_binary.display().to_string().into()).into_owned()
}

/// See the macOS variant: on Linux the probe is the script itself, run by `sh`
/// so its POSIX word splitting holds whatever shell the terminal tool uses.
#[cfg(target_os = "linux")]
fn priority_probe_command() -> std::future::Ready<String> {
    let script = shell_escape::escape(PRIORITY_PROBE_SCRIPT.into());
    std::future::ready(format!("sh -c {script}"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_priority_probe(stdout: &[u8]) -> [i32; 4] {
    let text = String::from_utf8_lossy(stdout);
    let [a, b, c, d] = text
        .split_whitespace()
        .map(|token| token.parse::<i32>().expect("probe prints signed integers"))
        .collect::<Vec<i32>>()
        .try_into()
        .expect("probe prints exactly four integers");
    [a, b, c, d]
}

#[cfg(target_os = "macos")]
const IO_UTILITY: i32 = 4; // IOPOL_UTILITY
#[cfg(target_os = "macos")]
const IO_BACKGROUND: i32 = 3; // IOPOL_THROTTLE
#[cfg(target_os = "linux")]
const IO_UTILITY: i32 = (2 << 13) | 7; // IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, 7)
#[cfg(target_os = "linux")]
const IO_BACKGROUND: i32 = 3 << 13; // IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0)

/// `may exec` under each configured command priority must apply the mapped
/// CPU nice level and I/O policy, and the probe's own child must inherit both.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
#[serial]
async fn exec_applies_local_command_priority() {
    let heel_bin = heel_bin();
    let probe = priority_probe_command().await;

    // Host baseline first, through a shell exactly as the sandbox runs it: an
    // unsandboxed probe's child inherits its priorities.
    let probe_for_baseline = probe.clone();
    let baseline_output = blocking::unblock(move || {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(probe_for_baseline)
            .output()
    })
    .await
    .expect("host probe should run");
    assert!(
        baseline_output.status.success(),
        "host probe failed: {}",
        String::from_utf8_lossy(&baseline_output.stderr)
    );
    let baseline = parse_priority_probe(&baseline_output.stdout);
    let [baseline_nice, baseline_io] = [baseline[0], baseline[1]];
    assert_eq!(
        [baseline[2], baseline[3]],
        [baseline_nice, baseline_io],
        "the probe's own child must inherit its priorities"
    );

    // Independent kernel expectations per configured level — never derived
    // from the mapping under test.
    let cases: [(CommandPriority, i32, i32); 3] = [
        (CommandPriority::Normal, baseline_nice, baseline_io),
        (
            CommandPriority::Low,
            (baseline_nice + 10).min(19),
            IO_UTILITY,
        ),
        (
            CommandPriority::Lowest,
            (baseline_nice + 19).min(19),
            IO_BACKGROUND,
        ),
    ];

    // The provider URL is unreachable by design: `may exec` reaches the
    // terminal tool directly and never talks to a model.
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let binary = may_binary_path();

    for (priority, expected_nice, expected_io) in cases {
        let expected = [expected_nice, expected_io, expected_nice, expected_io];

        let mut config = MayConfig::load_from(&harness.may_dir.join("config.toml"))
            .await
            .expect("harness config should load");
        config.sandbox.command_priority = priority;
        config
            .save_to(&harness.may_dir.join("config.toml"))
            .await
            .expect("harness config should save");

        let session = harness.create_session().await;

        let script = probe.clone();
        let session_id = session.id.clone();
        let home = harness.home_path().to_path_buf();
        let heel = heel_bin.clone();
        let binary_path = binary.clone();
        let output = blocking::unblock(move || {
            std::process::Command::new(&binary_path)
                .arg("exec")
                .arg("--session")
                .arg(&session_id)
                .arg("--mode")
                .arg("sandboxed")
                .arg("--")
                .arg(&script)
                .env("HOME", &home)
                .env("HEEL_BIN", &heel)
                .output()
        })
        .await
        .expect("may exec should run");
        assert!(
            output.status.success(),
            "may exec at {priority:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let observed = parse_priority_probe(&output.stdout);

        assert_eq!(
            observed,
            expected,
            "{priority:?} must map to nice={expected_nice}, io={expected_io}: observed={observed:?} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// A chat turn against a provider nobody listens on must end with the
/// provider's error on the stream, and the server must survive it.
#[tokio::test]
#[serial]
async fn ui_chat_reports_unreachable_provider_error() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let mut server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let response = client
        .post(format!("{}/api/chat/stream", server.base_url))
        .json(&serde_json::json!({
            "sessionId": session_id,
            "content": "Is anyone there?"
        }))
        .send()
        .await
        .expect("chat stream request should succeed");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat stream should open; server logs:\n{}",
        server.log.contents().await
    );
    let events = tokio::time::timeout(Duration::from_secs(60), collect_sse_events(response))
        .await
        .expect("chat stream should finish before timeout");

    let exit = server
        .child
        .try_wait()
        .expect("ui server status should be readable");
    assert!(
        exit.is_none(),
        "ui server exited with {exit:?} during the failing turn; server logs:\n{}",
        server.log.contents().await
    );
    let Some((_, error)) = events.iter().find(|(kind, _)| kind == "error") else {
        panic!("turn should end with an error event; events: {events:?}");
    };
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .expect("error event should carry a message");
    assert!(
        message.contains("127.0.0.1:9"),
        "error event should report the unreachable provider: {error}"
    );

    server.stop().await;
}

/// Upper bound on one container-profile task run, including its container
/// start.
const CONTAINER_TASK_RUN_TIMEOUT: Duration = Duration::from_secs(180);

/// Which script source a task under test uses.
#[derive(Debug, Clone, Copy)]
enum TaskScriptSource {
    Inline,
    File,
}

/// A container-profile task runs its script and reads its trigger file, so
/// the script and trigger paths in the command resolve inside the container.
async fn container_task_reads_its_trigger_file(source: TaskScriptSource) {
    let mock = MockOpenAiServer::spawn(MockChatScenario::TextEveryTurn("unused".to_string())).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningUiServer::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Container
        .prepare(&harness, &client, &server)
        .await;

    let script = r#"cat "$MAY_TASK_TRIGGER_FILE""#;
    let script_dir = tempfile::tempdir().expect("script dir should be created");
    let script_fields = match source {
        TaskScriptSource::Inline => serde_json::json!({ "scriptInline": script }),
        TaskScriptSource::File => {
            let path = script_dir.path().join("trigger-reader.sh");
            tokio::fs::write(&path, script)
                .await
                .expect("script file should be written");
            serde_json::json!({ "scriptPath": path.display().to_string() })
        }
    };
    let mut body = serde_json::json!({
        "profileId": environment.profile_id(),
        "name": "container trigger reader",
        "trigger": { "kind": "event", "eventName": "e2e.container.trigger" },
        "timeoutSecs": 120,
    });
    body.as_object_mut()
        .expect("task body is an object")
        .extend(
            script_fields
                .as_object()
                .expect("script fields are an object")
                .clone(),
        );

    let response = client
        .post(format!("{}/api/tasks", server.base_url))
        .json(&body)
        .send()
        .await
        .expect("create task request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let created: Value = response.json().await.expect("task response should be JSON");
    let task_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("task response should include id")
        .to_string();

    let response = client
        .post(format!("{}/api/tasks/{task_id}/run", server.base_url))
        .send()
        .await
        .expect("run task request should succeed");
    expect_status(&server.log, response, StatusCode::OK).await;

    let deadline = Instant::now() + CONTAINER_TASK_RUN_TIMEOUT;
    let task = loop {
        let response = client
            .get(format!("{}/api/tasks/{task_id}", server.base_url))
            .send()
            .await
            .expect("get task request should succeed");
        let response = expect_status(&server.log, response, StatusCode::OK).await;
        let task: Value = response.json().await.expect("task response should be JSON");
        let status = task.get("lastRunStatus").and_then(Value::as_str);
        if matches!(status, Some("success" | "failed")) {
            break task;
        }
        assert!(
            Instant::now() < deadline,
            "task did not finish within {CONTAINER_TASK_RUN_TIMEOUT:?}; task: {task}; server logs:\n{}",
            server.log.contents().await
        );
        sleep(Duration::from_millis(500)).await;
    };

    assert_eq!(
        task.get("lastRunStatus").and_then(Value::as_str),
        Some("success"),
        "the task should succeed; task: {task}; server logs:\n{}",
        server.log.contents().await
    );
    let output = task
        .get("lastRunOutput")
        .and_then(Value::as_str)
        .expect("a finished task should record its output");
    assert!(
        output.contains(&task_id),
        "the script should print the trigger file, which names the task; output: {output}"
    );

    server.stop().await;
    if let PreparedEnvironment::Container {
        profile_id,
        manager,
    } = environment
    {
        let (_store, db, _default_profile_id) = initialize_session_persistence(&harness.may_dir)
            .await
            .expect("session persistence should initialize");
        let profile = db
            .get_profile(&profile_id)
            .await
            .expect("profile lookup should succeed")
            .expect("container profile should exist");
        let name = format!("may-{}", profile.slug.as_deref().unwrap_or(&profile.id));
        let container_id = manager
            .resolve_container_id_by_name(&name)
            .await
            .expect("task container lookup should succeed")
            .unwrap_or_else(|| panic!("task container {name} should exist"));
        manager
            .remove(&container_id)
            .await
            .expect("task container should be removed");
    }
}

#[tokio::test]
#[serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn container_profile_inline_script_task_reads_its_trigger_file() {
    container_task_reads_its_trigger_file(TaskScriptSource::Inline).await;
}

#[tokio::test]
#[serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn container_profile_script_file_task_reads_its_trigger_file() {
    container_task_reads_its_trigger_file(TaskScriptSource::File).await;
}
