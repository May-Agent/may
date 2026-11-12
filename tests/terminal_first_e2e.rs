//! End-to-end tests for May's terminal-first execution and REST/SSE contracts.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

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
use may_agent::config::{ContextConfig, ProviderInstance, ProviderType};
use may_agent::db::{CreateProfileParams, ProfileEnvironmentKind};
use may_agent::model_selector::ModelSelector;
use may_agent::session::{
    SandboxResourceLayout, SessionId, SessionStore, initialize_session_persistence,
};
use may_agent::tools::ToolTarget;
use may_agent::{MayConfig, MayHomePaths};
use serde_json::Value;
use serial_test::file_serial;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};

const TEST_PROVIDER_ID: &str = "mock";
const TEST_MODEL: &str = "test-model";
const TIER_FLAGSHIP_MODEL: &str = "tier-flagship";
const TIER_BALANCED_MODEL: &str = "tier-balanced";
const TIER_FAST_MODEL: &str = "tier-fast";
const TEST_SELECTOR: &str = "mock::test-model";
const TEST_API_KEY: &str = "test-api-key";
const TITLE_REQUEST_PREFIX: &str =
    "Generate a short, descriptive title (3-6 words) for this conversation.";

#[derive(Template)]
#[template(path = "test_terminal_read.sh", escape = "none")]
struct TerminalReadScript<'a> {
    output_file: &'a str,
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
    /// Answers agent request `n` with a text reply whose usage reports
    /// `cached[n]` prompt tokens read from cache (the last entry repeats).
    CachedTextEveryTurn {
        cached: Vec<u32>,
    },
    /// Answers every agent turn with `chunks`, each streamed `delay` after
    /// the one before.
    SlowTextEveryTurn {
        chunks: Vec<String>,
        delay: Duration,
    },
    /// The first `failures` agent requests are refused with an overloaded
    /// (529) status; every later one gets `text`.
    OverloadedThenText {
        failures: usize,
        text: String,
    },
    /// First turn writes `tasks.md` through the terminal tool; every later
    /// request gets a plain text reply, so follow-up turns can probe what
    /// the model side injects from the file.
    WritesTasksFile,
    /// The main agent launches a real subagent from a terminal command and a
    /// scripted child runs one probe; see [`SubagentProbe`].
    SubagentProbe(SubagentProbe),
    /// The main agent prints a secret, then forks itself with
    /// `subagent fork`; the fork answers with [`FORK_ANSWER`].
    ForkProbe,
    /// The first request asks the user a question through the `ask_user`
    /// terminal command; every later request gets a plain text reply.
    AsksUser,
    /// The first request starts the script as a background terminal job;
    /// every later request gets a plain text reply.
    StartsBackgroundJob(&'static str),
    /// The first request runs the script as a foreground terminal command and
    /// the next replies with text, failing the turn when the command's result
    /// reports a denied operation.
    RunsScript(&'static str),
    /// A turn that runs [`COMPACTION_PROBE_STEPS`] commands over a
    /// context past the compaction start point, then replies; a turn that
    /// follows a restart is answered at once. See [`CompactionProbe`].
    CompactionProbe(CompactionProbe),
    /// Against the fake MCP server of
    /// `tests/fixtures/changing_tools_mcp_server.py`: the first turn runs
    /// `probe_add`, which makes the server add `probe_greet`; the second
    /// runs `probe_greet`; every other request gets a plain text reply.
    AddsMcpTool,
}

/// How the mock answers the compaction requests of [`MockChatScenario::CompactionProbe`].
#[derive(Clone, Copy)]
enum CompactionProbe {
    /// Every compaction is answered with a valid handoff once the run has
    /// asked the model again, or after [`COMPACTION_PROBE_GATE_LIMIT`].
    Lands,
    /// Every compaction is answered with a valid handoff at once, for a
    /// compaction nothing else in flight would release.
    LandsAtOnce,
    /// The compaction and the retry aither makes on a bad answer get
    /// something that is no handoff; the rerun on the cleaned context gets a
    /// valid one.
    FailsThenLands,
}

/// Commands a compaction probe's turn runs before it replies.
const COMPACTION_PROBE_STEPS: usize = 5;
/// How long the mock takes to ask for each command.
const COMPACTION_PROBE_STEP_DELAY: Duration = Duration::from_secs(1);
/// How long the mock holds a landing compaction's answer back when the run
/// never asks the model again, so such a run fails its assertion instead of
/// hanging.
const COMPACTION_PROBE_GATE_LIMIT: Duration = Duration::from_secs(60);
/// The task the probe's handoff records; only a compacted context holds it.
const COMPACTION_PROBE_TASK: &str = "COMPACTED-PROBE-TASK-5521";
/// The user message that follows the restart.
const COMPACTION_PROBE_FOLLOW_UP: &str = "second turn after the restart";

/// A background job that outlives every test turn.
const BACKGROUND_JOB_SCRIPT: &str = "sleep 120";
/// What [`FINISHING_JOB_SCRIPT`] prints once it is done.
const FINISHING_JOB_MARKER: &str = "BG-DONE-7731";
/// A background job that finishes a moment after its turn ended.
const FINISHING_JOB_SCRIPT: &str = "sleep 2; echo BG-DONE-7731";

/// A scripted main agent / subagent pair sharing one mock model.
///
/// The main agent runs `setup`, then `launch` (shell lines that start
/// subagents with `prompt.md` as the brief) in a background job and ends its
/// turn; the job's completion resumes it as a reminder, it reads the launch's
/// output, then runs `final_script`. Each child runs `child_script` once and
/// ends. A request is the child's when a user or system message carries
/// [`SUBAGENT_PROBE_MARKER`], which only the subagent's prompt file does.
#[derive(Clone)]
struct SubagentProbe {
    setup: &'static str,
    launch: &'static str,
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
const SUBAGENT_PROBE_MAX_REQUESTS: usize = 200;

/// Prints the launch job's result once its completion resumed the turn —
/// the job only writes `sub.done`/`sub.out` after it exits.
const SUBAGENT_READ_SCRIPT: &str = "echo POLL_DONE; cat sub.out";

/// How long a probe may wait for the turn a finished launch job resumes.
const SUBAGENT_PROBE_DONE_TIMEOUT: Duration = Duration::from_secs(60);

const ASK_USER_SCRIPT: &str =
    "ask_user --question 'Which database?' --option PostgreSQL --option SQLite";

/// Holds a landing compaction's answer until the run asks the model again,
/// or for [`COMPACTION_PROBE_GATE_LIMIT`].
#[derive(Default)]
struct CompactionGate {
    release: tokio::sync::Notify,
    /// Set when the limit passed first and the compaction landed unasked.
    timed_out: std::sync::atomic::AtomicBool,
}

impl CompactionGate {
    async fn hold(&self) {
        if tokio::time::timeout(COMPACTION_PROBE_GATE_LIMIT, self.release.notified())
            .await
            .is_err()
        {
            self.timed_out
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Releases the answer; whether it was still held back.
    fn release(&self) -> bool {
        self.release.notify_one();
        !self.timed_out.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Holds one agent request until the test releases it, so the test can act
/// while a run is in flight.
struct RequestGate {
    /// Zero-based index among agent requests (titles excluded) to hold.
    held_index: usize,
    seen: std::sync::atomic::AtomicUsize,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl RequestGate {
    fn holding(held_index: usize) -> Arc<Self> {
        Arc::new(Self {
            held_index,
            seen: std::sync::atomic::AtomicUsize::new(0),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }

    async fn hold_if_selected(&self) {
        let index = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if index == self.held_index {
            self.arrived.notify_one();
            self.release.notified().await;
        }
    }
}

struct MockOpenAiServer {
    addr: SocketAddr,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
    state: Arc<tokio::sync::Mutex<MockScenarioState>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockOpenAiServer {
    async fn spawn(scenario: MockChatScenario) -> Self {
        Self::spawn_gated(scenario, None).await
    }

    async fn spawn_gated(scenario: MockChatScenario, gate: Option<Arc<RequestGate>>) -> Self {
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
        let state_for_task = state.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let io = TokioIo::new(stream);
                let state = state_for_task.clone();
                let requests = requests_for_task.clone();
                let transcript = transcript.clone();
                let gate = gate.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        handle_mock_openai_request(
                            request,
                            state.clone(),
                            requests.clone(),
                            transcript.clone(),
                            gate.clone(),
                        )
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });
        Self {
            addr,
            requests,
            state,
            handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    /// Waits until a probe script served its final response — the last steps
    /// run on the turn the finished launch job resumes, after the turn that
    /// launched it already ended.
    async fn wait_probe_done(&self, what: &str, log: &ServerLog) {
        let done = tokio::time::timeout(SUBAGENT_PROBE_DONE_TIMEOUT, async {
            loop {
                if self.state.lock().await.probe_done {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        if done.is_err() {
            let signatures = self
                .agent_requests()
                .await
                .iter()
                .enumerate()
                .map(|(index, request)| {
                    format!(
                        "#{index} job_finished={} subagent={} fork={} latest={:?} last_msg={:?}",
                        launch_job_finished(request),
                        is_subagent_request(request),
                        is_fork_request(request),
                        latest_tool_output(request)
                            .map(|output| output.chars().take(60).collect::<String>()),
                        request
                            .get("messages")
                            .and_then(Value::as_array)
                            .and_then(|messages| messages.last())
                            .map(|message| {
                                format!("{:.300}", message.get("content").unwrap_or(&Value::Null))
                            })
                    )
                })
                .collect::<Vec<_>>();
            panic!(
                "{what} never finished its resumed turn; requests: {signatures:?}; server logs:\n{}",
                strip_ansi_escapes(&log.contents().await)
            );
        }
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
    output_file: Option<String>,
    /// Set once a probe script served its final response, so the test can
    /// wait for a turn the job's completion resumed on its own.
    probe_done: bool,
    /// Commands a compaction probe has asked for.
    probe_steps: usize,
    /// Compaction requests a compaction probe received.
    compactions: usize,
    /// Releases the answer of a landing compaction that is still unanswered.
    compaction_gate: Option<Arc<CompactionGate>>,
    /// Agent requests served while a compaction was unanswered.
    served_during_compaction: usize,
}

impl MockScenarioState {
    const fn new(scenario: MockChatScenario) -> Self {
        Self {
            scenario,
            request_index: 0,
            task_id: None,
            output_file: None,
            probe_done: false,
            probe_steps: 0,
            compactions: 0,
            compaction_gate: None,
            served_during_compaction: 0,
        }
    }

    fn next_compaction_probe_response(
        &mut self,
        probe: CompactionProbe,
        request: &Value,
    ) -> MockHttpResponse {
        self.request_index += 1;
        if is_compaction_request(request) {
            self.compactions += 1;
            if matches!(probe, CompactionProbe::FailsThenLands) && self.compactions <= 2 {
                return MockHttpResponse::sse(text_stream_frames(
                    vec!["this is not a handoff".to_string()],
                    Duration::ZERO,
                ));
            }
            let handoff = delayed_sse_frame(
                text_chunk(&compaction_probe_handoff(), None),
                Duration::ZERO,
            );
            let handoff = match probe {
                CompactionProbe::Lands => {
                    let gate = Arc::new(CompactionGate::default());
                    self.compaction_gate = Some(Arc::clone(&gate));
                    handoff.gated(gate)
                }
                CompactionProbe::FailsThenLands | CompactionProbe::LandsAtOnce => handoff,
            };
            return MockHttpResponse::sse(vec![
                handoff,
                delayed_sse_frame(text_chunk("", Some("stop")), Duration::ZERO),
                done_frame(Duration::ZERO),
            ]);
        }
        if self
            .compaction_gate
            .take()
            .is_some_and(|gate| gate.release())
        {
            self.served_during_compaction += 1;
        }
        let follow_up = request
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .and_then(|message| message.get("content").and_then(Value::as_str))
            .is_some_and(|content| content.contains(COMPACTION_PROBE_FOLLOW_UP));
        if follow_up || self.probe_steps >= COMPACTION_PROBE_STEPS {
            return MockHttpResponse::sse(text_stream_frames(
                vec!["all steps done".to_string()],
                Duration::from_millis(10),
            ));
        }
        self.probe_steps += 1;
        MockHttpResponse::sse(vec![
            delayed_sse_frame(
                tool_call_chunk(
                    &format!("call-compaction-{}", self.probe_steps),
                    "terminal",
                    &serde_json::json!({
                        "background": false,
                        "description": "Take a step",
                        "script": format!("echo COMPACTION-STEP-{}", self.probe_steps),
                        "mode": "default",
                        "raw": true,
                        "timeout": 30,
                        "max_lines": 20
                    }),
                ),
                COMPACTION_PROBE_STEP_DELAY,
            ),
            done_frame(Duration::from_millis(10)),
        ])
    }

    fn next_response(&mut self, request: &Value) -> Result<MockHttpResponse, String> {
        if let MockChatScenario::SubagentProbe(probe) = self.scenario.clone() {
            return self.next_probe_response(&probe, request);
        }
        if matches!(self.scenario, MockChatScenario::ForkProbe) {
            return self.next_fork_response(request);
        }
        if let MockChatScenario::CompactionProbe(probe) = self.scenario {
            return Ok(self.next_compaction_probe_response(probe, request));
        }
        verify_native_tool_surface(request)?;
        if self.request_index == 0 {
            verify_initial_tool_contract(request)?;
        }
        let response = match &self.scenario {
            MockChatScenario::SubagentProbe(_)
            | MockChatScenario::ForkProbe
            | MockChatScenario::CompactionProbe(_) => {
                unreachable!("handled before the match")
            }
            MockChatScenario::OverloadedThenText { failures, text } => {
                verify_initial_tool_contract(request)?;
                if self.request_index < *failures {
                    MockHttpResponse::Overloaded
                } else {
                    MockHttpResponse::sse(text_stream_frames(vec![text.clone()], Duration::ZERO))
                }
            }
            MockChatScenario::TextEveryTurn(text) => {
                // Every rebuilt agent must still present the prepared tools,
                // whether it prepared them or adopted them.
                verify_initial_tool_contract(request)?;
                MockHttpResponse::sse(text_stream_frames(vec![text.clone()], Duration::ZERO))
            }
            MockChatScenario::AddsMcpTool => match self.request_index {
                0 | 2 => {
                    let command = if self.request_index == 0 {
                        "probe_add"
                    } else {
                        "probe_greet"
                    };
                    MockHttpResponse::sse(vec![
                        delayed_sse_frame(
                            tool_call_chunk(
                                &format!("call-{command}"),
                                "terminal",
                                &serde_json::json!({
                                    "description": "Run the MCP probe command",
                                    "script": command,
                                    "mode": "default",
                                    "raw": true,
                                    "timeout": 30,
                                    "max_lines": 50
                                }),
                            ),
                            Duration::from_millis(10),
                        ),
                        done_frame(Duration::from_millis(10)),
                    ])
                }
                _ => MockHttpResponse::sse(text_stream_frames(
                    vec!["probed".to_string()],
                    Duration::from_millis(10),
                )),
            },
            MockChatScenario::CachedTextEveryTurn { cached } => {
                let read = cached
                    .get(self.request_index)
                    .or_else(|| cached.last())
                    .copied()
                    .unwrap_or_default();
                let usage = serde_json::json!({
                    "choices": [],
                    "usage": {
                        "prompt_tokens": read + 1_000,
                        "completion_tokens": 5,
                        "total_tokens": read + 1_005,
                        "prompt_tokens_details": { "cached_tokens": read }
                    }
                });
                let mut frames = text_stream_frames(vec!["cached".to_string()], Duration::ZERO);
                frames.insert(
                    frames.len() - 1,
                    delayed_sse_frame(format!("data: {usage}\n\n"), Duration::ZERO),
                );
                MockHttpResponse::sse(frames)
            }
            MockChatScenario::SlowTextEveryTurn { chunks, delay } => {
                verify_initial_tool_contract(request)?;
                MockHttpResponse::sse(text_stream_frames(chunks.clone(), *delay))
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
                // The command blocks on `read`; it is started detached so the
                // test drives its stdin as a background job.
                0 => MockHttpResponse::sse(vec![
                    delayed_sse_frame(
                        tool_call_chunk(
                            "call-terminal",
                            "terminal",
                            &serde_json::json!({
                                "background": true,
                                "description": "Ask the terminal for the user's name",
                                "script": "printf 'cwd=%s\\n' \"$(pwd -P)\"; printf 'name? '; read name; printf 'hello %s\\n' \"$name\"",
                                "mode": "default",
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
                    self.output_file = Some(
                        terminal_output_file_from_latest_tool_output(request).ok_or_else(|| {
                            "terminal tool result did not include an output file".to_string()
                        })?,
                    );
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
                    let input_result = latest_tool_output(request)
                        .ok_or_else(|| "terminal input command result missing".to_string())?;
                    if tool_output_reports_failure(&input_result) {
                        return Err(format!("terminal input IPC command failed: {input_result}"));
                    }
                    // Aither forgets a finished task once its completion
                    // reaches the model, so the id reads as unknown; the
                    // job's output lives in its stored file.
                    let output_file = self
                        .output_file
                        .clone()
                        .ok_or_else(|| "terminal output file was not captured".to_string())?;
                    let script = TerminalReadScript {
                        output_file: &output_file,
                    }
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
                                    "mode": "default",
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
            MockChatScenario::AsksUser => match self.request_index {
                0 => MockHttpResponse::sse(vec![
                    delayed_sse_frame(
                        tool_call_chunk(
                            "call-ask-user",
                            "terminal",
                            &serde_json::json!({
                                "description": "Ask the user which database to use",
                                "script": ASK_USER_SCRIPT,
                                "mode": "default",
                                "raw": true,
                                "timeout": 10,
                                "max_lines": 50
                            }),
                        ),
                        Duration::from_millis(10),
                    ),
                    done_frame(Duration::from_millis(10)),
                ]),
                _ => MockHttpResponse::sse(text_stream_frames(
                    vec!["noted".to_string()],
                    Duration::from_millis(10),
                )),
            },
            MockChatScenario::RunsScript(script) => {
                if self.request_index == 0 {
                    MockHttpResponse::sse(vec![
                        delayed_sse_frame(
                            tool_call_chunk(
                                "call-run-script",
                                "terminal",
                                &serde_json::json!({
                                    "description": "Run the probe script",
                                    "script": script,
                                    "mode": "default",
                                    "raw": true,
                                    "timeout": 30,
                                    "max_lines": 50
                                }),
                            ),
                            Duration::from_millis(10),
                        ),
                        done_frame(Duration::from_millis(10)),
                    ])
                } else {
                    let output = latest_tool_output(request).unwrap_or_default();
                    if output.contains("not permitted") {
                        return Err(format!("the probe script was denied: {output}"));
                    }
                    MockHttpResponse::sse(text_stream_frames(
                        vec!["ran".to_string()],
                        Duration::from_millis(10),
                    ))
                }
            }
            MockChatScenario::StartsBackgroundJob(script) => match self.request_index {
                0 => MockHttpResponse::sse(vec![
                    delayed_sse_frame(
                        tool_call_chunk(
                            "call-background-job",
                            "terminal",
                            &serde_json::json!({
                                "description": "Start a long job",
                                "script": script,
                                "mode": "default",
                                "background": true,
                                "timeout": 600,
                            }),
                        ),
                        Duration::from_millis(10),
                    ),
                    done_frame(Duration::from_millis(10)),
                ]),
                _ => MockHttpResponse::sse(text_stream_frames(
                    vec!["started".to_string()],
                    Duration::from_millis(10),
                )),
            },
            MockChatScenario::WritesTasksFile => {
                // The run loop keeps the model going while `tasks.md` has
                // open rows (`Doing`, `Queued`), so the mock writes two rows,
                // finishes the work by deleting them on the next request and
                // replies with plain text once nothing is open.
                if self.request_index >= TASKS_SCENARIO_MAX_REQUESTS {
                    return Err(format!(
                        "tasks scenario still looping after {} requests: the run loop keeps objecting to tasks.md",
                        self.request_index
                    ));
                }
                let objected = request
                    .get("messages")
                    .and_then(Value::as_array)
                    .and_then(|messages| messages.last())
                    .and_then(|message| message.get("content").and_then(Value::as_str))
                    .is_some_and(|content| content.contains("tasks.md still has rows"));
                let script = if self.request_index == 0 {
                    Some(TASKS_OPEN_SCRIPT)
                } else if objected {
                    Some(TASKS_DONE_SCRIPT)
                } else {
                    None
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
                                    "mode": "default",
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
        // A subagent whose definition declares no tier asks the fast model
        // which tier fits the prompt first; answer the routing request with
        // the balanced tier, then the launch continues on the routed model.
        if is_routing_request(request) {
            self.request_index += 1;
            return Ok(MockHttpResponse::sse(text_stream_frames(
                vec!["{\"model\": \"balanced\"}".to_string()],
                Duration::from_millis(10),
            )));
        }
        let is_child = is_subagent_request(request);
        if !is_child {
            verify_native_tool_surface(request)?;
        }
        let latest = latest_tool_output(request);
        let terminal_call = |background: bool, id: &str, script: &str| {
            MockHttpResponse::sse(vec![
                delayed_sse_frame(
                    tool_call_chunk(
                        id,
                        "terminal",
                        &serde_json::json!({
                            "background": background,
                            "description": "Run the subagent authority probe",
                            "script": script,
                            "mode": "default",
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
                || terminal_call(false, &id, probe.child_script),
                |output| text(format!("child probe output: {output}")),
            )
        } else {
            match latest {
                None => {
                    let launch = format!(
                        "( {setup}\nprintf '{SUBAGENT_PROBE_MARKER}: run the probe.\\n' > prompt.md\n{launch}\n) > sub.out 2>&1; touch sub.done",
                        setup = probe.setup,
                        launch = probe.launch,
                    );
                    terminal_call(true, &id, &launch)
                }
                Some(output) if output.contains("MAIN_FINAL") => {
                    self.probe_done = true;
                    text("probe complete".to_string())
                }
                Some(output) if output.contains("POLL_DONE") => {
                    terminal_call(false, &id, probe.final_script)
                }
                _ if launch_job_finished(request) => {
                    terminal_call(false, &id, SUBAGENT_READ_SCRIPT)
                }
                _ => text("the launch is running in the background".to_string()),
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
        // A context near its compaction point starts a background compaction;
        // its handoff arrives long after the probe is over, so the context
        // stays near the point while the fork is tried.
        if is_compaction_request(request) {
            self.request_index += 1;
            return Ok(MockHttpResponse::sse(text_stream_frames(
                vec![COMPACTION_HANDOFF.to_string()],
                COMPACTION_ANSWER_DELAY,
            )));
        }
        let terminal_call = |background: bool, id: &str, script: &str| {
            MockHttpResponse::sse(vec![
                delayed_sse_frame(
                    tool_call_chunk(
                        id,
                        "terminal",
                        &serde_json::json!({
                            "background": background,
                            "description": "Run the fork probe",
                            "script": script,
                            "mode": "default",
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
                None => terminal_call(false, &id, FORK_SECRET_SCRIPT),
                Some(output) if output.contains("MAIN_FINAL") => {
                    self.probe_done = true;
                    text("fork probe complete")
                }
                Some(output) if output.contains("POLL_DONE") => {
                    terminal_call(false, &id, "echo MAIN_FINAL")
                }
                Some(output) if output.contains(FORK_SECRET) => {
                    let launch = format!(
                        "( printf '{FORK_TASK_MARKER}: report what you know.\\n' > prompt.md\nsubagent fork --prompt-file prompt.md ) > sub.out 2>&1; touch sub.done"
                    );
                    terminal_call(true, &id, &launch)
                }
                _ if launch_job_finished(request) => {
                    terminal_call(false, &id, SUBAGENT_READ_SCRIPT)
                }
                _ => text("the launch is running in the background"),
            }
        };
        self.request_index += 1;
        Ok(response)
    }
}

const TASKS_OPEN_SCRIPT: &str =
    "printf -- '## Doing\\n- write report\\n\\n## Queued\\n- ship it\\n' > tasks.md";
const TASKS_DONE_SCRIPT: &str = "printf -- '## Notes\\nreport shipped\\n' > tasks.md";

/// Requests the tasks scenario may serve before the harness fails it, so a
/// run loop that never settles ends the test with a message instead of hanging.
const TASKS_SCENARIO_MAX_REQUESTS: usize = 8;

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
            "compact",
            "monitor",
            "terminal",
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
    for command in ["apply_patch", "rg", "ast-grep", "jq"] {
        if !catalog.contains(&format!("\n- {command}\n")) {
            return Err(format!("prepared tool catalog omits {command}"));
        }
    }
    Ok(())
}

enum MockHttpResponse {
    Sse(Vec<MockSseFrame>),
    /// The upstream is overloaded: status 529.
    Overloaded,
}

impl MockHttpResponse {
    const fn sse(frames: Vec<MockSseFrame>) -> Self {
        Self::Sse(frames)
    }
}

struct MockSseFrame {
    /// Held by this gate before the delay.
    gate: Option<Arc<CompactionGate>>,
    delay: Duration,
    data: String,
}

impl MockSseFrame {
    fn gated(self, gate: Arc<CompactionGate>) -> Self {
        Self {
            gate: Some(gate),
            ..self
        }
    }
}

const fn delayed_sse_frame(data: String, delay: Duration) -> MockSseFrame {
    MockSseFrame {
        gate: None,
        delay,
        data,
    }
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
    gate: Option<Arc<RequestGate>>,
) -> Result<Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>, Infallible> {
    if request.method() == Method::GET && request.uri().path() == "/v1/models" {
        let data = [
            TEST_MODEL,
            TIER_FLAGSHIP_MODEL,
            TIER_BALANCED_MODEL,
            TIER_FAST_MODEL,
        ]
        .map(|id| {
            serde_json::json!({
                "id": id,
                "owned_by": "mock",
                "context_length": 128_000
            })
        });
        return Ok(json_response(
            StatusCode::OK,
            &serde_json::json!({ "data": data }),
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

    if let Some(gate) = &gate {
        gate.hold_if_selected().await;
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
        MockHttpResponse::Overloaded => json_response(
            StatusCode::from_u16(529).expect("529 is a valid status code"),
            &serde_json::json!({ "error": { "type": "overloaded_error", "message": "Overloaded" } }),
        ),
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
            if let Some(gate) = &frame.gate {
                gate.hold().await;
            }
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
    /// Owns the temporary home; dropping it removes the directory.
    _home: TempDir,
    /// The home's real path: May resolves its home, so every path it reports
    /// is spelled this way (`/var` is a link to `/private/var` on macOS).
    home_path: PathBuf,
    may_dir: PathBuf,
}

impl Drop for TestHarness {
    /// Deletes the home, then sweeps the relocated-home agents whose plists
    /// went with it, so a run leaves none loaded.
    fn drop(&mut self) {
        // A `may exec` of the test started a daemon in this home; it is
        // gone with the home.
        let _ = std::process::Command::new(may_binary_path())
            .args(["daemon", "stop"])
            .env("HOME", &self.home_path)
            .output();
        let _ = std::fs::remove_dir_all(&self.home_path);
        let swept = may_agent::repo_search::sweep_stale_relocated_agents();
        if !std::thread::panicking() {
            swept.expect("sweeping stale relocated-home agents should succeed");
        }
    }
}

const HOST_USER_NAME: &str = "E2E User";
const HOST_USER_EMAIL: &str = "e2e.user@example.com";

/// The file standing in for the host user's global git configuration, which
/// every `may` the suite starts reads instead of the developer's own.
fn host_gitconfig(home: &Path) -> PathBuf {
    home.join("host-gitconfig")
}

/// The environment that gives a `may` process the hermetic host user.
fn hermetic_git_env(home: &Path) -> [(&'static str, PathBuf); 2] {
    [
        ("GIT_CONFIG_GLOBAL", host_gitconfig(home)),
        ("GIT_CONFIG_SYSTEM", PathBuf::from("/dev/null")),
    ]
}

impl TestHarness {
    /// Makes the host user sign their commits with a fresh ssh key.
    fn host_user_signs(&self) {
        let key = self.home_path().join("host-signing-key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "e2e", "-f"])
            .arg(&key)
            .status()
            .expect("ssh-keygen should run");
        assert!(status.success(), "ssh-keygen should generate the host key");
        std::fs::write(
            host_gitconfig(self.home_path()),
            format!(
                "[user]\n\tname = {HOST_USER_NAME}\n\temail = {HOST_USER_EMAIL}\n\
                 \tsigningkey = {}\n[gpg]\n\tformat = ssh\n[commit]\n\tgpgsign = true\n",
                key.display()
            ),
        )
        .expect("host gitconfig should be written");
    }

    async fn new(config_base_url: &str) -> Self {
        Self::build(config_base_url, None, ContextConfig::default()).await
    }

    /// A harness whose `[context]` thresholds are `context`.
    async fn new_with_context(config_base_url: &str, context: ContextConfig) -> Self {
        Self::build(config_base_url, None, context).await
    }

    /// A harness whose three model tiers are three models of their own, so a
    /// request's model names the tier it ran on, and whose session runs on
    /// `session_model`.
    async fn new_with_tier_models_on(config_base_url: &str, session_model: &str) -> Self {
        Self::build(
            config_base_url,
            Some(session_model),
            ContextConfig::default(),
        )
        .await
    }

    async fn build(
        config_base_url: &str,
        tier_models: Option<&str>,
        context: ContextConfig,
    ) -> Self {
        // A killed earlier run left agents of homes that no longer exist.
        may_agent::repo_search::sweep_stale_relocated_agents()
            .expect("sweeping stale relocated-home agents should succeed");
        let home = tempfile::tempdir().expect("test home tempdir should be created");
        let home_path = dunce::canonicalize(home.path()).expect("test home should resolve");
        let may_dir = home_path.join(".may");
        tokio::fs::create_dir_all(&may_dir)
            .await
            .expect("test may dir should be created");

        let mut config = MayConfig::default();
        // Aux generation would fire an unscripted request per completed
        // turn into the mock provider and starve the scripted responses.
        config.suggestions.enabled = false;
        // A port of its own, so a daemon a `may exec` of the test starts
        // meets no other.
        config.daemon.port = Some(
            std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|listener| listener.local_addr())
                .expect("a free port should be found")
                .port(),
        );
        config.context = context;
        config.provider.insert(
            TEST_PROVIDER_ID.to_string(),
            ProviderInstance::new(ProviderType::Custom)
                .with_api_key(TEST_API_KEY)
                .with_base_url(config_base_url),
        );
        if tier_models.is_some() {
            config.models.advanced =
                vec![ModelSelector::new(TEST_PROVIDER_ID, TIER_FLAGSHIP_MODEL)];
            config.models.balanced =
                vec![ModelSelector::new(TEST_PROVIDER_ID, TIER_BALANCED_MODEL)];
            config.models.fast = vec![ModelSelector::new(TEST_PROVIDER_ID, TIER_FAST_MODEL)];
        } else {
            config.models.advanced = vec![ModelSelector::new(TEST_PROVIDER_ID, TEST_MODEL)];
        }
        config.last_model = Some(ModelSelector::new(
            TEST_PROVIDER_ID,
            tier_models.unwrap_or(TEST_MODEL),
        ));
        config
            .save_to(&may_dir.join("config.toml"))
            .await
            .expect("test config should be written");

        // A typical user: an identity and no commit signing.
        std::fs::write(
            host_gitconfig(home.path()),
            format!("[user]\n\tname = {HOST_USER_NAME}\n\temail = {HOST_USER_EMAIL}\n"),
        )
        .expect("host gitconfig should be written");

        futures_lite::future::zip(
            share_and_warm_tool_cache(&may_dir),
            warm_may_binary(&home_path),
        )
        .await;

        Self {
            _home: home,
            home_path,
            may_dir,
        }
    }

    /// Tracing output a spawned server emitted, with ANSI styling stripped so
    /// assertions can match log content. May keeps diagnostics in an in-memory
    /// ring now; the captured stderr copy carries the same lines.
    async fn server_logs(&self, server: &RunningDaemon) -> String {
        strip_ansi_escapes(&server.log.contents().await)
    }

    /// Session in a local-environment profile. Terminal-first contract tests
    /// use this so they exercise Leash, the session filesystem, and IPC
    /// independently of an optional host container runtime.
    async fn create_session(&self) -> TestSession {
        self.create_session_in_profile("build").await
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
        let session_id = db
            .issue_session_name()
            .await
            .expect("a session name")
            .to_string();
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
        &self.home_path
    }
}

struct TestSession {
    id: String,
    sandbox_dir: PathBuf,
}

struct RunningDaemon {
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

/// Removes ANSI CSI styling sequences (`ESC [ … final-byte`) from captured
/// terminal output so log assertions can match plain text.
fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(escape) = rest.find("\x1b[") {
        out.push_str(&rest[..escape]);
        let after = &rest[escape + 2..];
        rest = after
            .find(|c: char| ('@'..='~').contains(&c))
            .map_or("", |end| &after[end + 1..]);
    }
    out.push_str(rest);
    out
}

impl RunningDaemon {
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
            .envs(hermetic_git_env(home))
            // Module-level directives outrank May's baked crate-level filter,
            // keeping failed assertions diagnosable from captured logs.
            .env(
                "RUST_LOG",
                "aither_openai::client=debug,may_agent::daemon::chat=debug,may_agent::container=debug",
            )
            .stdin(Stdio::null())
            .stdout(StdStdio::from(log_file))
            .stderr(StdStdio::from(log_file_err));
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

    /// Kills the daemon outright, as a crash would: no shutdown path runs.
    /// For tests whose point is surviving an abrupt death; it leaves a
    /// relocated home's launchd agents loaded.
    async fn kill(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    /// Stops the daemon through its shutdown path, so it retires what it
    /// installed for its relocated home, and waits for the process to exit.
    ///
    /// # Panics
    /// When the daemon ignores the shutdown: it is then killed and the test
    /// fails.
    async fn stop(mut self) {
        let requested = reqwest::Client::new()
            .post(format!("{}/api/daemon/shutdown", self.base_url))
            .json(&serde_json::json!({}))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        let exited = requested
            && tokio::time::timeout(DAEMON_SHUTDOWN_TIMEOUT, self.child.wait())
                .await
                .is_ok();
        if !exited {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
            panic!(
                "daemon did not exit within {DAEMON_SHUTDOWN_TIMEOUT:?} of a shutdown request (accepted: {requested}); server logs:\n{}",
                self.log.contents().await
            );
        }
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

/// Directory holding the downloads every test run shares. `MAY_TEST_CACHE`
/// names it so CI can restore it from the Actions cache; unset, it is the
/// build's test scratch directory.
fn shared_download_root() -> PathBuf {
    std::env::var_os("MAY_TEST_CACHE")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from)
}

/// Points the test home's tool cache at one cache shared by every test run
/// and prepares the host's baseline tools and `heel` in it through the
/// embedded distribution, so the first hash-verified download happens here
/// instead of inside a server's readiness deadline. Later runs reuse the
/// cached artifacts.
async fn share_and_warm_tool_cache(may_dir: &Path) {
    let shared = shared_download_root().join("may-tools");
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
    let distribution =
        may_agent::tools::embedded_distribution().expect("embedded distribution should validate");
    let tools_dir = home_paths.tools_dir();
    let (bundle, heel) = futures_lite::future::zip(
        distribution.load(
            &ToolTarget::native().expect("test host should have a tool target"),
            &home_paths.tool_artifacts_dir(),
        ),
        may_agent::tools::prepare_heel(&distribution, &tools_dir),
    )
    .await;
    heel.expect("the distribution's heel should download and verify");
    let bundle = bundle.expect("baseline tool artifacts should download and verify");
    bundle
        .prepare_local(&home_paths.tool_runtimes_dir())
        .await
        .expect("baseline tools should prepare");
}

/// The daemon's shutdown drains runs for up to 30 s and settles cancelled
/// ones for 5 s more.
const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);

impl Drop for RunningDaemon {
    /// A test that failed before `stop` still lets the daemon shut down in
    /// order: SIGTERM runs the same path as the shutdown request, and the drop
    /// blocks until the process exits (SIGKILL after the shutdown timeout), so
    /// the test's home outlives it. Reaping goes through tokio's own
    /// `try_wait`, so the child is never reaped behind its back.
    fn drop(&mut self) {
        let Some(pid) = self.child.id() else {
            return; // already reaped by `stop` or `kill`
        };
        let pid = i32::try_from(pid).expect("a pid fits in i32");
        // SAFETY: signalling a child this value spawned and has not reaped.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + DAEMON_SHUTDOWN_TIMEOUT;
        while matches!(self.child.try_wait(), Ok(None)) {
            if std::time::Instant::now() >= deadline {
                let _ = self.child.start_kill();
                let _ = self.child.try_wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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

#[derive(Debug)]
struct TestSseEvent {
    event: String,
    data: String,
    id: Option<String>,
}

/// Starts a turn the way every client does: subscribes to the session's
/// `session_turn` frames on `/api/sessions/{id}/events`, then `POST`s the
/// message. Returns the event stream, or the refused `POST`'s response.
async fn send_turn(
    client: &reqwest::Client,
    base_url: &str,
    body: &Value,
) -> reqwest::Result<reqwest::Response> {
    let session_id = body
        .get("sessionId")
        .and_then(Value::as_str)
        .expect("turn body names its session")
        .to_string();
    let events = client
        .get(format!("{base_url}/api/sessions/{session_id}/events"))
        .send()
        .await?;
    let sent = client
        .post(format!("{base_url}/api/sessions/{session_id}/turns"))
        .json(body)
        .send()
        .await?;
    if sent.status().is_success() {
        Ok(events)
    } else {
        Ok(sent)
    }
}

/// Drain a session's `session_turn` frames into `(event kind, parsed data)`
/// pairs until the turn finishes (`done`/`paused`/`error`/`end`) or the
/// server closes the stream.
async fn collect_sse_events(response: reqwest::Response) -> Vec<(String, Value)> {
    let mut reader = SseReader::new(response);
    let mut events = Vec::new();
    while let Some(event) = reader.next_event().await {
        if event.event != "session_turn" {
            continue;
        }
        let frame: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);
        let kind = frame
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = frame.get("data").cloned().unwrap_or(Value::Null);
        let finished = matches!(kind.as_str(), "done" | "paused" | "error" | "end");
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
    let mut id = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        // Per the SSE spec, a single space after the field colon is optional.
        if let Some(value) = line.strip_prefix("event:") {
            event = value.strip_prefix(' ').unwrap_or(value).to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.strip_prefix(' ').unwrap_or(value).to_string());
        } else if let Some(value) = line.strip_prefix("id:") {
            id = Some(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
    }
    TestSseEvent {
        event,
        data: data_lines.join("\n"),
        id,
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
    server: &RunningDaemon,
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
    server: &RunningDaemon,
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
/// Whether the launch job announced its completion: the sandbox reports a
/// finished background task as a reminder in the next request, and the
/// launch is the only background job this probe starts.
fn launch_job_finished(request: &Value) -> bool {
    request
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .any(|content| {
                    content.contains("Background task")
                        && (content.contains("finished") || content.contains("failed"))
                })
        })
}

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

fn terminal_output_file_from_latest_tool_output(request: &Value) -> Option<String> {
    let output = latest_tool_output(request)?;
    let marker = "being written to ";
    let start = output.find(marker)? + marker.len();
    let end = output[start..].find(' ')? + start;
    Some(output[start..end].to_string())
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
    /// The `build` profile: commands run on the host under heel.
    Local,
    /// A container profile the test creates on demand: commands run inside
    /// the May workspace container. The profile is not location-aware, so a
    /// turn never performs a geolocation lookup.
    ///
    /// Its tests expect a container runtime on Linux (Docker on the CI
    /// runners), where a missing runtime fails them; elsewhere they are
    /// ignored unless requested.
    Container,
}

/// Upper bound on building the May workspace image from the repository's
/// container context on a cold runtime.
const CONTAINER_IMAGE_PULL_TIMEOUT: Duration = Duration::from_mins(20);

/// Extra time a container session's first turn gets for starting its
/// container and preparing the container's tool bundle before the model is
/// first asked.
const CONTAINER_FIRST_TURN_ALLOWANCE: Duration = Duration::from_secs(120);

impl ChatEnvironment {
    /// Resolves the environment's profile on `server` and makes it ready for
    /// chat the way the web UI does before opening a profile.
    async fn prepare(
        self,
        harness: &TestHarness,
        client: &reqwest::Client,
        server: &RunningDaemon,
    ) -> PreparedEnvironment {
        match self {
            Self::Local => {
                let profile_id = profile_id_via_api(client, server, "build").await;
                PreparedEnvironment::Local { profile_id }
            }
            Self::Container => {
                let profile_id = create_container_profile(harness).await;
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
                    CONTAINER_IMAGE_PULL_TIMEOUT,
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
                         {CONTAINER_IMAGE_PULL_TIMEOUT:?}; server logs:\n{}",
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
                PreparedEnvironment::Container {
                    profile_id,
                    manager: may_agent::container::lifecycle::ContainerManager::new(
                        Arc::new(runtime),
                        harness.may_dir.clone(),
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

    /// The sentence of May's environment section that says where commands
    /// run for this environment.
    fn context_heading(&self) -> String {
        match self {
            Self::Local { .. } => {
                let operating_system = match std::env::consts::OS {
                    "macos" => "macOS",
                    "linux" => "Linux",
                    "windows" => "Windows",
                    other => other,
                };
                format!(
                    "You run on the user's own {operating_system} machine; commands run in a sandbox on it, in POSIX sh."
                )
            }
            Self::Container { .. } => "You run in a managed Linux container, isolated from the user's machine; commands run in POSIX sh.".to_string(),
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
    server: &RunningDaemon,
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

/// Creates the container-environment profile container tests run in, and
/// returns its id. The id stays short on purpose: it becomes part of the
/// session container's name, which Linux caps as a hostname at 63 bytes.
async fn create_container_profile(harness: &TestHarness) -> String {
    let (_store, db, _default_profile_id) = initialize_session_persistence(&harness.may_dir)
        .await
        .expect("session persistence should initialize");
    if let Some(existing) = db
        .get_profile("e2e-container")
        .await
        .expect("profile lookup should succeed")
    {
        return existing.id;
    }
    db.create_profile(CreateProfileParams {
        id: "e2e-container",
        name: "e2e-container",
        environment: ProfileEnvironmentKind::Container,
        slug: None,
        icon: None,
        persona: None,
        preferences: None,
        location_aware: false,
    })
    .await
    .expect("container profile should be created")
    .id
}

async fn profile_id_via_api(
    client: &reqwest::Client,
    server: &RunningDaemon,
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

async fn get_runtime(client: &reqwest::Client, server: &RunningDaemon, session_id: &str) -> Value {
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

fn extract_assistant_messages(transcript: &Value) -> Vec<String> {
    transcript
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            if entry.get("kind")?.as_str()? != "assistant" {
                return None;
            }
            entry.get("text")?.as_str().map(ToOwned::to_owned)
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
#[file_serial]
async fn exec_uses_session_directory_and_exposes_ipc_command_help() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let session = harness.create_session().await;
    let binary = may_binary_path();

    let pwd_output = Command::new(&binary)
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("default")
        .arg("--")
        .arg("pwd")
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
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
        .arg("default")
        .arg("--")
        .arg(
            "for d in tmp data files outputs artifacts; do test -d \"$d\" || exit 10; done; test -e skills; test -e subagents; session --help >/dev/null; ask --help >/dev/null; search --help >/dev/null; webfetch --help >/dev/null; task --help >/dev/null; printf ready",
        )
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
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

/// Makes one commit in a fresh repository of the session's sandbox through
/// `may exec` and prints the commit object.
async fn commit_in_sandbox(harness: &TestHarness) -> std::process::Output {
    let session = harness.create_session().await;
    Command::new(may_binary_path())
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("default")
        .arg("--")
        .arg(
            "git init -q -b main repo && cd repo && echo x > f && git add f \
             && git commit -q -m one \
             && git cat-file commit HEAD",
        )
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
        .output()
        .await
        .expect("may exec commit should run")
}

#[tokio::test]
#[file_serial]
async fn a_user_who_does_not_sign_commits_unsigned_as_themselves_in_the_sandbox() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;

    let output = commit_in_sandbox(&harness).await;

    assert!(
        output.status.success(),
        "the commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit = String::from_utf8_lossy(&output.stdout);
    let who = format!("{HOST_USER_NAME} <{HOST_USER_EMAIL}>");
    assert!(commit.contains(&format!("author {who} ")), "{commit}");
    assert!(commit.contains(&format!("committer {who} ")), "{commit}");
    assert!(!commit.contains("gpgsig"), "an unsigned commit: {commit}");
}

#[tokio::test]
#[file_serial]
async fn a_user_who_signs_has_sandbox_commits_signed_through_the_harness() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    harness.host_user_signs();

    let output = commit_in_sandbox(&harness).await;

    assert!(
        output.status.success(),
        "the commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit = String::from_utf8_lossy(&output.stdout);
    let who = format!("{HOST_USER_NAME} <{HOST_USER_EMAIL}>");
    assert!(commit.contains(&format!("author {who} ")), "{commit}");
    assert!(commit.contains(&format!("committer {who} ")), "{commit}");
    assert!(
        commit.contains("gpgsig -----BEGIN SSH SIGNATURE-----"),
        "a signed commit: {commit}"
    );
    let audit = std::fs::read_to_string(harness.may_dir.join("logs/signing/commits.jsonl"))
        .expect("the signing is in the audit log");
    assert_eq!(audit.lines().count(), 1, "{audit}");
}

/// A home reached through a symlink: the session directory May hands the
/// terminal is the real one, so a `cd` into it and a write there succeed under
/// heel's strict filesystem grants.
#[cfg(unix)]
#[tokio::test]
#[file_serial]
async fn a_terminal_command_runs_and_writes_in_the_session_directory_when_home_is_a_symlink() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::RunsScript(
        "cd \"$(pwd)\" && printf written > symlink-home-probe && cat symlink-home-probe",
    ))
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let link_parent = tempfile::tempdir().expect("link tempdir should be created");
    let linked_home = link_parent.path().join("linked-home");
    std::os::unix::fs::symlink(harness.home_path(), &linked_home)
        .expect("home symlink should be created");
    let server = RunningDaemon::spawn(&linked_home, &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let sandbox_dir = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&session_id, Some(environment.profile_id()));

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "run the probe" }),
    )
    .await
    .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = collect_turn_events(response, "the symlinked-home turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );
    let written = std::fs::read_to_string(sandbox_dir.join("symlink-home-probe"))
        .expect("the command should have written its file in the session directory");
    assert_eq!(written, "written");
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// The text of each message of a model request, in order.
fn message_texts(request: &Value) -> Vec<String> {
    request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|message| match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
            _ => String::new(),
        })
        .collect()
}

/// The messages after a request's system prefix that announce a change to
/// the command table.
fn command_table_alerts(request: &Value) -> Vec<String> {
    message_texts(request)
        .into_iter()
        .skip(1)
        .filter(|text| text.contains("<commands_diff boundary="))
        .collect()
}

/// A tool an MCP server adds mid-session is runnable in the same session at
/// once, and the model hears of it once, as a diff, while the cached prefix
/// stays as it was sent.
#[tokio::test]
#[file_serial]
async fn a_tool_an_mcp_server_adds_mid_session_runs_and_is_announced_once() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::AddsMcpTool).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/changing_tools_mcp_server.py");
    std::fs::write(
        harness.may_dir.join("mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "probe": {
                    "command": "python3",
                    "args": [server_script.display().to_string()]
                }
            }
        })
        .to_string(),
    )
    .expect("mcp.json should be written");
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    for content in ["add the tool", "greet", "anything new?"] {
        let response = send_turn(
            &client,
            &server.base_url,
            &serde_json::json!({ "sessionId": session_id, "content": content }),
        )
        .await
        .expect("chat stream request should succeed");
        let response = expect_status(&server.log, response, StatusCode::OK).await;
        let events = collect_turn_events(response, content, &server.log).await;
        assert!(
            events.iter().any(|(kind, _)| kind == "done"),
            "{content:?} should complete; events: {events:?}; server logs:\n{}",
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

    let requests = mock.agent_requests().await;
    assert_eq!(requests.len(), 5, "two tool turns and a text turn");
    let prefix = &message_texts(&requests[0])[0];
    assert!(
        prefix.contains("- probe_add: Adds the probe_greet tool"),
        "{prefix}"
    );
    assert!(!prefix.contains("probe_greet:"), "{prefix}");
    assert!(
        latest_tool_output(&requests[1]).is_some_and(|output| output.contains("ADDED-PROBE-GREET")),
        "{:?}",
        latest_tool_output(&requests[1])
    );
    assert!(
        latest_tool_output(&requests[3])
            .is_some_and(|output| output.contains("GREETED-PROBE-4417")),
        "probe_greet should run in the same session: {:?}; server logs:\n{}",
        latest_tool_output(&requests[3]),
        server.log.contents().await
    );
    for request in &requests {
        assert_eq!(
            &message_texts(request)[0],
            prefix,
            "the cached prefix stays as sent"
        );
    }
    let announced = command_table_alerts(&requests[2]);
    assert_eq!(announced.len(), 1, "{announced:?}");
    assert!(
        announced[0].contains("\n+- probe_greet: Greets the probe\n"),
        "{}",
        announced[0]
    );
    assert_eq!(
        command_table_alerts(&requests[4]),
        announced,
        "the change is announced once"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// Runs `script` through `may exec` in `mode` on `session`, in a home of its
/// own and with `bin` ahead of `PATH`.
async fn exec_script(
    harness: &TestHarness,
    session: &TestSession,
    mode: &str,
    bin: &Path,
    workdir: &Path,
    script: &str,
) -> std::process::Output {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH is set")
    );
    Command::new(may_binary_path())
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg(mode)
        .arg("--")
        .arg(script)
        .current_dir(workdir)
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
        .env("PATH", path)
        .output()
        .await
        .expect("may exec should run")
}

#[tokio::test]
#[file_serial]
async fn issue_changes_are_recorded_and_gh_credentials_are_unreadable_from_the_sandbox() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let session = harness.create_session().await;
    let work = tempfile::tempdir().expect("workdir");
    let bin = session.sandbox_dir.join("tmp");
    tokio::fs::create_dir_all(&bin).await.expect("fake bin");
    let fake_gh = bin.join("gh");
    let gh_marker = bin.join("gh-marker");
    tokio::fs::write(
        &fake_gh,
        format!("#!/bin/sh\ntouch '{}'\necho gh-ran\n", gh_marker.display()),
    )
    .await
    .expect("fake gh");
    std::fs::set_permissions(
        &fake_gh,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod fake gh");
    let gh_config = harness.home_path().join(".config/gh");
    tokio::fs::create_dir_all(&gh_config).await.expect("gh dir");
    tokio::fs::write(gh_config.join("hosts.yml"), "oauth_token: SECRET-TOKEN\n")
        .await
        .expect("gh credentials");

    let created = exec_script(
        &harness,
        &session,
        "default",
        &bin,
        work.path(),
        "issue create bug 'from exec' --body body && issue comment 1 hello && issue close 1",
    )
    .await;
    assert!(
        created.status.success(),
        "issue changes failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let provenance = exec_script(
        &harness,
        &session,
        "default",
        &bin,
        work.path(),
        "issue provenance 1; issue view 1",
    )
    .await;
    let text = String::from_utf8_lossy(&provenance.stdout).to_string();
    assert!(provenance.status.success(), "{text}");
    for expected in [
        "create",
        "comment",
        "close",
        session.id.as_str(),
        "agent exec",
        "written by may: yes",
        "[written by may] hello",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }

    let credentials = exec_script(
        &harness,
        &session,
        "default",
        &bin,
        work.path(),
        "cat ~/.config/gh/hosts.yml",
    )
    .await;
    assert!(
        !credentials.status.success()
            && !String::from_utf8_lossy(&credentials.stdout).contains("SECRET-TOKEN"),
        "gh credentials must be unreadable in the sandbox: {}",
        String::from_utf8_lossy(&credentials.stdout)
    );

    let sandboxed_gh = exec_script(
        &harness,
        &session,
        "default",
        &bin,
        work.path(),
        "gh issue create --title t",
    )
    .await;
    let refusal = format!(
        "{}{}",
        String::from_utf8_lossy(&sandboxed_gh.stdout),
        String::from_utf8_lossy(&sandboxed_gh.stderr)
    );
    assert!(
        !sandboxed_gh.status.success()
            && refusal.contains("not available in the sandbox")
            && refusal.contains("`issue`")
            && !refusal.contains("gh-ran"),
        "a sandboxed gh is refused with a pointer to issue: {refusal}"
    );
    assert!(
        !gh_marker.exists(),
        "the fake gh must not run in the sandbox"
    );

    let unsafe_gh = exec_script(
        &harness,
        &session,
        "unsafe",
        &bin,
        work.path(),
        "gh --version",
    )
    .await;
    let ran = String::from_utf8_lossy(&unsafe_gh.stdout);
    assert!(
        unsafe_gh.status.success() && ran.contains("gh-ran"),
        "an approved unsafe command runs gh: {} {}",
        String::from_utf8_lossy(&unsafe_gh.stdout),
        String::from_utf8_lossy(&unsafe_gh.stderr)
    );
    assert!(gh_marker.exists(), "the approved unsafe gh ran the fake gh");
}

#[tokio::test]
#[file_serial]
async fn exec_large_output_is_previewed_and_saved_to_outputs_file() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let session = harness.create_session().await;
    let binary = may_binary_path();

    let output = Command::new(binary)
        .arg("exec")
        .arg("--session")
        .arg(&session.id)
        .arg("--mode")
        .arg("default")
        .arg("--")
        .arg("perl -e 'print \"x\" x 60000'")
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
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
#[file_serial]
async fn ui_session_crud_round_trip_matches_api_contract() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();

    let create_response = client
        .post(format!("{}/api/sessions", server.base_url))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("create session request should succeed");
    let create_response = expect_status(&server.log, create_response, StatusCode::OK).await;
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
        .patch(format!("{}/api/sessions/{session_id}", server.base_url))
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
#[file_serial]
async fn ui_chat_tasks_file_is_the_canonical_todo_document() {
    const MODEL_WRITTEN: &str = "## Notes\nreport shipped\n";
    const REWRITTEN: &str = "## Doing\n- rewritten plan\n";

    let mock = MockOpenAiServer::spawn(MockChatScenario::WritesTasksFile).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let sandbox_dir = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&session_id, Some(environment.profile_id()));

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": "Plan the work in tasks.md."
        }),
    )
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
    // its rows down the stream; deleting them later pushes an empty list.
    let updates: Vec<&Value> = events
        .iter()
        .filter(|(kind, _)| kind == "todo_update")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(
        updates.len(),
        2,
        "each tasks.md write should emit one todo_update: {events:?}"
    );
    let rows = |update: &Value| update.get("rows").cloned();
    assert_eq!(
        rows(updates[0]),
        Some(serde_json::json!([
            { "section": "doing", "title": "write report", "detail": "", "blocking": null, "references": [] },
            { "section": "queued", "title": "ship it", "detail": "", "blocking": null, "references": [] },
        ])),
        "todo_update should carry the parsed tasks.md rows: {:?}",
        updates[0]
    );
    assert_eq!(
        rows(updates[1]),
        Some(serde_json::json!([])),
        "finishing the work deletes its rows: {:?}",
        updates[1]
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
    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": "Continue."
        }),
    )
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
    let session_model =
        matches!(scenario, MockChatScenario::SubagentProbe(_)).then_some(TIER_FLAGSHIP_MODEL);
    run_probe_turn_on(scenario, content, session_model).await
}

/// `run_probe_turn` on a harness whose tiers are models of their own and whose
/// session runs on `session_model`; `None` configures only the session model.
async fn run_probe_turn_on(
    scenario: MockChatScenario,
    content: &str,
    session_model: Option<&str>,
) -> (ProbeFiles, MockOpenAiServer) {
    let mock = MockOpenAiServer::spawn(scenario).await;
    let harness = match session_model {
        Some(model) => TestHarness::new_with_tier_models_on(&mock.base_url(), model).await,
        None => TestHarness::new(&mock.base_url()).await,
    };
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let sandbox_dir = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&session_id, Some(environment.profile_id()));

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": content
        }),
    )
    .await
    .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = collect_turn_events(response, "the subagent probe turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the probe turn should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );
    mock.wait_probe_done("the subagent probe", &server.log)
        .await;
    let files = ProbeFiles::read(&sandbox_dir);
    server.stop().await;
    environment.release(&harness, &session_id).await;
    (files, mock)
}

/// Whether a model request is the routing request a launch with no model
/// flag sends first: the router's system prompt wrapped around the same
/// prompt file the child will read, so it also carries the marker.
fn is_routing_request(request: &Value) -> bool {
    let routed = request
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("system")
                    && message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| content.contains("You route a subagent task"))
            })
        });
    routed && is_subagent_request(request)
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

/// A valid handoff document, the answer to a compaction request.
const COMPACTION_HANDOFF: &str = r#"{"current_task":"run the fork probe","key_decisions":[],"context_files":[],"errors_fixes":[],"pending_work":[],"recovery_hints":[],"skills":[]}"#;
/// How long the mock holds a compaction's answer back.
const COMPACTION_ANSWER_DELAY: Duration = Duration::from_secs(600);

/// Whether a model request asks for a compaction handoff: the conversation
/// followed by the compaction instruction and the schema it must answer to.
fn is_compaction_request(request: &Value) -> bool {
    request
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().rev().take(3).any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| {
                        content.contains("Your context window is being compacted")
                    })
            })
        })
}

/// A user turn long enough to put the main agent's context between the
/// refusal point (90% of the pause point, the window less the handoff
/// reserve) and the pause point.
fn near_compaction_user_turn() -> String {
    format!(
        "Run the fork probe. {}",
        "lorem ipsum ".repeat(FORK_FILLER_REPEATS)
    )
}

const FORK_FILLER_REPEATS: usize = 32_500;

#[tokio::test]
#[file_serial]
async fn a_fork_continues_the_main_agents_context() {
    let probe = run_probe_turn(MockChatScenario::ForkProbe, "Run the fork probe.").await;
    assert_fork_continues(probe).await;
}

/// The session runs on the balanced tier's model, which is not the configured
/// advanced model.
#[tokio::test]
#[file_serial]
async fn a_fork_on_a_non_advanced_session_model_succeeds() {
    let probe = run_probe_turn_on(
        MockChatScenario::ForkProbe,
        "Run the fork probe.",
        Some(TIER_BALANCED_MODEL),
    )
    .await;
    assert_fork_continues(probe).await;
}

/// The session's model belongs to none of the configured tiers.
#[tokio::test]
#[file_serial]
async fn a_fork_on_a_model_in_no_tier_succeeds() {
    let probe = run_probe_turn_on(
        MockChatScenario::ForkProbe,
        "Run the fork probe.",
        Some(TEST_MODEL),
    )
    .await;
    assert_fork_continues(probe).await;
}

async fn assert_fork_continues((files, mock): (ProbeFiles, MockOpenAiServer)) {
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
#[file_serial]
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
#[file_serial]
async fn subagent_cannot_write_tasks_md_but_the_main_agent_can() {
    // The fixture is well-formed free text under `## Notes`: the run loop
    // holds the turn open while tasks.md breaks its format or has open rows.
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: "printf -- '## Notes\\noriginal\\n' > tasks.md",
        launch: "subagent explore --prompt-file prompt.md",
        child_script: "printf -- 'hacked\\n' >> ../../tasks.md; echo CHILD_RC=$?",
        final_script: "cp tasks.md tasks.before; printf -- '## Notes\\noriginal\\nmain wrote\\n' > tasks.md; echo MAIN_FINAL",
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
        "## Notes\noriginal\n",
        "tasks.md changed while only the subagent had run"
    );
    assert_eq!(
        files.get("tasks.md"),
        "## Notes\noriginal\nmain wrote\n",
        "the main agent's write in the same session must succeed"
    );
}

#[tokio::test]
#[file_serial]
async fn subagent_without_a_commit_grant_cannot_commit_in_a_worktree() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: WORKTREE_SETUP,
        launch: "subagent general --prompt-file prompt.md --worktrees \"$PWD/wt\"",
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

/// `general` declares no tier, so the fast model picks one before the child
/// starts — the routed path is what overflows the tokio worker's default
/// 512 KiB stack on macOS without a configured worker stack. The result
/// reports the tier and model the routing picked.
#[tokio::test]
#[file_serial]
async fn general_is_routed_by_the_fast_model_and_reports_the_pick() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: ":",
        launch: "subagent general --prompt-file prompt.md",
        child_script: "echo CHILD_RC=0",
        final_script: "echo MAIN_FINAL",
    })
    .await;

    let requests = mock.agent_requests().await;
    let routing: Vec<&Value> = requests.iter().filter(|r| is_routing_request(r)).collect();
    assert_eq!(routing.len(), 1, "one routing request: {requests:?}");
    assert_eq!(
        routing[0]["model"], TIER_FAST_MODEL,
        "the fast model routes the brief"
    );
    let outputs = child_probe_outputs(&mock).await;
    assert_eq!(
        child_exit_code(&outputs),
        0,
        "the routed subagent's probe must succeed: {outputs:?}"
    );
    assert_eq!(
        child_request_models(&requests),
        [TIER_BALANCED_MODEL],
        "the child runs on the model the routing picked"
    );
    let result = files.get("sub.out");
    assert!(
        result.contains(&format!(
            "Subagent 'general' completed (balanced tier, model: {TIER_BALANCED_MODEL})"
        )),
        "the result must report the routing's pick: {result}"
    );
}

/// The models of the subagents' requests, in order, one entry per run of
/// consecutive requests on a model.
fn child_request_models(requests: &[Value]) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for request in requests
        .iter()
        .filter(|request| is_subagent_request(request) && !is_routing_request(request))
    {
        let model = request["model"]
            .as_str()
            .expect("request model")
            .to_string();
        if models.last() != Some(&model) {
            models.push(model);
        }
    }
    models
}

/// The names `subagent` offers.
const SUBAGENT_NAMES: [&str; 7] = [
    "explore", "research", "audit", "review", "plan", "general", "fork",
];

/// `subagent` alone runs nothing and fails with the list of subcommands.
#[tokio::test]
#[file_serial]
async fn subagent_without_a_subcommand_fails_with_the_list() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: ":",
        launch: "subagent; echo BARE_RC=$?",
        child_script: "echo CHILD_RC=0",
        final_script: "echo MAIN_FINAL",
    })
    .await;

    let result = files.get("sub.out");
    assert!(
        !result.contains("BARE_RC=0") && result.contains("BARE_RC="),
        "a bare `subagent` must fail: {result}"
    );
    for name in SUBAGENT_NAMES {
        assert!(
            result.contains(&format!("- {name}:")),
            "the failure must list `{name}`: {result}"
        );
    }
    assert!(
        mock.agent_requests()
            .await
            .iter()
            .all(|request| !is_subagent_request(request)),
        "no subagent may run"
    );
}

/// Every subcommand runs on its tier's model; only `general`, given a
/// worktree, writes outside its scratch directory, and only there.
#[tokio::test]
#[file_serial]
async fn each_subcommand_runs_on_its_model_and_only_general_writes_the_workspace() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: WORKTREE_SETUP,
        launch: "for name in explore research audit review plan; do\n  subagent $name --prompt-file prompt.md > sub.$name 2>&1\ndone\nsubagent general --prompt-file prompt.md --worktrees \"$PWD/wt\" > sub.general 2>&1",
        child_script: "echo x > ../../workspace.txt; echo WORKSPACE_RC=$?; echo y > scratch.txt; echo SCRATCH_RC=$?; echo z > ../../wt/f.txt; echo WORKTREE_RC=$?",
        final_script: "echo MAIN_FINAL",
    })
    .await;

    let requests = mock.agent_requests().await;
    assert_eq!(
        child_request_models(&requests),
        [TIER_FAST_MODEL, TIER_FLAGSHIP_MODEL, TIER_BALANCED_MODEL],
        "explore and research on the fast model, audit, review and plan on the flagship, general on the routed one"
    );
    for (name, tier) in [
        ("explore", "fast"),
        ("research", "fast"),
        ("audit", "advanced"),
        ("review", "advanced"),
        ("plan", "advanced"),
        ("general", "balanced"),
    ] {
        let result = files.get(&format!("sub.{name}"));
        assert!(
            result.contains(&format!("Subagent '{name}' completed ({tier} tier")),
            "{name} must complete on the {tier} tier: {result}"
        );
    }

    let outputs = child_probe_outputs(&mock).await;
    assert_eq!(outputs.len(), SUBAGENT_NAMES.len() - 1, "{outputs:?}");
    let code = |output: &str, key: &str| -> i32 {
        let start = output
            .find(key)
            .unwrap_or_else(|| panic!("no {key} in {output}"))
            + key.len();
        output[start..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("no code after {key} in {output}"))
    };
    for (index, output) in outputs.iter().enumerate() {
        let name = SUBAGENT_NAMES[index];
        assert_ne!(
            code(output, "WORKSPACE_RC="),
            0,
            "{name}'s write to the workspace must be refused: {output}"
        );
        assert_eq!(
            code(output, "SCRATCH_RC="),
            0,
            "{name}'s write to its scratch directory must succeed: {output}"
        );
        let worktree = code(output, "WORKTREE_RC=");
        if name == "general" {
            assert_eq!(worktree, 0, "general writes its granted worktree: {output}");
        } else {
            assert_ne!(worktree, 0, "{name} must not write a worktree: {output}");
        }
    }
    assert!(
        !files.0.contains_key("workspace.txt"),
        "no subagent may have written the session root"
    );
}

#[tokio::test]
#[file_serial]
async fn subagent_with_a_commit_grant_commits_and_the_main_agent_gets_the_listing() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: WORKTREE_SETUP,
        launch: "subagent general --prompt-file prompt.md --commit-worktrees \"$PWD/wt\"",
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
#[file_serial]
async fn daemon_error_logs_its_cause() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let profile_id = profile_id_via_api(&client, &server, "build").await;

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

    let logs = harness.server_logs(&server).await;
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
#[file_serial]
async fn ui_stream_stop_mid_response_persists_partial_assistant_text() {
    stream_stop_mid_response_persists_partial_assistant_text(ChatEnvironment::Local).await;
}

#[tokio::test]
#[file_serial]
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
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = environment.prepare(&harness, &client, &server).await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let chat_task = tokio::spawn({
        let client = client.clone();
        let base_url = server.base_url.clone();
        let log = server.log.clone();
        let session_id = session_id.clone();
        async move {
            let response = send_turn(
                &client,
                &base_url,
                &serde_json::json!({
                    "sessionId": session_id,
                    "content": "Say several chunks slowly."
                }),
            )
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
        .delete(format!(
            "{}/api/sessions/{}/turns/current",
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

    let transcript_response = client
        .get(format!(
            "{}/api/sessions/{}/transcript",
            server.base_url, session_id
        ))
        .send()
        .await
        .expect("get transcript after cancel should succeed");
    let transcript_response = expect_status(&server.log, transcript_response, StatusCode::OK).await;
    let transcript: Value = transcript_response
        .json()
        .await
        .expect("transcript after cancel should be valid JSON");
    let assistant_messages = extract_assistant_messages(&transcript);
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
#[file_serial]
async fn ui_chat_executes_terminal_and_terminal_input_round_trip() {
    terminal_and_terminal_input_round_trip(ChatEnvironment::Local).await;
}

/// The round trip runs its commands inside the session's container — they
/// start in the `/workspace` mount and reach host IPC commands through the
/// container's bridge route — and the model sees the container environment
/// context.
#[tokio::test]
#[file_serial]
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
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = environment.prepare(&harness, &client, &server).await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let chat_task = tokio::spawn({
        let client = client.clone();
        let base_url = server.base_url.clone();
        let log = server.log.clone();
        let session_id = session_id.clone();
        async move {
            let response = send_turn(
                &client,
                &base_url,
                &serde_json::json!({
                    "sessionId": session_id,
                    "content": "Please ask the terminal for my name and answer with the result."
                }),
            )
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
        first_request_system.contains(context_heading.as_str()),
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

/// A turn sent after the provider's cache lifetime that reads nothing of
/// what the previous turn cached is recorded as one expired-cache miss, and
/// `/usage` reports it; the warm turns before it record none.
#[tokio::test]
#[file_serial]
async fn an_expired_prompt_cache_is_recorded_and_reported_by_usage() {
    const LIFETIME: Duration = Duration::from_secs(2);
    let mock = MockOpenAiServer::spawn(MockChatScenario::CachedTextEveryTurn {
        cached: vec![0, 3_000, 0],
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let config_path = harness.may_dir.join("config.toml");
    let mut config = MayConfig::load_from(&config_path)
        .await
        .expect("harness config should load");
    config
        .set_provider_cache_lifetime(TEST_PROVIDER_ID, "2s")
        .expect("the lifetime parses");
    config
        .save_to(&config_path)
        .await
        .expect("harness config should save");
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    for (turn, content) in ["First turn.", "Second turn.", "After the lifetime."]
        .into_iter()
        .enumerate()
    {
        if turn == 2 {
            sleep(LIFETIME + Duration::from_millis(500)).await;
        }
        let response = send_turn(
            &client,
            &server.base_url,
            &serde_json::json!({ "sessionId": session_id, "content": content }),
        )
        .await
        .expect("chat stream request should succeed");
        let response = expect_status(&server.log, response, StatusCode::OK).await;
        let events = tokio::time::timeout(Duration::from_secs(60), collect_sse_events(response))
            .await
            .expect("chat stream should finish before timeout");
        assert!(
            events.iter().any(|(kind, _)| kind == "done"),
            "{content:?} should complete; events: {events:?}; server logs:\n{}",
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

    let response = client
        .get(format!(
            "{}/api/sessions/{session_id}/usage",
            server.base_url
        ))
        .send()
        .await
        .expect("usage request should succeed");
    let usage: Value = expect_status(&server.log, response, StatusCode::OK)
        .await
        .json()
        .await
        .expect("usage should be JSON");
    assert_eq!(
        usage["cacheMissCauses"],
        // The mock reports no writes, so the second request is credited with
        // its prompt rounded down to the route's block, inferred from its one
        // read of 3,000 tokens; the expired cache lost all of it.
        serde_json::json!([{ "cause": "expired", "misses": 1, "missedTokens": 3_000 }]),
        "usage: {usage}; server logs:\n{}",
        server.log.contents().await
    );
    assert_eq!(usage["recentCacheMisses"][0]["cause"], "expired");
    assert_eq!(usage["models"][0]["cacheMisses"], 1);

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

#[tokio::test]
#[file_serial]
async fn ui_chat_turns_on_one_session_prepare_tools_once() {
    chat_turns_on_one_session_prepare_tools_once(ChatEnvironment::Local).await;
}

#[tokio::test]
#[file_serial]
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
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
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
        let response = send_turn(
            &client,
            &server.base_url,
            &serde_json::json!({
                "sessionId": session_id,
                "content": content
            }),
        )
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

    let logs = harness.server_logs(&server).await;
    let ready_lines = logs
        .lines()
        .filter(|line| line.contains("session tool environments ready"))
        .filter(|line| line.contains(session_id.as_str()))
        .collect::<Vec<_>>();
    let prepared = ready_lines
        .iter()
        .filter(|line| line.contains("outcome=\"prepared\""))
        .count();
    // Later turns continue with the session's idle agent, or adopt the
    // prepared tools when it had to be rebuilt.
    assert_eq!(
        prepared, 1,
        "only the first turn should prepare tools; readiness logs: {ready_lines:#?}"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A burst of upstream 529s that outlasts the `OpenAI` client's own retries
/// (the first request plus `OPENAI_CLIENT_RETRIES`) is retried by the turn on
/// the same provider: it completes without an error after one announced wait,
/// and the failed attempt never reaches history.
#[tokio::test]
#[file_serial]
async fn ui_chat_turn_survives_upstream_overload_on_its_own_provider() {
    // The aither OpenAI client retries a retryable failure three times
    // before it surfaces to the turn.
    const OPENAI_CLIENT_RETRIES: usize = 3;
    const FAILURES: usize = OPENAI_CLIENT_RETRIES + 1;
    let mock = MockOpenAiServer::spawn(MockChatScenario::OverloadedThenText {
        failures: FAILURES,
        text: "recovered answer".to_string(),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": "Answer despite the overload."
        }),
    )
    .await
    .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = tokio::time::timeout(Duration::from_secs(90), collect_sse_events(response))
        .await
        .expect("the turn should finish after its retries");

    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "turn should complete; events: {events:?}"
    );
    assert!(
        events.iter().all(|(kind, _)| kind != "error"),
        "no error should reach the client; events: {events:?}"
    );
    let holds = events.iter().filter(|(kind, _)| kind == "hold").count();
    assert_eq!(holds, 1, "the turn's retry announces its wait: {events:?}");
    assert_eq!(mock.agent_requests().await.len(), FAILURES + 1);

    let transcript = client
        .get(format!(
            "{}/api/sessions/{session_id}/transcript",
            server.base_url
        ))
        .send()
        .await
        .expect("get transcript request should succeed");
    let transcript: Value = expect_status(&server.log, transcript, StatusCode::OK)
        .await
        .json()
        .await
        .expect("transcript should be valid JSON");
    assert_eq!(
        extract_assistant_messages(&transcript),
        vec!["recovered answer".to_string()],
        "the failed attempts leave nothing in history: {transcript}"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
#[file_serial]
async fn ui_linux_runtime_has_heel_env_for_sandbox_stdin_detection() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::TerminalRoundTrip).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;

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
#[file_serial]
async fn exec_applies_local_command_priority() {
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
        let binary_path = binary.clone();
        let output = blocking::unblock(move || {
            std::process::Command::new(&binary_path)
                .arg("exec")
                .arg("--session")
                .arg(&session_id)
                .arg("--mode")
                .arg("default")
                .arg("--")
                .arg(&script)
                .env("HOME", &home)
                .envs(hermetic_git_env(&home))
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

/// The acceptance script of the managed baseline tools. It ends by printing
/// the line [`BASELINE_ACCEPTANCE_PASSED`] and prints the managed `uv` and
/// `python3` versions for the test to compare with the pins.
const BASELINE_TOOLS_SCRIPT: &str = include_str!("fixtures/verify_baseline_tools.sh");
const BASELINE_ACCEPTANCE_PASSED: &str = "baseline acceptance passed";

/// The version the embedded distribution pins for `key` of `entries` on this
/// host, the `uv` artifact or the managed Python.
fn pinned_version(entries: &str, tool: Option<&str>) -> String {
    let manifest =
        may_agent::tools::manifest::combined_manifest().expect("the embedded distribution is JSON");
    let target = ToolTarget::native().expect("test host should have a tool target");
    manifest[entries]
        .as_array()
        .expect("the distribution lists its entries")
        .iter()
        .find(|entry| {
            entry["target"]["os"] == target.os()
                && entry["target"]["arch"] == target.arch()
                && tool.is_none_or(|tool| entry["tool"] == tool)
        })
        .and_then(|entry| entry["version"].as_str())
        .unwrap_or_else(|| panic!("the distribution pins {entries} {tool:?} for this host"))
        .to_owned()
}

/// Runs the baseline-tools acceptance script through the real sandboxed
/// terminal of `may exec` against the real managed tools, which the harness
/// downloads and prepares from the embedded distribution. Run by the
/// `verify-tools` CI job on Linux and macOS, which restores the downloads
/// from a cache keyed on the pins.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
#[file_serial]
#[ignore = "downloads the pinned tools; run by the verify-tools CI job"]
async fn the_baseline_tools_pass_their_acceptance_script_in_the_sandbox() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let session = harness.create_session().await;
    let home = harness.home_path().to_path_buf();
    let session_id = session.id.clone();
    let binary = may_binary_path();
    let output = blocking::unblock(move || {
        std::process::Command::new(&binary)
            .arg("exec")
            .arg("--session")
            .arg(&session_id)
            .arg("--mode")
            .arg("default")
            .arg("--")
            .arg(BASELINE_TOOLS_SCRIPT)
            .env("HOME", &home)
            .envs(hermetic_git_env(&home))
            .output()
    })
    .await
    .expect("may exec should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains(BASELINE_ACCEPTANCE_PASSED),
        "the acceptance script failed: stdout={stdout} stderr={stderr}"
    );
    if ToolTarget::native()
        .expect("test host should have a tool target")
        .provides_python()
    {
        let uv = pinned_version("artifacts", Some("uv"));
        let python = pinned_version("python", None);
        assert!(
            stdout.contains(&format!("uv {uv} ")),
            "uv {uv} missing: {stdout}"
        );
        assert!(
            stdout.contains(&format!("Python {python}")),
            "Python {python} missing: {stdout}"
        );
    }
}

/// A command of a container session runs at the configured nice level, with
/// the tools mount on its `PATH`, and reaches May's IPC commands through the
/// relayed endpoint. The level is measured against the same session's
/// unprioritised baseline, as in [`exec_applies_local_command_priority`].
/// Needs Docker, which the Ubuntu runners of the `container-acceptance` CI
/// job have.
#[cfg(target_os = "linux")]
#[tokio::test]
#[file_serial]
#[ignore = "needs Docker; run by the container-acceptance CI job"]
async fn a_container_command_runs_at_the_configured_nice_and_reaches_ipc() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::TextEveryTurn("unused".to_string())).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Container
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let binary = may_binary_path();

    let mut observed = Vec::new();
    for priority in [CommandPriority::Normal, CommandPriority::Low] {
        let mut config = MayConfig::load_from(&harness.may_dir.join("config.toml"))
            .await
            .expect("harness config should load");
        config.sandbox.command_priority = priority;
        config
            .save_to(&harness.may_dir.join("config.toml"))
            .await
            .expect("harness config should save");

        let home = harness.home_path().to_path_buf();
        let binary = binary.clone();
        let session_id = session_id.clone();
        let output = blocking::unblock(move || {
            std::process::Command::new(&binary)
                .arg("exec")
                .arg("--session")
                .arg(&session_id)
                .arg("--mode")
                .arg("default")
                .arg("--")
                .arg(
                    "printf 'nice=%s\\n' \"$(nice)\"; jq --version; \
                     session --help >/dev/null; printf 'ipc_exit=%s\\n' \"$?\"",
                )
                .env("HOME", &home)
                .envs(hermetic_git_env(&home))
                .output()
        })
        .await
        .expect("may exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        println!("container script output at {priority:?}:\n{stdout}");
        assert!(
            output.status.success() && stdout.contains("ipc_exit=0"),
            "the container command at {priority:?} failed: stdout={stdout} stderr={}; server logs:\n{}",
            String::from_utf8_lossy(&output.stderr),
            server.log.contents().await
        );
        assert!(
            stdout.contains("jq-"),
            "a prepared tool should run from the tools mount: {stdout}"
        );
        let nice: i32 = stdout
            .lines()
            .find_map(|line| line.strip_prefix("nice="))
            .and_then(|nice| nice.trim().parse().ok())
            .unwrap_or_else(|| panic!("the script should print its nice level: {stdout}"));
        observed.push(nice);
    }
    // Low maps to nice +10, which the kernel clamps at 19.
    assert_eq!(
        observed[1],
        (observed[0] + 10).min(19),
        "the container command should run at the configured nice level: {observed:?}"
    );

    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A chat turn against a provider nobody listens on must end with the
/// provider's error on the stream, and the server must survive it.
#[tokio::test]
#[file_serial]
async fn ui_chat_reports_unreachable_provider_error() {
    let harness = TestHarness::new("http://127.0.0.1:9/v1").await;
    let mut server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": "Is anyone there?"
        }),
    )
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
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
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
#[file_serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn container_profile_inline_script_task_reads_its_trigger_file() {
    container_task_reads_its_trigger_file(TaskScriptSource::Inline).await;
}

#[tokio::test]
#[file_serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn container_profile_script_file_task_reads_its_trigger_file() {
    container_task_reads_its_trigger_file(TaskScriptSource::File).await;
}

/// Answers `request_id` through the API with `value` selected.
async fn answer_open_question(
    client: &reqwest::Client,
    server: &RunningDaemon,
    request_id: &str,
    value: &str,
) {
    let response = client
        .post(format!("{}/api/interactions/{request_id}", server.base_url))
        .json(&serde_json::json!({
            "kind": "askUser",
            "answers": [{
                "section": "",
                "answer": { "kind": "selected", "values": [value] }
            }]
        }))
        .send()
        .await
        .expect("ask-user respond request should succeed");
    expect_status(&server.log, response, StatusCode::OK).await;
}

/// The id of the single open question in the runtime summary.
fn only_open_question_id(runtime: &Value) -> String {
    let open = runtime
        .get("openQuestions")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("runtime has no open questions: {runtime}"));
    assert_eq!(open.len(), 1, "exactly one open question: {runtime}");
    open[0]
        .get("id")
        .and_then(Value::as_str)
        .expect("open question has an id")
        .to_string()
}

/// `ask_user` returns at once and the run ends without an answer; the
/// question stays open in the runtime summary, and an answer after the run is
/// committed and then starts a follow-up run that sees its text.
#[tokio::test]
#[file_serial]
async fn ask_user_after_the_run_ended_delivers_the_answer_to_a_new_run() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::AsksUser).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": "Pick a database."
        }),
    )
    .await
    .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;
    let events = collect_turn_events(response, "the asking turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the run must finish without waiting for the answer; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );
    let runtime = wait_for_runtime_condition(
        &client,
        &server,
        &session_id,
        Duration::from_secs(10),
        "the run to end with the question still open",
        |runtime| runtime_is_idle(runtime) && runtime.get("openQuestions").is_some(),
    )
    .await;
    let request_id = only_open_question_id(&runtime);
    let before = mock.agent_requests().await;
    assert!(
        !serde_json::to_string(&before)
            .expect("requests serialize")
            .contains("Answer: PostgreSQL"),
        "no answer exists yet"
    );

    answer_open_question(&client, &server, &request_id, "PostgreSQL").await;

    let deadline = Instant::now() + Duration::from_secs(30);
    let delivered = loop {
        let requests = mock.agent_requests().await;
        if requests.len() > before.len() {
            break requests[before.len()].to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no follow-up run started within 30s of the answer; server logs:\n{}",
            server.log.contents().await
        );
        sleep(Duration::from_millis(100)).await;
    };
    assert!(
        delivered.contains("answered your ask_user question")
            && !delivered.contains(&request_id)
            && delivered.contains("Answer: PostgreSQL"),
        "the follow-up request must name the question by its word and carry the answer: {delivered}"
    );
    wait_for_runtime_condition(
        &client,
        &server,
        &session_id,
        Duration::from_secs(20),
        "the follow-up run to end with no question open",
        |runtime| {
            runtime_is_idle(runtime)
                && runtime
                    .get("openQuestions")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty)
        },
    )
    .await;

    server.stop().await;
}

/// An answer that arrives while the run is still in flight is committed
/// first and reaches the model in that run's next request, not the one
/// already sent.
#[tokio::test]
#[file_serial]
async fn ask_user_answered_mid_run_reaches_the_next_request_of_that_run() {
    // Agent request 0 asks; request 1 follows the command result and is held.
    let gate = RequestGate::holding(1);
    let mock = MockOpenAiServer::spawn_gated(MockChatScenario::AsksUser, Some(gate.clone())).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let response = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": "Pick a database."
        }),
    )
    .await
    .expect("chat stream request should succeed");
    let response = expect_status(&server.log, response, StatusCode::OK).await;

    tokio::time::timeout(Duration::from_secs(60), gate.arrived.notified())
        .await
        .unwrap_or_else(|_| panic!("the run never sent the request after the ask_user command"));
    let runtime = get_runtime(&client, &server, &session_id).await;
    let request_id = only_open_question_id(&runtime);
    let held = mock.agent_requests().await.len();

    answer_open_question(&client, &server, &request_id, "PostgreSQL").await;
    gate.release.notify_one();

    let events = collect_turn_events(response, "the interrupted turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let next = loop {
        let requests = mock.agent_requests().await;
        if requests.len() > held {
            break requests[held].to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no request carried the answer within 30s; server logs:\n{}",
            server.log.contents().await
        );
        sleep(Duration::from_millis(100)).await;
    };
    let held_request = mock.agent_requests().await[held - 1].to_string();
    assert!(
        !held_request.contains("Answer: PostgreSQL"),
        "the request already in flight predates the answer"
    );
    assert!(
        next.contains("answered your ask_user question")
            && !next.contains(&request_id)
            && next.contains("Answer: PostgreSQL"),
        "the next request must name the question by its word and carry the answer: {next}"
    );

    server.stop().await;
}

/// Gives the harness home a daemon port of its own and a local default
/// profile, so a one-shot run starts a daemon that meets no other.
async fn prepare_oneshot_home(harness: &TestHarness) {
    // A port of its own, so the daemon this run starts never meets another.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free port should be found")
        .port();
    let config_path = harness.home_path().join(".may/config.toml");
    let mut config: toml::Table = toml::from_str(
        &tokio::fs::read_to_string(&config_path)
            .await
            .expect("test config should be readable"),
    )
    .expect("test config should parse");
    let mut daemon = toml::Table::new();
    daemon.insert("port".into(), toml::Value::Integer(i64::from(port)));
    config.insert("daemon".into(), toml::Value::Table(daemon));
    tokio::fs::write(
        &config_path,
        toml::to_string(&config).expect("test config should serialize"),
    )
    .await
    .expect("test config should be written");

    // One-shot runs use the default profile, a container one; this host may
    // have no container runtime, so the profile runs locally instead.
    let db = may_agent::db::ChatDb::open(&harness.home_path().join(".may/history.db"))
        .await
        .expect("test database should open");
    let default_profile = db
        .ensure_default_profiles()
        .await
        .expect("default profiles should exist");
    db.update_profile(
        &default_profile,
        may_agent::db::UpdateProfileParams {
            name: None,
            icon: None,
            environment: Some(may_agent::db::ProfileEnvironmentKind::Local),
            container_id: None,
            sort_order: None,
            persona: None,
            preferences: None,
            location_aware: None,
        },
    )
    .await
    .expect("default profile should switch to local");
    drop(db);
}

/// A one-shot run whose turn ends on a detached command stays attached,
/// streams the turn that command's result starts, and exits only after it.
#[tokio::test]
#[file_serial]
async fn oneshot_run_follows_the_turn_a_background_command_resumes() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(FINISHING_JOB_SCRIPT)).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    prepare_oneshot_home(&harness).await;

    let binary = may_binary_path();
    let output = Command::new(&binary)
        .arg("start the job")
        .env("HOME", harness.home_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("may one-shot should spawn")
        .wait_with_output()
        .await
        .expect("may one-shot should run");
    let daemon_log = tokio::fs::read_to_string(harness.home_path().join(".may/logs/daemon.log"))
        .await
        .unwrap_or_default();
    // The daemon outlives the run so that the check above reads its log
    // before anything stops it.
    let stop = Command::new(&binary)
        .args(["daemon", "stop"])
        .env("HOME", harness.home_path())
        .output()
        .await
        .expect("may daemon stop should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "one-shot run failed: {stderr}\ndaemon log:\n{daemon_log}"
    );
    // The mock answers the first turn and the resumed one with the same
    // text, so a run that left before the resume carries it once.
    assert_eq!(
        stdout.matches("started").count(),
        2,
        "stdout should carry the first turn's text and the resumed turn's; stdout: {stdout}; stderr: {stderr}\ndaemon log:\n{daemon_log}"
    );
    let requests = mock.agent_requests().await;
    assert!(
        requests
            .last()
            .is_some_and(|request| request.to_string().contains(FINISHING_JOB_MARKER)),
        "the resumed turn should have seen the job's output before the run exited"
    );
    assert!(
        !daemon_log.contains("received session stop request"),
        "nothing should stop the session before the run exits:\n{daemon_log}"
    );
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("Stopped"),
        "the run should leave its daemon running: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

#[tokio::test]
#[file_serial]
async fn oneshot_run_starts_the_daemon_and_streams_its_turn() {
    // The reply pauses longer than a model's typical think after its first
    // text; the run still ends only when the turn does.
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowTextEveryTurn {
        chunks: vec!["hello from ".into(), "the daemon".into()],
        delay: Duration::from_secs(3),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    prepare_oneshot_home(&harness).await;

    let binary = may_binary_path();
    let oneshot = Command::new(&binary)
        .arg("say hi")
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("may one-shot should spawn");
    // The repo-trust gate asks only on an interactive stdin; a piped
    // one-shot proceeds unbound.
    let output = oneshot
        .wait_with_output()
        .await
        .expect("may one-shot should run");
    let stop = Command::new(&binary)
        .args(["daemon", "stop"])
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
        .output()
        .await
        .expect("may daemon stop should run");

    assert!(
        output.status.success(),
        "one-shot run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("hello from the daemon"),
        "stdout should carry the turn's text: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("Stopped"),
        "the one-shot run should leave its daemon running: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

/// A turn that streams far more deltas than the client reads at once, and
/// ends right after them, reaches a one-shot run whole: the run prints every
/// delta through the turn's end however far its event stream falls behind.
#[tokio::test]
#[file_serial]
async fn oneshot_run_prints_a_burst_turn_through_its_end() {
    const DELTAS: usize = 6_000;
    let chunks = (0..DELTAS)
        .map(|index| format!("d{index} "))
        .collect::<Vec<_>>();
    let expected = chunks.concat();
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowTextEveryTurn {
        chunks,
        delay: Duration::ZERO,
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    prepare_oneshot_home(&harness).await;

    let binary = may_binary_path();
    let output = Command::new(&binary)
        .arg("stream a long reply")
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("may one-shot should spawn")
        .wait_with_output()
        .await
        .expect("may one-shot should run");
    let daemon_log = tokio::fs::read_to_string(harness.home_path().join(".may/logs/daemon.log"))
        .await
        .unwrap_or_default();
    let stop = Command::new(&binary)
        .args(["daemon", "stop"])
        .env("HOME", harness.home_path())
        .envs(hermetic_git_env(harness.home_path()))
        .output()
        .await
        .expect("may daemon stop should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "one-shot run failed: {stderr}\ndaemon log:\n{daemon_log}"
    );
    assert!(
        stdout.contains(&expected),
        "stdout should carry all {DELTAS} deltas in order; it ends with {:?}\ndaemon log:\n{daemon_log}",
        stdout
            .char_indices()
            .rev()
            .nth(80)
            .map_or(&*stdout, |(at, _)| &stdout[at..])
    );
    assert!(
        !daemon_log.contains("turn stream lagged"),
        "no subscriber should lose turn frames:\n{daemon_log}"
    );
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("Stopped"),
        "the one-shot run should leave its daemon running: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

/// Whether launchd has `label` loaded in the invoking user's domain.
#[cfg(target_os = "macos")]
fn launchd_job_loaded(label: &str) -> bool {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    std::process::Command::new("/bin/launchctl")
        .args(["print", &format!("user/{uid}/{label}")])
        .output()
        .expect("launchctl print should run")
        .status
        .success()
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[file_serial]
async fn temp_home_daemon_leaves_no_repo_search_job_after_it_stops() {
    use repo_search_client::launcher::launchd::labels_for;

    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowTextEveryTurn {
        chunks: vec!["hi".into()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let home = harness.home_path().to_path_buf();
    let login = dirs::home_dir().expect("login home should resolve");
    let (daemon_label, server_label) = labels_for(&home, &login);
    assert!(
        !launchd_job_loaded(&daemon_label) && !launchd_job_loaded(&server_label),
        "a fresh temp home owns no job"
    );

    let server = RunningDaemon::spawn(&home, &[]).await;
    install_repo_search_agents(&server, &daemon_label, &server_label).await;

    server.stop().await;
    assert!(
        !launchd_job_loaded(&daemon_label) && !launchd_job_loaded(&server_label),
        "stopping the daemon should boot out the relocated home's agents"
    );
}

/// Makes `server` install its relocated home's repo-search agents and, when
/// launchd accepted them, asserts both are loaded. Returns whether they are.
#[cfg(target_os = "macos")]
async fn install_repo_search_agents(
    server: &RunningDaemon,
    daemon_label: &str,
    server_label: &str,
) -> bool {
    // An index-coverage poll on a git worktree reaches repo-search, which
    // installs the home's agents.
    let worktree = tempfile::tempdir().expect("worktree tempdir should be created");
    git2::Repository::init(worktree.path()).expect("worktree should initialise");
    let cwd = dunce::canonicalize(worktree.path()).expect("worktree should resolve");
    let client = reqwest::Client::new();
    let events = client
        .get(format!("{}/api/events", server.base_url))
        .query(&[
            ("topics", "session_index"),
            ("cwd", cwd.to_str().expect("utf-8 path")),
        ])
        .send()
        .await
        .expect("event stream should open");
    let mut reader = SseReader::new(events);
    let snapshot = loop {
        let event = tokio::time::timeout(Duration::from_secs(120), reader.next_event())
            .await
            .expect("an index snapshot should arrive")
            .expect("the stream should stay open");
        if event.event == "session_index" {
            break event.data;
        }
    };
    drop(reader);
    // A launchd that refuses the bootstrap (a port or the session already
    // taken) leaves nothing to boot out; the callers' checks then still hold.
    let installed =
        serde_json::from_str::<Value>(&snapshot).expect("snapshot is JSON")["error"].is_null();
    if installed {
        assert!(
            launchd_job_loaded(daemon_label) && launchd_job_loaded(server_label),
            "the temp-home daemon should have installed its repo-search agents; snapshot: {snapshot}"
        );
    }
    installed
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[file_serial]
async fn a_killed_temp_home_daemons_agents_are_swept_by_the_next_daemon_start() {
    use repo_search_client::launcher::launchd::{LABEL, SERVER_LABEL, labels_for};

    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowTextEveryTurn {
        chunks: vec!["hi".into()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let home = harness.home_path().to_path_buf();
    let login = dirs::home_dir().expect("login home should resolve");
    let (daemon_label, server_label) = labels_for(&home, &login);
    let production_before = (launchd_job_loaded(LABEL), launchd_job_loaded(SERVER_LABEL));

    // Built before the kill: building a harness sweeps too, which would hide
    // the daemon's own sweep.
    let next = TestHarness::new(&mock.base_url()).await;
    assert_ne!(next.home_path(), home, "the next daemon has a fresh home");

    // SIGKILL: no shutdown path runs, so the agents outlive the daemon.
    let killed = RunningDaemon::spawn(&home, &[]).await;
    let installed = install_repo_search_agents(&killed, &daemon_label, &server_label).await;
    killed.kill().await;
    if installed {
        assert!(
            launchd_job_loaded(&daemon_label) && launchd_job_loaded(&server_label),
            "a killed daemon leaves its agents loaded"
        );
    }

    // The home is deleted with the agents still registered.
    std::fs::remove_dir_all(&home).expect("the killed daemon's home should be deleted");

    // The next daemon, under a home of its own, sweeps the dead home's agents.
    let successor = RunningDaemon::spawn(next.home_path(), &[]).await;
    assert!(
        !launchd_job_loaded(&daemon_label) && !launchd_job_loaded(&server_label),
        "the next daemon start should boot out the deleted home's agents"
    );
    successor.stop().await;

    assert_eq!(
        production_before,
        (launchd_job_loaded(LABEL), launchd_job_loaded(SERVER_LABEL)),
        "production agents are untouched"
    );
}

/// The session's background work, as `GET /api/sessions/{id}/background`
/// reports it.
async fn background_of(
    client: &reqwest::Client,
    server: &RunningDaemon,
    session_id: &str,
) -> Value {
    let response = client
        .get(format!(
            "{}/api/sessions/{session_id}/background",
            server.base_url
        ))
        .send()
        .await
        .expect("background request should succeed");
    expect_status(&server.log, response, StatusCode::OK)
        .await
        .json()
        .await
        .expect("background payload should be JSON")
}

fn background_job<'a>(background: &'a Value, script: &str) -> Option<&'a Value> {
    background
        .get("jobs")
        .and_then(Value::as_array)?
        .iter()
        .find(|job| job.get("script").and_then(Value::as_str) == Some(script))
}

#[tokio::test]
#[file_serial]
async fn a_background_job_is_listed_pushed_and_killable_over_the_api() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(BACKGROUND_JOB_SCRIPT)).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    // The session stream replays deltas published after this cursor.
    let session: Value = client
        .get(format!("{}/api/sessions/{session_id}", server.base_url))
        .send()
        .await
        .expect("session request should succeed")
        .json()
        .await
        .expect("session JSON");
    let cursor = session
        .get("cursor")
        .and_then(Value::as_str)
        .expect("the session read carries its event cursor")
        .to_string();

    // The turn ends with the model's reply; its job keeps running.
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "start the job" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the background-job turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should end while its job runs; events: {events:?}"
    );
    let background = background_of(&client, &server, &session_id).await;
    assert_eq!(
        background_job(&background, BACKGROUND_JOB_SCRIPT)
            .and_then(|job| job.pointer("/status/state"))
            .and_then(Value::as_str),
        Some("running"),
        "the job outlives its turn: {background}; server logs:\n{}",
        server.log.contents().await
    );

    // The session stream reports the job as a `job.upserted` delta.
    let events = client
        .get(format!(
            "{}/api/sessions/{session_id}/events?after={cursor}",
            server.base_url
        ))
        .send()
        .await
        .expect("event stream should open");
    let mut reader = SseReader::new(events);
    let pid = loop {
        let event = tokio::time::timeout(Duration::from_secs(10), reader.next_event())
            .await
            .expect("a job.upserted event should arrive")
            .expect("the stream should stay open");
        if event.event != "job.upserted" {
            continue;
        }
        let delta: Value = serde_json::from_str(&event.data).expect("upsert JSON");
        let Some(pid) = delta
            .get("job")
            .filter(|job| job.get("script").and_then(Value::as_str) == Some(BACKGROUND_JOB_SCRIPT))
            .and_then(|job| job.get("pid"))
            .and_then(Value::as_u64)
        else {
            continue;
        };
        break pid;
    };

    let response = client
        .post(format!(
            "{}/api/sessions/{session_id}/jobs/{pid}/kill",
            server.base_url
        ))
        .send()
        .await
        .expect("kill request should succeed");
    let killed: Value = expect_status(&server.log, response, StatusCode::OK)
        .await
        .json()
        .await
        .expect("kill payload should be JSON");
    assert_eq!(killed.get("ok"), Some(&Value::Bool(true)), "{killed}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let background = background_of(&client, &server, &session_id).await;
        if background_job(&background, BACKGROUND_JOB_SCRIPT)
            .and_then(|job| job.pointer("/status/state"))
            .and_then(Value::as_str)
            == Some("killed")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the killed job should read as killed: {background}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    server.stop().await;
}

/// A background job that finishes after its turn ended starts a turn of its
/// own, in which the model sees the job's output; meanwhile the session is
/// idle, so the user is free to send messages.
#[tokio::test]
#[file_serial]
async fn a_background_job_finishing_after_its_turn_resumes_the_session() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(FINISHING_JOB_SCRIPT)).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "start the job" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn starting the job", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should end while its job runs; events: {events:?}"
    );
    let requests_after_first_turn = mock.agent_requests().await.len();

    // The resumed turn shows up on the session's stream.
    let stream = client
        .get(format!(
            "{}/api/sessions/{session_id}/events",
            server.base_url
        ))
        .send()
        .await
        .expect("event stream should open");
    let runtime: Value = client
        .get(format!(
            "{}/api/sessions/{session_id}/runtime",
            server.base_url
        ))
        .send()
        .await
        .expect("runtime request should succeed")
        .json()
        .await
        .expect("runtime JSON");
    assert_ne!(
        runtime.get("state").and_then(Value::as_str),
        Some("running"),
        "the session is idle while the job runs: {runtime}"
    );
    let resumed = collect_turn_events(stream, "the resumed turn", &server.log).await;
    assert!(
        resumed.iter().any(|(kind, _)| kind == "done"),
        "the job's completion should resume the session; events: {resumed:?}; server logs:\n{}",
        server.log.contents().await
    );

    let requests = mock.agent_requests().await;
    assert!(
        requests.len() > requests_after_first_turn,
        "the resumed turn asked the model"
    );
    let resumed_request = requests.last().expect("a resumed request").to_string();
    assert!(
        resumed_request.contains(FINISHING_JOB_MARKER),
        "the model sees the job's output: {resumed_request}"
    );
    server.stop().await;
}

/// Opens the session's events stream, continuing after the cursor `after`
/// when given. Frames of every kind arrive; readers pick their own.
async fn agent_frames(
    client: &reqwest::Client,
    server: &RunningDaemon,
    session_id: &str,
    after: Option<String>,
) -> SseReader {
    let mut url = format!("{}/api/sessions/{session_id}/events", server.base_url);
    if let Some(after) = after {
        url = format!("{url}?after={after}");
    }
    let response = client
        .get(url)
        .send()
        .await
        .expect("event stream should open");
    SseReader::new(response)
}

/// The next `session_agent` frame of `reader` — the kind the old
/// `topics=session_agent` subscription delivered — or a `reset`. Other
/// session frames (turns, suggestions, jobs) share the stream now and are
/// skipped here; the reader's cursor bookkeeping still uses the last frame
/// it read, so a reconnect loses nothing.
async fn next_turn_frame(reader: &mut SseReader) -> TestSseEvent {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), reader.next_event())
            .await
            .expect("a frame should arrive")
            .expect("the stream should stay open");
        if event.event == "session_agent" || event.event == "reset" {
            return event;
        }
    }
}

fn frame_cursor(event: &TestSseEvent) -> String {
    event
        .id
        .clone()
        .unwrap_or_else(|| panic!("frames carry their cursor: {event:?}"))
}

fn frame_index(event: &TestSseEvent) -> u64 {
    event
        .id
        .as_deref()
        .and_then(|id| id.rsplit_once('.'))
        .and_then(|(_, index)| index.parse().ok())
        .unwrap_or_else(|| panic!("turn frames carry their index: {event:?}"))
}

/// A client that reconnects mid-turn names the last frame it saw and gets
/// only the frames after it; one that missed frames of a turn that already
/// ended is told to reload the session.
#[tokio::test]
#[file_serial]
async fn a_reconnecting_client_continues_after_the_last_frame_it_saw() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: (0..12).map(|index| format!("chunk-{index} ")).collect(),
        delay: Duration::from_millis(150),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let mut first = agent_frames(&client, &server, &session_id, None).await;
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "write slowly" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;

    // Read the start and a few chunks, then drop the stream mid-turn.
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(next_turn_frame(&mut first).await);
    }
    assert!(seen[0].data.contains("\"start\""), "{:?}", seen[0]);
    let last_seen = frame_cursor(seen.last().expect("frames were read"));
    let last_index = frame_index(seen.last().expect("frames were read"));
    drop(first);

    let mut reopened = agent_frames(&client, &server, &session_id, Some(last_seen)).await;
    let next = next_turn_frame(&mut reopened).await;
    // Indices count every frame on the session stream; the next agent
    // frame may skip the indices of the kinds this test filtered out.
    assert!(
        frame_index(&next) > last_index,
        "the reopened stream continues after the last frame seen: {next:?}"
    );
    assert!(
        !next.data.contains("\"start\""),
        "the turn is not replayed from its start: {next:?}"
    );

    let events = collect_turn_events(turn, "the slow turn", &server.log).await;
    assert!(events.iter().any(|(kind, _)| kind == "done"), "{events:?}");

    // The turn ended: frames after the first one are gone for good.
    // A cursor the hub cannot continue from — a foreign epoch — gets `reset`.
    let mut stale = agent_frames(&client, &server, &session_id, Some("1.0".to_string())).await;
    let resync = next_turn_frame(&mut stale).await;
    assert_eq!(resync.event, "reset", "{resync:?}");
    server.stop().await;
}

/// The handoff a compaction probe's compaction answers with.
fn compaction_probe_handoff() -> String {
    serde_json::json!({
        "current_task": COMPACTION_PROBE_TASK,
        "key_decisions": [],
        "context_files": [],
        "errors_fixes": [],
        "pending_work": [],
        "recovery_hints": [],
        "skills": [],
    })
    .to_string()
}

/// Characters of filler that put a compaction probe's first request at about
/// 33k tokens: past the start point of [`compaction_probe_context`] and, with
/// a start distance as wide as the pause point's, past that too.
const COMPACTION_PROBE_FILLER_REPEATS: usize = 10_000;

/// A `[context]` section whose auto-compact point is 40k tokens, whatever the
/// mock model's 128k window: compaction starts at `40k - start_distance` and
/// the run pauses for it at `40k - safe_buffer`.
fn compaction_probe_context(start_distance: usize, safe_buffer: usize) -> ContextConfig {
    ContextConfig {
        // The pause point is the ceiling; compaction starts `start_distance`
        // before the 40k point, which is `start_distance - safe_buffer`
        // before the pause point.
        compact_ceiling: 40_000 - safe_buffer,
        start_distance: start_distance.saturating_sub(safe_buffer),
        ..ContextConfig::default()
    }
}

/// A user turn that fills the context of a compaction probe.
fn compaction_probe_user_turn() -> String {
    format!(
        "Take the steps. {}",
        "lorem ipsum ".repeat(COMPACTION_PROBE_FILLER_REPEATS)
    )
}

/// A compaction that starts while the run streams lands without stopping it,
/// is persisted with the context snapshot, and is what a restarted daemon
/// resumes from.
#[tokio::test]
#[file_serial]
async fn a_compaction_started_mid_run_lands_and_survives_a_restart() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::CompactionProbe(CompactionProbe::Lands)).await;
    let harness =
        TestHarness::new_with_context(&mock.base_url(), compaction_probe_context(12_000, 2_000))
            .await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": compaction_probe_user_turn(),
        }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the compacting turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );

    let requests = mock.agent_requests().await;
    assert!(
        requests.iter().any(is_compaction_request),
        "crossing the start point forks a compaction: {} requests; server logs:\n{}",
        requests.len(),
        server.log.contents().await
    );
    assert!(
        mock.state.lock().await.served_during_compaction >= 1,
        "the run keeps asking the model while the compaction is in flight"
    );
    let before_compaction = requests
        .iter()
        .find(|request| !is_compaction_request(request))
        .expect("the turn's first request")
        .to_string()
        .len();

    // Crash the daemon: the next one has nothing but the persisted snapshot.
    server.kill().await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": COMPACTION_PROBE_FOLLOW_UP,
        }),
    )
    .await
    .expect("follow-up turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn after the restart", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the follow-up should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );

    let requests = mock.agent_requests().await;
    let resumed = requests
        .last()
        .expect("the follow-up's request")
        .to_string();
    assert!(
        resumed.contains(COMPACTION_PROBE_TASK),
        "the restarted session resumes from the compacted context"
    );
    assert!(
        resumed.len() < before_compaction / 2,
        "the compaction replaced the conversation: {} bytes after the restart, {before_compaction} before the compaction",
        resumed.len()
    );
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A compaction that fails at the pause point is cleaned up and rerun, and
/// the run shows both in its agent frames.
#[tokio::test]
#[file_serial]
async fn a_failed_compaction_shows_its_cleanup_and_rerun() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::CompactionProbe(
        CompactionProbe::FailsThenLands,
    ))
    .await;
    // The pause point equals the start point, so the first request boundary
    // runs the compaction instead of forking it.
    let harness =
        TestHarness::new_with_context(&mock.base_url(), compaction_probe_context(12_000, 12_000))
            .await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let mut frames = agent_frames(&client, &server, &session_id, None).await;
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": compaction_probe_user_turn(),
        }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;

    let mut agent_events = Vec::new();
    loop {
        let frame = next_turn_frame(&mut frames).await;
        let complete = frame.data.contains("\"kind\":\"end\"");
        agent_events.push(frame.data);
        if complete {
            break;
        }
    }
    let Some(recovery) = agent_events
        .iter()
        .find(|data| data.contains("\"CompactionRecovery\""))
    else {
        panic!(
            "the failed compaction's cleanup should reach the transcript: {agent_events:?}; server logs:\n{}",
            server.log.contents().await
        );
    };
    let frame: Value = serde_json::from_str(recovery).expect("agent frame JSON");
    let event = &frame["body"]["event"]["event"]["CompactionRecovery"];
    assert!(
        event["alerts_removed"].is_u64() && event["outputs_spilled"].is_u64(),
        "the recovery frame counts what the cleanup did: {frame}"
    );
    let events = collect_turn_events(turn, "the recovering turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the rerun compaction lets the turn finish; events: {events:?}"
    );
    let compactions = mock
        .agent_requests()
        .await
        .iter()
        .filter(|request| is_compaction_request(request))
        .count();
    assert_eq!(
        compactions, 3,
        "the compaction and its retry fail, the rerun lands"
    );
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// An explicit compaction on an idle session reaches every client of the
/// session as a `session_compaction` start and an applied finish.
#[tokio::test]
#[file_serial]
async fn an_explicit_compact_reports_its_start_and_finish_on_the_session_events() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::CompactionProbe(
        CompactionProbe::LandsAtOnce,
    ))
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({
            "sessionId": session_id,
            "content": COMPACTION_PROBE_FOLLOW_UP,
        }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn to compact", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should complete; events: {events:?}; server logs:\n{}",
        server.log.contents().await
    );

    let mut frames = agent_frames(&client, &server, &session_id, None).await;
    let compacted = client
        .post(format!(
            "{}/api/sessions/{session_id}/actions/compact",
            server.base_url
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("the compact request should be answered");
    let compacted = expect_status(&server.log, compacted, StatusCode::OK).await;
    let compacted: Value = compacted.json().await.expect("compact answer JSON");
    assert_eq!(compacted["compacted"], true, "{compacted}");

    let mut phases = Vec::new();
    while phases.len() < 2 {
        let Ok(next) = tokio::time::timeout(Duration::from_secs(20), frames.next_event()).await
        else {
            panic!(
                "compaction frames so far {phases:?}; server logs:\n{}",
                server.log.contents().await
            );
        };
        let event = next.expect("the stream should stay open");
        if event.event == "session_compaction" {
            let frame: Value = serde_json::from_str(&event.data).expect("compaction frame JSON");
            assert_eq!(frame["sessionId"], session_id);
            phases.push(frame["phase"].as_str().expect("a phase").to_string());
        }
    }
    assert_eq!(phases, ["started", "applied"]);
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A command run through `may exec` draws its jobs from the daemon's
/// machine-wide pool: it can take every token of the pool, and no more.
#[tokio::test]
#[file_serial]
async fn exec_commands_draw_from_the_daemons_job_pool() {
    const POOL: i64 = 3;
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    prepare_oneshot_home(&harness).await;
    let session = harness.create_session().await;
    set_job_pool_size(&harness, POOL).await;

    let held = tokens_held_by_exec_probe(&harness, &session.id, &session.sandbox_dir).await;
    let stop = Command::new(may_binary_path())
        .args(["daemon", "stop"])
        .env("HOME", harness.home_path())
        .output()
        .await
        .expect("may daemon stop should run");
    assert!(stop.status.success(), "daemon stop failed");
    assert_eq!(
        held,
        POOL.to_string(),
        "the command drew exactly the daemon pool's tokens"
    );
}

/// A container session's commands draw from the same machine-wide pool as
/// local ones: while a container command holds every token, a local command
/// gets none.
#[tokio::test]
#[file_serial]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "container runtimes are expected only on Linux"
)]
async fn container_and_local_commands_share_the_daemons_job_pool() {
    const POOL: i64 = 3;
    let mock = MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(
        "python3 /workspace/tmp/job_probe.py /workspace/tmp/held",
    ))
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    set_job_pool_size(&harness, POOL).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Container
        .prepare(&harness, &client, &server)
        .await;
    let container_session =
        create_session_via_api(&client, &server, environment.profile_id()).await;
    let container_sandbox = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&container_session, Some(environment.profile_id()));
    tokio::fs::create_dir_all(container_sandbox.join("tmp"))
        .await
        .expect("container session tmp should be created");
    tokio::fs::write(
        container_sandbox.join("tmp/job_probe.py"),
        include_str!("fixtures/job_probe.py"),
    )
    .await
    .expect("probe should be written");

    // The turn ends with the model's reply; the probe keeps its tokens.
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": container_session, "content": "start the job" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    collect_turn_events(turn, "the container probe turn", &server.log).await;
    let report = container_sandbox.join("tmp/held");
    let deadline = Instant::now() + Duration::from_secs(30) + environment.first_turn_allowance();
    while !report.exists() {
        assert!(
            Instant::now() < deadline,
            "the container probe never reported; server logs:\n{}",
            server.log.contents().await
        );
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        tokio::fs::read_to_string(&report)
            .await
            .expect("container probe report should be readable")
            .trim(),
        POOL.to_string(),
        "the container command drew every token of the daemon pool"
    );

    let local = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let local_session = create_session_via_api(&client, &server, local.profile_id()).await;
    let local_sandbox = SessionStore::new(MayHomePaths::new(harness.may_dir.clone()))
        .sandbox_dir(&local_session, Some(local.profile_id()));
    let held = tokens_held_by_exec_probe(&harness, &local_session, &local_sandbox).await;
    assert_eq!(
        held, "0",
        "a local command got tokens the container command holds"
    );

    server.stop().await;
    environment.release(&harness, &container_session).await;
}

/// Sets `[sandbox] job_pool_size` in the harness's config before a daemon
/// starts.
async fn set_job_pool_size(harness: &TestHarness, size: i64) {
    let config_path = harness.home_path().join(".may/config.toml");
    let mut config: toml::Table = toml::from_str(
        &tokio::fs::read_to_string(&config_path)
            .await
            .expect("test config should be readable"),
    )
    .expect("test config should parse");
    let sandbox = config
        .entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("sandbox is a table");
    sandbox.insert("job_pool_size".into(), toml::Value::Integer(size));
    tokio::fs::write(
        &config_path,
        toml::to_string(&config).expect("test config should serialize"),
    )
    .await
    .expect("test config should be written");
}

/// Runs the job probe through `may exec` in the local session `session_id`
/// and returns how many tokens it could take from the daemon's pool.
async fn tokens_held_by_exec_probe(
    harness: &TestHarness,
    session_id: &str,
    sandbox_dir: &Path,
) -> String {
    // The sandbox reads only the session's own files.
    tokio::fs::create_dir_all(sandbox_dir.join("tmp"))
        .await
        .expect("session tmp should be created");
    let probe = sandbox_dir.join("tmp/job_probe.py");
    tokio::fs::write(&probe, include_str!("fixtures/job_probe.py"))
        .await
        .expect("probe should be written");
    let report = sandbox_dir.join("tmp/held");
    let output = Command::new(may_binary_path())
        .arg("exec")
        .arg("--session")
        .arg(session_id)
        .arg("--mode")
        .arg("default")
        .arg("--timeout")
        .arg("20")
        .arg("--")
        .arg(format!(
            "python3 {} {} & probe=$!; while [ ! -e {} ]; do sleep 0.1; done; cat {}; kill $probe",
            probe.display(),
            report.display(),
            report.display(),
            report.display(),
        ))
        .env("HOME", harness.home_path())
        .output()
        .await
        .expect("may exec should run");
    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Leases words of `session_id`'s pool in the daemon, on one connection,
/// until every free one is taken; returns the pool and the leases.
async fn lease_every_free_word(
    socket: &Path,
    session_id: &str,
) -> (may_agent::model_ids::SessionIds, Vec<aither_core::IdLease>) {
    let ids = may_agent::client::ids::DaemonIds::connect(socket)
        .await
        .expect("the daemon id connection")
        .session(session_id);
    tokio::task::spawn_blocking(move || {
        let mut leases = Vec::new();
        while let Ok(lease) = ids.lease() {
            leases.push(lease);
        }
        (ids, leases)
    })
    .await
    .expect("leasing finishes")
}

/// Returns `ids` once the daemon served every request sent on its connection
/// before: a claim of a word no pool has is refused only after them, since
/// one connection's requests are served in order.
async fn served(ids: may_agent::model_ids::SessionIds) -> may_agent::model_ids::SessionIds {
    tokio::task::spawn_blocking(move || {
        assert!(
            ids.allocator().claim("qqqqqq").is_err(),
            "no pool has the word"
        );
        ids
    })
    .await
    .expect("the claim finishes")
}

/// The word `may exec` named its job by, from the notice of a job killed at
/// its timeout.
fn killed_job_word(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("background task: ")
                .and_then(|rest| rest.strip_suffix(" (killed)"))
        })
        .unwrap_or_else(|| panic!("exec reports its killed job: {stderr}"))
        .to_string()
}

/// `may exec` leases its ids from the daemon's pool of the session: a word
/// the daemon holds is never handed to exec, and a word exec holds is never
/// handed out by the daemon.
#[tokio::test]
#[file_serial]
async fn exec_and_the_daemon_never_lease_one_word_of_a_session() {
    let mock = MockOpenAiServer::spawn(MockChatScenario::SlowText {
        chunks: vec!["unused".to_string()],
        delay: Duration::from_millis(10),
    })
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    prepare_oneshot_home(&harness).await;
    // A session per direction, so neither sees the other's leases.
    let session = harness.create_session().await;
    let other = harness.create_session().await;
    let socket = harness.may_dir.join("daemon.sock");
    let binary = may_binary_path();
    let exec = |session_id: &str, script: String, timeout: &str| {
        Command::new(&binary)
            .arg("exec")
            .arg("--session")
            .arg(session_id)
            .arg("--mode")
            .arg("default")
            .arg("--timeout")
            .arg(timeout)
            .arg("--")
            .arg(script)
            .env("HOME", harness.home_path())
            .envs(hermetic_git_env(harness.home_path()))
            .output()
    };

    // While exec's job holds its word, the daemon hands out every other word.
    let started = session.sandbox_dir.join("tmp/started");
    let running = tokio::spawn(exec(
        &session.id,
        format!("touch {}; while :; do sleep 0.1; done", started.display()),
        "60",
    ));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(Instant::now() < deadline, "exec's job never started");
        sleep(Duration::from_millis(50)).await;
    }
    let (ids, leases) = lease_every_free_word(&socket, &session.id).await;
    let daemon_words: std::collections::BTreeSet<String> =
        leases.iter().map(ToString::to_string).collect();
    // The words go back before exec's job ends, which stores its output
    // under words of its own.
    drop(leases);
    drop(served(ids).await);
    let output = running
        .await
        .expect("exec task")
        .expect("may exec should run");
    let exec_word = killed_job_word(&output);
    assert!(
        !daemon_words.contains(&exec_word),
        "the daemon handed out exec's word {exec_word}"
    );
    assert!(
        !daemon_words.is_empty(),
        "the daemon leased the other words"
    );

    // While the daemon holds every word of the other session but a few,
    // exec's job there gets one of those. A few, since the killed job's
    // output is stored under words too.
    let (ids, mut held) = lease_every_free_word(&socket, &other.id).await;
    let free: std::collections::BTreeSet<String> =
        held.drain(..4).map(|lease| lease.to_string()).collect();
    let ids = served(ids).await;
    let output = exec(&other.id, "while :; do sleep 0.1; done".to_string(), "1")
        .await
        .expect("may exec should run");
    let exec_word = killed_job_word(&output);
    assert!(
        free.contains(&exec_word),
        "exec took {exec_word}, not one of the words the daemon did not hold: {free:?}"
    );
    drop((ids, held));

    let stop = Command::new(&binary)
        .args(["daemon", "stop"])
        .env("HOME", harness.home_path())
        .output()
        .await
        .expect("may daemon stop should run");
    assert!(stop.status.success(), "daemon stop failed");
}

/// `explore` runs the bundled explore subagent on the fast model and returns
/// its result; the subagent's writes to the workspace, `tasks.md` and issues
/// are refused.
#[tokio::test]
#[file_serial]
async fn explore_runs_read_only_on_the_fast_model() {
    let (files, mock) = run_subagent_probe(SubagentProbe {
        setup: "printf -- '## Notes\\noriginal\\n' > tasks.md",
        launch: "explore --prompt-file prompt.md",
        child_script: "echo x > ../../workspace.txt; echo WORKSPACE_RC=$?; printf -- 'hacked\\n' >> ../../tasks.md; echo TASKS_RC=$?; issue create bug 'from explore' --body b; echo ISSUE_RC=$?; issue list >/dev/null; echo ISSUE_LIST_RC=$?",
        final_script: "cp tasks.md tasks.before; echo MAIN_FINAL",
    })
    .await;

    let requests = mock.agent_requests().await;
    assert_eq!(
        child_request_models(&requests),
        [TIER_FAST_MODEL],
        "explore runs on the fast model"
    );
    let result = files.get("sub.out");
    assert!(
        result.contains("Subagent 'explore' completed (fast tier"),
        "the explore result comes back: {result}"
    );
    let outputs = child_probe_outputs(&mock).await;
    assert_eq!(outputs.len(), 1, "{outputs:?}");
    let code = |key: &str| -> i32 {
        let start = outputs[0]
            .find(key)
            .unwrap_or_else(|| panic!("no {key} in {}", outputs[0]))
            + key.len();
        outputs[0][start..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("no code after {key} in {}", outputs[0]))
    };
    assert_ne!(code("WORKSPACE_RC="), 0, "{outputs:?}");
    assert_ne!(code("TASKS_RC="), 0, "{outputs:?}");
    assert_ne!(code("ISSUE_RC="), 0, "{outputs:?}");
    assert_eq!(code("ISSUE_LIST_RC="), 0, "reads stay open: {outputs:?}");
    assert!(!files.0.contains_key("workspace.txt"));
    assert_eq!(files.get("tasks.before"), "## Notes\noriginal\n");
}

/// What the alert that the daemon exited unexpectedly opens with.
const DAEMON_EXIT_ALERT: &str = "The May daemon exited unexpectedly";
/// How long the daemon's background watcher takes, at most, to record the
/// sessions with background work.
const BACKGROUND_RECORD_DELAY: Duration = Duration::from_secs(3);

fn mentions_daemon_exit(request: &Value) -> bool {
    message_texts(request)
        .iter()
        .any(|text| text.contains(DAEMON_EXIT_ALERT))
}

/// Whether process `pid` or any process of the group it leads is alive.
fn process_tree_alive(pid: u32) -> bool {
    let pid = rustix::process::Pid::from_raw(pid.cast_signed()).expect("a nonzero pid");
    rustix::process::test_kill_process(pid).is_ok()
        || rustix::process::test_kill_process_group(pid).is_ok()
}

/// Waits until the mock received more than `seen` agent requests and
/// returns the newest.
async fn next_agent_request(mock: &MockOpenAiServer, seen: usize, log: &ServerLog) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let requests = mock.agent_requests().await;
        if requests.len() > seen {
            return requests.last().expect("a request").clone();
        }
        assert!(
            Instant::now() < deadline,
            "no agent request after the first {seen}; server logs:\n{}",
            strip_ansi_escapes(&log.contents().await)
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// Starts the session's background job with one turn and returns the job's
/// pid once the daemon has had time to record the session's background work.
async fn start_background_job(
    client: &reqwest::Client,
    server: &RunningDaemon,
    session_id: &str,
) -> u32 {
    let turn = send_turn(
        client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "start the job" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the background-job turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should end while its job runs; events: {events:?}"
    );
    let background = background_of(client, server, session_id).await;
    let job = background_job(&background, BACKGROUND_JOB_SCRIPT)
        .filter(|job| job.pointer("/status/state").and_then(Value::as_str) == Some("running"))
        .unwrap_or_else(|| panic!("the job outlives its turn: {background}"));
    let pid = job
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("the job has a pid");
    sleep(BACKGROUND_RECORD_DELAY).await;
    pid
}

/// A daemon killed outright while a session's background command runs:
/// the next daemon start ends the command, and since the session's prompt
/// cache is warm its agent starts again at once with the alert.
#[tokio::test]
#[file_serial]
async fn an_unexpected_exit_kills_background_commands_and_restarts_a_warm_session() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(BACKGROUND_JOB_SCRIPT)).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    let pid = start_background_job(&client, &server, &session_id).await;
    let seen = mock.agent_requests().await.len();

    server.kill().await;
    #[cfg(target_os = "macos")]
    assert!(
        process_tree_alive(pid),
        "the job outlives the daemon that started it until the next start ends it"
    );

    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_tree_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the next daemon start should end the exited daemon's job {pid}; server logs:\n{}",
            strip_ansi_escapes(&server.log.contents().await)
        );
        sleep(Duration::from_millis(100)).await;
    }
    let restarted = next_agent_request(&mock, seen, &server.log).await;
    assert!(
        mentions_daemon_exit(&restarted),
        "the restarted turn carries the alert: {:?}",
        message_texts(&restarted)
    );
    wait_for_runtime_state_clear(&client, &server, &session_id, Duration::from_secs(30)).await;
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A daemon killed outright during a turn of a session whose prompt cache
/// has since gone cold: nothing runs at the next start, and the user's next
/// message carries the alert.
#[tokio::test]
#[file_serial]
async fn an_unexpected_exit_leaves_a_cold_session_for_the_next_message() {
    // Agent request 1, the second turn's, is held so the daemon dies in it.
    let gate = RequestGate::holding(1);
    let mock = MockOpenAiServer::spawn_gated(
        MockChatScenario::CachedTextEveryTurn { cached: vec![500] },
        Some(gate.clone()),
    )
    .await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let config_path = harness.may_dir.join("config.toml");
    let mut config = MayConfig::load_from(&config_path)
        .await
        .expect("test config should load");
    config
        .set_provider_cache_lifetime(TEST_PROVIDER_ID, "2s")
        .expect("a valid cache lifetime");
    config
        .save_to(&config_path)
        .await
        .expect("test config should be written");
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "first question" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the first turn", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "events: {events:?}"
    );
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "second question" }),
    )
    .await
    .expect("turn request should succeed");
    expect_status(&server.log, turn, StatusCode::OK).await;
    tokio::time::timeout(Duration::from_secs(60), gate.arrived.notified())
        .await
        .unwrap_or_else(|_| panic!("the second turn never asked the model"));
    server.kill().await;
    let seen = mock.agent_requests().await.len();

    // Past the cache lifetime the session is cold.
    sleep(Duration::from_secs(3)).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    sleep(Duration::from_secs(4)).await;
    assert_eq!(
        mock.agent_requests().await.len(),
        seen,
        "a cold session starts nothing until the user writes; server logs:\n{}",
        strip_ansi_escapes(&server.log.contents().await)
    );
    let runtime = get_runtime(&client, &server, &session_id).await;
    assert!(runtime_is_idle(&runtime), "{runtime}");

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "third question" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn after the restart", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "events: {events:?}"
    );
    let requests = mock.agent_requests().await;
    let next = &requests[seen];
    let texts = message_texts(next);
    assert!(
        mentions_daemon_exit(next),
        "the alert rides along: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("third question")),
        "with the user's message: {texts:?}"
    );
    // The daemon died before the second turn's first checkpoint: the rebuilt
    // agent still has the message that turn was answering, ahead of the new one.
    let interrupted = texts
        .iter()
        .position(|text| text.contains("second question"))
        .unwrap_or_else(|| panic!("the interrupted turn's message is in context: {texts:?}"));
    let latest = texts
        .iter()
        .position(|text| text.contains("third question"))
        .expect("the new message");
    assert!(interrupted < latest, "in order: {texts:?}");
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A daemon killed the moment a turn reported `done` was not running that
/// session: the next start neither restarts it nor owes it the alert. The
/// finished turn is one a background job's completion started: from the
/// job's start until that turn ends the session has background work, so the
/// daemon records it as busy throughout, and `done` must follow that record
/// being brought up to date. The next daemon settles the sessions an exit
/// interrupted before it serves anyone, so the follow-up message is admitted
/// only when nothing restarted, and it carries no alert.
#[tokio::test]
#[file_serial]
async fn a_daemon_killed_right_after_done_does_not_restart_the_finished_session() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(FINISHING_JOB_SCRIPT)).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "start the job" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn starting the job", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "the turn should end while its job runs; events: {events:?}"
    );
    let stream = client
        .get(format!(
            "{}/api/sessions/{session_id}/events",
            server.base_url
        ))
        .send()
        .await
        .expect("event stream should open");
    let resumed = collect_turn_events(stream, "the resumed turn", &server.log).await;
    assert!(
        resumed.iter().any(|(kind, _)| kind == "done"),
        "the job's completion should resume the session; events: {resumed:?}; server logs:\n{}",
        server.log.contents().await
    );
    server.kill().await;
    let seen = mock.agent_requests().await.len();

    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "anything new?" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn after the kill", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "events: {events:?}"
    );
    let requests = mock.agent_requests().await;
    assert_eq!(
        requests.len(),
        seen + 1,
        "only the user's message ran after the restart; server logs:\n{}",
        strip_ansi_escapes(&server.log.contents().await)
    );
    assert!(
        !mentions_daemon_exit(&requests[seen]),
        "a finished session is owed no alert: {:?}",
        message_texts(&requests[seen])
    );
    server.stop().await;
    environment.release(&harness, &session_id).await;
}

/// A daemon stopped through its shutdown path owes no session the alert:
/// nothing starts at the next start, and the next message carries none.
#[tokio::test]
#[file_serial]
async fn a_graceful_stop_does_not_alert() {
    let mock =
        MockOpenAiServer::spawn(MockChatScenario::StartsBackgroundJob(BACKGROUND_JOB_SCRIPT)).await;
    let harness = TestHarness::new(&mock.base_url()).await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    let client = reqwest::Client::new();
    let environment = ChatEnvironment::Local
        .prepare(&harness, &client, &server)
        .await;
    let session_id = create_session_via_api(&client, &server, environment.profile_id()).await;
    start_background_job(&client, &server, &session_id).await;
    let seen = mock.agent_requests().await.len();

    server.stop().await;
    let server = RunningDaemon::spawn(harness.home_path(), &[]).await;
    sleep(Duration::from_secs(3)).await;
    assert_eq!(
        mock.agent_requests().await.len(),
        seen,
        "nothing restarts after a graceful stop"
    );

    let turn = send_turn(
        &client,
        &server.base_url,
        &serde_json::json!({ "sessionId": session_id, "content": "anything new?" }),
    )
    .await
    .expect("turn request should succeed");
    let turn = expect_status(&server.log, turn, StatusCode::OK).await;
    let events = collect_turn_events(turn, "the turn after the stop", &server.log).await;
    assert!(
        events.iter().any(|(kind, _)| kind == "done"),
        "events: {events:?}"
    );
    let next = &mock.agent_requests().await[seen];
    assert!(
        !mentions_daemon_exit(next),
        "no alert after a graceful stop: {:?}",
        message_texts(next)
    );
    server.stop().await;
    environment.release(&harness, &session_id).await;
}
