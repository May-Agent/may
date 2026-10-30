//! The ordering and keying of responses to permission, workspace and
//! `ask_user` prompts a run is waiting on, against real SQLite database
//! files: a response commits before the waiter wakes, a failed commit wakes
//! nobody, and a response for an older request leaves a newer one alone.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aither_agent::ask_user::{AnswerValue, AskUserArgs, AskUserRequest, AskUserRequestPayload};
use aither_agent::workspace_request::RequestWorkspaceArgs;
use aither_sandbox::TerminalMode;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::state::{AskUserApprover, AskUserOutcome, WorkspaceApprover};
use crate::agent::PermissionApprover;
use crate::db::ChatDb;
use crate::session::{SessionRuntimeService, SessionRuntimeSummary};
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// Upper bound on any single wait in this module.
const WAIT_BOUND: Duration = Duration::from_secs(20);

fn init_executor() {
    let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
}

/// Bound `future` by [`WAIT_BOUND`], naming `what` it waits for on expiry.
async fn bounded<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT_BOUND, future)
        .await
        .unwrap_or_else(|_| panic!("timed out after {WAIT_BOUND:?} waiting for {what}"))
}

struct Fixture {
    _dir: TempDir,
    runtime: Arc<SessionRuntimeService>,
    events: broadcast::Receiver<UiEventTopics>,
}

impl Fixture {
    async fn new() -> Self {
        init_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open database");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        let bus = UiEventBus::new();
        let events = bus.subscribe();
        Self {
            _dir: dir,
            runtime: Arc::new(SessionRuntimeService::new(db, bus)),
            events,
        }
    }

    /// Wait for a runtime change that makes `done` true, naming `what`.
    async fn until(&mut self, what: &str, done: impl Fn(&SessionRuntimeSummary) -> bool + Sync) {
        bounded(what, async {
            loop {
                let runtime = self.runtime.get("session").await.expect("runtime loads");
                if done(&runtime) {
                    return;
                }
                match self.events.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        panic!("the event bus closed while waiting for {what}")
                    }
                }
            }
        })
        .await;
    }
}

/// What a woken waiter saw: its decision and the state it woke into.
#[derive(Debug)]
struct Woken {
    accepted: bool,
    runtime: SessionRuntimeSummary,
    responses: usize,
}

/// One kind of prompt a run waits on.
trait Prompt: Sized + Send + Sync {
    fn new(runtime: &Arc<SessionRuntimeService>) -> Self;
    /// Start a run waiting on a new prompt; the id of the pending request is
    /// read from the runtime once it shows.
    fn wait(&self, runtime: &Arc<SessionRuntimeService>) -> JoinHandle<Woken>;
    /// The id of the request the runtime shows pending, if any.
    fn pending(runtime: &SessionRuntimeSummary) -> Option<String>;
    /// Respond; `Some(true)` when a waiter was woken.
    async fn respond(&self, id: &str, accept: bool) -> Result<Option<bool>, String>;
    /// SQL that makes recording the response in the history fail.
    const BREAK_HISTORY: &'static str;
    /// SQL that lets it succeed again.
    const MEND_HISTORY: &'static str = "DROP TRIGGER IF EXISTS fail_response_history";
    /// The role of the history rows recording a response.
    const RESPONSE_ROLE: &'static str;
}

async fn observe(runtime: &Arc<SessionRuntimeService>, role: &str, accepted: bool) -> Woken {
    Woken {
        accepted,
        runtime: runtime.get("session").await.expect("runtime loads"),
        responses: runtime
            .db()
            .get_messages_by_role("session", role)
            .await
            .expect("history loads")
            .len(),
    }
}

struct Permission(PermissionApprover);

impl Prompt for Permission {
    const BREAK_HISTORY: &'static str = "CREATE TRIGGER fail_response_history BEFORE INSERT ON message_permission_responses BEGIN SELECT RAISE(ABORT, 'disk full'); END";
    const RESPONSE_ROLE: &'static str = "permission_response";

    fn new(runtime: &Arc<SessionRuntimeService>) -> Self {
        Self(
            PermissionApprover::new()
                .with_runtime_service(runtime.clone())
                .for_session("session"),
        )
    }

    fn wait(&self, runtime: &Arc<SessionRuntimeService>) -> JoinHandle<Woken> {
        let approver = self.0.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let accepted = approver
                .request_and_wait(TerminalMode::Unsafe, "make install".to_string())
                .await
                .expect("the waiter is woken with a decision");
            observe(&runtime, Self::RESPONSE_ROLE, accepted).await
        })
    }

    fn pending(runtime: &SessionRuntimeSummary) -> Option<String> {
        runtime
            .waiting_permission
            .as_ref()
            .map(|request| request.id.clone())
    }

    async fn respond(&self, id: &str, accept: bool) -> Result<Option<bool>, String> {
        self.0
            .resolve(id, accept)
            .await
            .map(|resolved| resolved.map(|resolved| resolved.woke))
            .map_err(|error| error.to_string())
    }
}

struct Workspace(WorkspaceApprover);

impl Prompt for Workspace {
    const BREAK_HISTORY: &'static str = "CREATE TRIGGER fail_response_history BEFORE INSERT ON message_workspace_responses BEGIN SELECT RAISE(ABORT, 'disk full'); END";
    const RESPONSE_ROLE: &'static str = "workspace_response";

    fn new(runtime: &Arc<SessionRuntimeService>) -> Self {
        Self(WorkspaceApprover::new().with_runtime_service(runtime.clone()))
    }

    fn wait(&self, runtime: &Arc<SessionRuntimeService>) -> JoinHandle<Woken> {
        let approver = self.0.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let accepted = approver
                .request_and_wait(
                    "session",
                    RequestWorkspaceArgs {
                        path: "/srv/data".to_string(),
                        reason: "read the dataset".to_string(),
                    },
                )
                .await
                .expect("the waiter is woken with a decision");
            observe(&runtime, Self::RESPONSE_ROLE, accepted).await
        })
    }

    fn pending(runtime: &SessionRuntimeSummary) -> Option<String> {
        runtime
            .waiting_workspace
            .as_ref()
            .map(|request| request.id.clone())
    }

    async fn respond(&self, id: &str, accept: bool) -> Result<Option<bool>, String> {
        self.0
            .resolve(id, accept)
            .await
            .map(|resolved| resolved.map(|resolved| resolved.woke))
            .map_err(|error| error.to_string())
    }
}

struct Question(AskUserApprover);

impl Prompt for Question {
    const BREAK_HISTORY: &'static str = "CREATE TRIGGER fail_response_history BEFORE INSERT ON message_ask_user_responses BEGIN SELECT RAISE(ABORT, 'disk full'); END";
    const RESPONSE_ROLE: &'static str = "ask_user_response";

    fn new(runtime: &Arc<SessionRuntimeService>) -> Self {
        Self(AskUserApprover::new().with_runtime_service(runtime.clone()))
    }

    fn wait(&self, runtime: &Arc<SessionRuntimeService>) -> JoinHandle<Woken> {
        let approver = self.0.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let (tx, rx) = async_channel::bounded(1);
            let request = AskUserRequest::new(
                AskUserRequestPayload {
                    session_id: "session".to_string(),
                    request: AskUserArgs {
                        question: Some("Proceed?".to_string()),
                        option: vec!["yes".to_string(), "no".to_string()],
                        multi_select: false,
                        questions: Vec::new(),
                    },
                },
                tx,
            );
            approver.enqueue(request, Some("session".to_string())).await;
            let answers = rx.recv().await.expect("the waiter is woken with answers");
            let accepted = matches!(
                &answers[0].answer,
                AnswerValue::Selected(values) if values == &["yes".to_string()]
            );
            observe(&runtime, Self::RESPONSE_ROLE, accepted).await
        })
    }

    fn pending(runtime: &SessionRuntimeSummary) -> Option<String> {
        runtime
            .waiting_ask_user
            .as_ref()
            .map(|request| request.id.clone())
    }

    async fn respond(&self, id: &str, accept: bool) -> Result<Option<bool>, String> {
        let answer = if accept { "yes" } else { "no" };
        self.0
            .resolve(
                id,
                vec![aither_agent::ask_user::QuestionAnswer {
                    section: String::new(),
                    answer: AnswerValue::Selected(vec![answer.to_string()]),
                }],
            )
            .await
            .map(|resolved| {
                resolved
                    .filter(|resolved| resolved.outcome != AskUserOutcome::AlreadyAnswered)
                    .map(|resolved| resolved.outcome == AskUserOutcome::Woke)
            })
            .map_err(|error| error.to_string())
    }
}

/// Start a waiter and return it with the id of the request it waits on.
async fn start_waiter<P: Prompt>(fixture: &mut Fixture, prompt: &P) -> (String, JoinHandle<Woken>) {
    let waiter = prompt.wait(&fixture.runtime);
    fixture
        .until("the prompt to show as pending", |runtime| {
            P::pending(runtime).is_some()
        })
        .await;
    let id = P::pending(&fixture.runtime.get("session").await.expect("runtime loads"))
        .expect("pending after the wait");
    (id, waiter)
}

/// The waiter wakes into a state where the request is cleared and the
/// response is already in the history: the commit came first.
async fn the_response_commits_before_the_waiter_wakes<P: Prompt>() {
    let mut fixture = Fixture::new().await;
    let prompt = P::new(&fixture.runtime);
    let (id, waiter) = start_waiter(&mut fixture, &prompt).await;

    let woke = prompt
        .respond(&id, true)
        .await
        .expect("the response commits")
        .expect("the request was pending");
    assert!(woke, "a live waiter was woken");

    let woken = bounded("the waiter to wake", waiter)
        .await
        .expect("the waiter task finishes");
    assert!(woken.accepted);
    assert_eq!(
        P::pending(&woken.runtime),
        None,
        "the request was already cleared when the waiter woke"
    );
    assert_eq!(
        woken.responses, 1,
        "the response was already in the history when the waiter woke"
    );
}

/// A response whose commit fails is returned to the caller and wakes nobody:
/// the waiter stays registered and is woken by the next response, whose
/// decision it receives.
async fn a_failed_commit_does_not_wake_the_waiter<P: Prompt>() {
    let mut fixture = Fixture::new().await;
    let prompt = P::new(&fixture.runtime);
    let (id, waiter) = start_waiter(&mut fixture, &prompt).await;

    fixture
        .runtime
        .db()
        .execute_for_test(P::BREAK_HISTORY)
        .await
        .expect("break the history writes");
    let failed = prompt.respond(&id, false).await;
    assert!(
        failed.is_err(),
        "the persistence failure is returned: {failed:?}"
    );
    assert_eq!(
        P::pending(&fixture.runtime.get("session").await.expect("runtime loads")),
        Some(id.clone()),
        "nothing was committed, so the request is still pending"
    );
    assert!(!waiter.is_finished(), "the failed response woke the waiter");

    fixture
        .runtime
        .db()
        .execute_for_test(P::MEND_HISTORY)
        .await
        .expect("mend the history writes");
    let woke = prompt
        .respond(&id, true)
        .await
        .expect("the response commits once storage works")
        .expect("the request is still pending");
    assert!(woke, "the waiter was still registered");
    let woken = bounded("the waiter to wake", waiter)
        .await
        .expect("the waiter task finishes");
    assert!(
        woken.accepted,
        "the waiter got the response that committed, not the one that failed"
    );
}

/// A response for an older request, arriving after a run asked a newer one,
/// is refused and leaves the newer request pending.
async fn a_response_for_an_older_request_does_not_clear_a_newer_one<P: Prompt>() {
    let mut fixture = Fixture::new().await;
    let prompt = P::new(&fixture.runtime);
    let (older, first) = start_waiter(&mut fixture, &prompt).await;
    prompt
        .respond(&older, true)
        .await
        .expect("the response commits")
        .expect("the older request was pending");
    bounded("the first waiter to wake", first)
        .await
        .expect("the first waiter finishes");

    // The resumed run asks again before the stale response arrives.
    let (newer, second) = start_waiter(&mut fixture, &prompt).await;
    assert_ne!(older, newer);

    let stale = prompt
        .respond(&older, true)
        .await
        .expect("a stale response is not a storage failure");
    assert_eq!(stale, None, "the older request is no longer pending");
    assert_eq!(
        P::pending(&fixture.runtime.get("session").await.expect("runtime loads")),
        Some(newer.clone()),
        "the newer request is still pending"
    );
    assert!(
        !second.is_finished(),
        "the stale response woke the newer waiter"
    );

    prompt
        .respond(&newer, false)
        .await
        .expect("the response commits")
        .expect("the newer request is pending");
    let woken = bounded("the second waiter to wake", second)
        .await
        .expect("the second waiter finishes");
    assert!(!woken.accepted);
}

macro_rules! prompt_tests {
    ($($module:ident => $prompt:ty),+ $(,)?) => {$(
        mod $module {
            use super::*;

            #[tokio::test]
            async fn the_response_commits_before_the_waiter_wakes() {
                super::the_response_commits_before_the_waiter_wakes::<$prompt>().await;
            }

            #[tokio::test]
            async fn a_failed_commit_does_not_wake_the_waiter() {
                super::a_failed_commit_does_not_wake_the_waiter::<$prompt>().await;
            }

            #[tokio::test]
            async fn a_response_for_an_older_request_does_not_clear_a_newer_one() {
                super::a_response_for_an_older_request_does_not_clear_a_newer_one::<$prompt>().await;
            }
        }
    )+};
}

prompt_tests! {
    permission => Permission,
    workspace => Workspace,
    ask_user => Question,
}

fn answer(value: &str) -> Vec<aither_agent::ask_user::QuestionAnswer> {
    vec![aither_agent::ask_user::QuestionAnswer {
        section: String::new(),
        answer: AnswerValue::Selected(vec![value.to_string()]),
    }]
}

/// An answer to a prompt whose run is gone is committed and routed by the
/// kind of prompt: an internal decision resumes the session with the
/// interrupted-run reminder and queues no question of the agent's own. A
/// retry finds the committed state.
#[tokio::test]
async fn a_late_answer_to_a_blocking_prompt_resumes_the_session_and_queues_no_question() {
    let mut fixture = Fixture::new().await;
    let prompt = Question::new(&fixture.runtime);
    let (id, waiter) = start_waiter(&mut fixture, &prompt).await;
    waiter.abort();
    let _ = waiter.await;

    let resolved = prompt
        .0
        .resolve(&id, answer("yes"))
        .await
        .expect("the answer commits")
        .expect("the prompt was pending");
    assert_eq!(
        resolved.outcome,
        AskUserOutcome::Resume { approved: true },
        "a prompt whose waiter is gone resumes the session"
    );
    let runtime = fixture.runtime.get("session").await.expect("runtime loads");
    assert!(
        !runtime.has_pending_messages(),
        "an internal decision is not delivered as a question of the agent"
    );
    assert_eq!(runtime.waiting_ask_user, None, "the prompt was cleared");

    let denied = prompt
        .0
        .resolve(&id, answer("no"))
        .await
        .expect("a retry is not a failure")
        .expect("the retry finds the committed answer");
    assert_eq!(denied.outcome, AskUserOutcome::AlreadyAnswered);
    assert_eq!(denied.session_id, "session");
}

/// A denial in the answer resumes the session as denied.
#[tokio::test]
async fn a_late_denial_resumes_the_session_as_denied() {
    let mut fixture = Fixture::new().await;
    let approver = AskUserApprover::new().with_runtime_service(fixture.runtime.clone());
    let prompt = Question(approver);
    let (id, waiter) = start_waiter(&mut fixture, &prompt).await;
    waiter.abort();
    let _ = waiter.await;

    let resolved = prompt
        .0
        .resolve(&id, answer("Deny"))
        .await
        .expect("the answer commits")
        .expect("the prompt was pending");
    assert_eq!(resolved.outcome, AskUserOutcome::Resume { approved: false });
}

/// Approval of a late answer to a blocking prompt means the allow option was
/// selected, as the live gate decides: a typed reply, even one that reads as
/// consent, is not approval.
#[tokio::test]
async fn only_selecting_the_allow_option_approves_a_late_answer() {
    for typed in ["no", "yes", "allow"] {
        let mut fixture = Fixture::new().await;
        let prompt = Question::new(&fixture.runtime);
        let (id, waiter) = start_waiter(&mut fixture, &prompt).await;
        waiter.abort();
        let _ = waiter.await;
        let resolved = prompt
            .0
            .resolve(
                &id,
                vec![aither_agent::ask_user::QuestionAnswer {
                    section: String::new(),
                    answer: AnswerValue::Custom(typed.to_string()),
                }],
            )
            .await
            .expect("the answer commits")
            .expect("the prompt was pending");
        assert_eq!(
            resolved.outcome,
            AskUserOutcome::Resume { approved: false },
            "a typed {typed:?} is not a selection of the allow option"
        );
    }
}

/// An answer to an open question joins the session queue as the agent's own
/// question answered; a retry delivers nothing a second time and returns the
/// committed state.
#[tokio::test]
async fn an_answer_to_an_open_question_is_queued_once_and_a_retry_finds_it_committed() {
    let fixture = Fixture::new().await;
    let approver = AskUserApprover::new().with_runtime_service(fixture.runtime.clone());
    fixture
        .runtime
        .open_question(
            "session",
            &crate::session::PersistedAskUserRequest {
                id: "open-1".to_string(),
                questions: vec![crate::session::PersistedAskUserQuestion {
                    section: String::new(),
                    question: "Which database?".to_string(),
                    option: vec!["yes".to_string(), "no".to_string()],
                    multi_select: false,
                }],
            },
        )
        .await
        .expect("the question opens");

    let first = approver
        .resolve("open-1", answer("yes"))
        .await
        .expect("the answer commits")
        .expect("the question was open");
    assert_eq!(first.outcome, AskUserOutcome::Queued);
    let retry = approver
        .resolve("open-1", answer("yes"))
        .await
        .expect("a retry is not a failure")
        .expect("the retry finds the committed answer");
    assert_eq!(retry.outcome, AskUserOutcome::AlreadyAnswered);

    let runtime = fixture.runtime.get("session").await.expect("runtime loads");
    let queued = runtime
        .pending_immediate
        .expect("the answer is queued")
        .hidden_context_xml
        .expect("hidden context")
        .to_string();
    assert!(queued.contains("open-1"), "{queued}");
    assert!(runtime.queued_messages.is_empty(), "queued once");
}

/// An answer to a question that was never asked, or withdrawn, is not found.
#[tokio::test]
async fn an_answer_to_an_unknown_question_is_not_found() {
    let fixture = Fixture::new().await;
    let approver = AskUserApprover::new().with_runtime_service(fixture.runtime.clone());
    let unknown = approver
        .resolve("never-asked", answer("yes"))
        .await
        .expect("an unknown id is not a failure");
    assert!(unknown.is_none());
}

/// A second process sharing the database: its own service over the same
/// database file, with its own run registry.
fn other_process(fixture: &Fixture) -> SessionRuntimeService {
    SessionRuntimeService::new(fixture.runtime.db(), UiEventBus::new())
}

async fn resume_from(process: &SessionRuntimeService) -> super::chat::WaitingTurnResume {
    super::chat::queue_waiting_turn_resume(
        process,
        "session",
        super::chat::ResumeRequestKind::Authorization,
        super::chat::ResumeResolution::Approved,
    )
    .await
    .expect("the resume schedules")
}

/// The resume path of the process that won the commit must not touch the
/// state of a session whose run is live in another process: that run owns
/// the state.
#[tokio::test]
async fn a_resume_leaves_the_state_of_a_session_with_a_live_run_alone() {
    init_executor();
    let fixture = Fixture::new().await;
    let holder = fixture
        .runtime
        .runs()
        .acquire("session")
        .await
        .expect("the live run takes the lease");
    let winner = other_process(&fixture);

    let resumed = bounded("the resume", resume_from(&winner)).await;

    assert!(
        matches!(resumed, super::chat::WaitingTurnResume::RunLive(_)),
        "a live run's lease blocks the resume"
    );
    assert_eq!(
        winner.get("session").await.expect("runtime").state,
        crate::session::SessionRuntimeState::Running,
        "the resume rewrote the state of a session with a live run"
    );
    holder
        .finish_idle(crate::session::RunOutcome::Completed)
        .await
        .expect("the live run ends");
}

/// The resume reminder is for a run that is gone. Queued behind a live run
/// it would reach the model after that run, which was already answered
/// through its own prompt: a second, stale delivery.
#[tokio::test]
async fn a_resume_queues_no_reminder_behind_a_live_run() {
    init_executor();
    let fixture = Fixture::new().await;
    let holder = fixture
        .runtime
        .runs()
        .acquire("session")
        .await
        .expect("the live run takes the lease");
    let winner = other_process(&fixture);

    let _ = bounded("the resume", resume_from(&winner)).await;

    assert!(
        !fixture
            .runtime
            .get("session")
            .await
            .expect("runtime")
            .has_pending_messages(),
        "a reminder was queued for a prompt the live run already answered"
    );
    holder
        .finish_idle(crate::session::RunOutcome::Completed)
        .await
        .expect("the live run ends");
}

/// With no live run the resume takes the lease and queues the reminder.
#[tokio::test]
async fn a_resume_of_a_session_without_a_live_run_queues_the_reminder_under_the_lease() {
    init_executor();
    let fixture = Fixture::new().await;
    let winner = other_process(&fixture);

    let resumed = bounded("the resume", resume_from(&winner)).await;

    let super::chat::WaitingTurnResume::Dispatch { guard, runtime } = resumed else {
        panic!("a session without a live run must be resumed");
    };
    assert!(runtime.has_pending_messages(), "the reminder is queued");
    assert!(
        winner.runs().is_held("session").await.expect("lease read"),
        "the resume holds the lease while it dispatches"
    );
    guard
        .finish_idle(crate::session::RunOutcome::Completed)
        .await
        .expect("the resumed run ends");
}
