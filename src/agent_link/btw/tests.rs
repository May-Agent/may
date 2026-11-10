//! `btw` against real session agents on a stub model server: the fork
//! answers from the recipient's context, leaves the recipient untouched,
//! refuses a cold recipient before any model call, and cannot write.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::idle_compaction_tests::{
    SESSION_MODEL, Session, StubModel, StubReply, run_short_turn, saved_config,
};
use crate::agent_link::{
    AgentDirectory, AgentLinkError, AgentMessaging, CacheLifetimes, ForkHosts,
};
use crate::cache_lifetime::CacheLifetime;
use crate::db::{ChatDb, PromptCacheRequest};
use crate::repo_clone::test_sandbox::TestTerminal;
use crate::session::SessionRuntimeService;
use crate::ui_event_bus::UiEventBus;

const ASKER: &str = "asker";
const RECIPIENT: &str = "recipient";
/// What only the recipient's context holds.
const FACT: &str = "FACT-SENTINEL the release branch is rel/2026-11";
const QUESTION: &str = "BTW-SENTINEL which branch carries the release?";
const ANSWER: &str = "ANSWER-SENTINEL rel/2026-11";
const WRITE_QUESTION: &str = "BTW-WRITE-SENTINEL record the branch in a file";
/// Opening words of the refusal every command of a btw fork gets.
const REFUSAL: &str = "A btw fork answers from its context alone";

/// The daemon's view of the asking and the answering agent.
struct Host {
    _may_dir: tempfile::TempDir,
    runtime_service: Arc<SessionRuntimeService>,
    messaging: AgentMessaging,
    /// The asking agent's terminal, which launches the fork's.
    asker_root: tempfile::TempDir,
    asker_terminal: TestTerminal,
}

impl Host {
    async fn new() -> Self {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let may_dir = tempfile::tempdir().expect("may dir");
        let db = ChatDb::open(&may_dir.path().join("history.db"))
            .await
            .expect("history.db opens");
        let runtime_service = Arc::new(SessionRuntimeService::new(
            db,
            UiEventBus::new(),
            crate::session::MayHomePaths::new(may_dir.path().join("home")),
        ));
        let messaging = AgentMessaging {
            directory: AgentDirectory::new(
                runtime_service.clone(),
                may_dir.path().to_path_buf(),
                |_| {},
            ),
            runtime_service: runtime_service.clone(),
            cache_lifetime: CacheLifetimes::new(|_| CacheLifetime::default()),
            forks: ForkHosts::default(),
        };
        let asker_root = tempfile::tempdir().expect("asker root");
        let asker_terminal = TestTerminal::new(asker_root.path()).await;
        Self {
            _may_dir: may_dir,
            runtime_service,
            messaging,
            asker_root,
            asker_terminal,
        }
    }

    async fn session(&self, session_id: &str, title: &str) {
        self.runtime_service
            .db()
            .create_session(session_id, Some(title), None)
            .await
            .expect("session is created");
    }

    /// A recipient session agent on `stub` whose context holds [`FACT`] and
    /// one finished turn.
    async fn recipient(&self, stub: &StubModel) -> Session {
        self.session(ASKER, ASKER).await;
        let mut session = Box::pin(Session::start_with_messaging(
            &saved_config(stub, None),
            self.messaging.clone(),
        ))
        .await;
        self.session(session.parts.session_id.as_str(), RECIPIENT)
            .await;
        session
            .agent
            .push_message(aither_core::llm::Message::assistant(FACT));
        run_short_turn(&mut session.agent).await;
        session
    }

    async fn ask(&self, question: &str) -> Result<super::BtwAnswer, AgentLinkError> {
        self.messaging
            .btw(
                ASKER,
                RECIPIENT,
                question,
                self.asker_terminal.binding().launcher(),
            )
            .await
    }

    /// Where the asking terminal launches its subagents' spaces.
    fn spaces(&self) -> PathBuf {
        self.asker_root.path().join("subagent-spaces")
    }
}

/// Bodies of the stub's requests that contain `needle`.
async fn requests_with(stub: &StubModel, needle: &str) -> Vec<String> {
    stub.requests
        .lock()
        .await
        .iter()
        .filter(|body| body.contains(needle))
        .cloned()
        .collect()
}

fn cache_key(body: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(body).expect("a JSON request")["prompt_cache_key"]
        .clone()
}

/// The content of the last message of a chat-completions request.
fn last_message(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).expect("a JSON request");
    request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .map(|message| message["content"].to_string())
        .expect("a request has messages")
}

fn empty_or_absent(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none())
}

/// The answer comes from the recipient's context, under its prompt-cache
/// key, while the recipient runs a turn of its own; neither that turn nor
/// the next one carries the question or the answer.
#[tokio::test]
async fn the_fork_answers_from_the_recipients_context_and_leaves_it_unchanged() {
    let stub = StubModel::spawn_scripted(SESSION_MODEL, |body| {
        body.contains(QUESTION)
            .then(|| StubReply::Text(ANSWER.to_string()))
    })
    .await;
    let host = Host::new().await;
    let mut session = host.recipient(&stub).await;

    let (answer, ()) = tokio::join!(host.ask(QUESTION), async {
        let _ = session
            .agent
            .run("Keep working.", [])
            .drive((), std::future::pending::<()>(), |(), _| async {
                std::ops::ControlFlow::Continue(())
            })
            .await
            .expect("the recipient's turn runs");
    });
    let answer = answer.expect("the fork answers");
    assert_eq!(answer.recipient, RECIPIENT);
    assert_eq!(answer.answer, ANSWER);

    let asked = requests_with(&stub, QUESTION).await;
    assert_eq!(asked.len(), 1, "one fork request");
    let fork_request = &asked[0];
    assert!(
        fork_request.contains(FACT) && fork_request.contains("Say hello."),
        "the fork starts from the recipient's context: {fork_request}"
    );
    let own = requests_with(&stub, "Say hello.").await;
    assert_eq!(
        cache_key(fork_request),
        cache_key(&own[0]),
        "the fork reads the recipient's prompt cache"
    );

    let _ = session
        .agent
        .run("Next step.", [])
        .drive((), std::future::pending::<()>(), |(), _| async {
            std::ops::ControlFlow::Continue(())
        })
        .await
        .expect("the recipient's next turn runs");
    // The recipient's own requests end with its turn's prompt; the fork's
    // request, which may have forked the concurrent turn, ends with the
    // question.
    for turn in ["Keep working.", "Next step."] {
        let own = requests_with(&stub, turn)
            .await
            .into_iter()
            .filter(|body| last_message(body).contains(turn))
            .collect::<Vec<_>>();
        assert!(
            !own.is_empty(),
            "the recipient's turn {turn:?} reached the model"
        );
        for body in own {
            assert!(
                !body.contains(QUESTION) && !body.contains(ANSWER) && !body.contains("agent-btw"),
                "the recipient's request carries the btw: {body}"
            );
        }
    }
    assert!(
        empty_or_absent(&host.spaces()),
        "the fork's space is removed"
    );
}

/// An idle recipient past its cache lifetime is refused, and the model is
/// never called.
#[tokio::test]
async fn a_cold_recipient_is_refused_without_a_model_call() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let host = Host::new().await;
    let session = host.recipient(&stub).await;
    let usage = aither_core::llm::Usage::new(100, 10).with_cache_tokens(0, 100);
    host.runtime_service
        .db()
        .observe_prompt_cache(PromptCacheRequest {
            session_id: session.parts.session_id.as_str(),
            provider_id: "session",
            model: SESSION_MODEL,
            at: jiff::Timestamp::now() - jiff::SignedDuration::from_hours(2),
            usage: &usage,
            prefix: aither_agent::context::PromptPrefix {
                chain: aither_agent::context::PrefixChain::of(&[], &[]),
                rewrites: aither_agent::context::PrefixRewrites::default(),
                first_request: true,
            },
            lifetime: CacheLifetime::default(),
            published_block: None,
        })
        .await
        .expect("request is recorded");
    let before = stub.requests.lock().await.len();

    let error = host.ask(QUESTION).await.expect_err("a cold recipient");
    assert!(
        matches!(error, AgentLinkError::ColdBtwRecipient(_)),
        "{error}"
    );
    assert!(error.to_string().contains("cache has gone cold"), "{error}");
    assert_eq!(stub.requests.lock().await.len(), before, "no model call");
    assert!(empty_or_absent(&host.spaces()), "no fork was launched");
}

/// A command the fork runs is refused before it starts, even one writing
/// where the asking terminal may write; the fork then answers.
#[tokio::test]
async fn the_fork_cannot_write() {
    let leak = tempfile::tempdir().expect("leak dir");
    let target = leak.path().join("leaked.txt");
    let script = format!("echo leaked > '{}'", target.display());
    let stub = StubModel::spawn_scripted(SESSION_MODEL, move |body| {
        if !body.contains(WRITE_QUESTION) {
            return None;
        }
        Some(if body.contains(REFUSAL) {
            StubReply::Text(ANSWER.to_string())
        } else {
            StubReply::ToolCall {
                name: "terminal",
                arguments: serde_json::json!({
                    "description": "record the branch",
                    "script": script,
                    "mode": "default",
                    "timeout": 30,
                }),
            }
        })
    })
    .await;
    let host = Host::new().await;
    let _session = host.recipient(&stub).await;

    let answer = host.ask(WRITE_QUESTION).await.expect("the fork answers");
    assert_eq!(answer.answer, ANSWER);
    assert!(!target.exists(), "the fork wrote {}", target.display());
    assert!(
        !requests_with(&stub, REFUSAL).await.is_empty(),
        "the refusal reached the fork's model"
    );
    assert!(
        empty_or_absent(&host.spaces()),
        "the fork's space is removed"
    );
}

/// A session with no agent that sent a request has nothing to fork.
#[tokio::test]
async fn a_recipient_without_a_published_request_is_refused() {
    let host = Host::new().await;
    host.session(ASKER, ASKER).await;
    host.session("idle", RECIPIENT).await;
    let error = host.ask(QUESTION).await.expect_err("nothing to fork");
    assert!(matches!(error, AgentLinkError::NoForkContext(_)), "{error}");
}

/// What only the running turn adds to the recipient's context.
const TURN_MARKER: &str = "TURN-MARKER-SENTINEL";

/// A side request answers from the context of a recipient in the middle of
/// a turn, holding what the turn added so far, and the recipient's turn
/// sends the same requests it would have without it.
///
/// The turn's first request answers with a tool call carrying
/// [`TURN_MARKER`]; its second request, which holds the marker, is held at
/// the model until the side request is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_side_request_reads_the_context_of_a_running_turn() {
    use futures_lite::StreamExt as _;

    let (open_gate, gate) = std::sync::mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let (held_tx, mut held) = tokio::sync::mpsc::unbounded_channel::<()>();
    let stub = StubModel::spawn_scripted(SESSION_MODEL, move |body| {
        if body.contains(QUESTION) {
            return Some(StubReply::Text(ANSWER.to_string()));
        }
        if !body.contains("Keep working.") {
            return None;
        }
        if body.contains(TURN_MARKER) {
            // Held until the side request is answered. The reply script
            // runs on a worker of the stub server, not on the test's.
            held_tx.send(()).expect("the test waits for the request");
            gate.lock()
                .expect("gate")
                .recv()
                .expect("the gate is opened");
            return None;
        }
        Some(StubReply::ToolCall {
            name: "terminal",
            arguments: serde_json::json!({
                "description": TURN_MARKER,
                "script": "echo working",
                "mode": "default",
                "timeout": 30,
            }),
        })
    })
    .await;
    let host = Host::new().await;
    let mut session = host.recipient(&stub).await;
    let session_id = session.parts.session_id.as_str().to_string();

    let side_request = async {
        // The turn's second request is published before it is sent, so its
        // arrival at the model means the context holds the marker.
        tokio::time::timeout(std::time::Duration::from_secs(60), held.recv())
            .await
            .expect("the turn reaches its second request")
            .expect("the stub holds the request");
        let side = host
            .messaging
            .side_context(&session_id)
            .await
            .expect("the side request is allowed")
            .expect("the recipient has a live agent");
        let mut messages = side.messages;
        messages.push(aither_core::llm::Message::user(QUESTION));
        let mut answer = String::new();
        let mut stream = std::pin::pin!(side.model.respond(messages));
        while let Some(event) = stream.next().await {
            if let aither_core::llm::Event::Text(text) = event.expect("the model answers") {
                answer.push_str(&text);
            }
        }
        open_gate.send(()).expect("the turn waits at the gate");
        answer
    };
    let (answer, ()) = tokio::join!(side_request, async {
        let _ = session
            .agent
            .run("Keep working.", [])
            .drive((), std::future::pending::<()>(), |(), _| async {
                std::ops::ControlFlow::Continue(())
            })
            .await
            .expect("the recipient's turn runs to completion");
    });
    assert_eq!(answer, ANSWER);

    let asked = requests_with(&stub, QUESTION).await;
    assert_eq!(asked.len(), 1, "one side request");
    assert!(
        asked[0].contains("Keep working.") && asked[0].contains(TURN_MARKER),
        "the side request holds what the running turn added: {}",
        asked[0]
    );

    let own = requests_with(&stub, "Keep working.")
        .await
        .into_iter()
        .filter(|body| !body.contains(QUESTION))
        .collect::<Vec<_>>();
    assert_eq!(own.len(), 2, "the turn sent its two requests");
    assert!(own[1].contains(TURN_MARKER));
    for body in own {
        assert!(
            !body.contains(ANSWER) && !body.contains("agent-btw"),
            "the recipient's request carries the side request: {body}"
        );
    }
}
