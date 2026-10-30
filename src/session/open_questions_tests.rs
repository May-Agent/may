//! Open `ask_user` questions against real SQLite database files: the command
//! returns at once, a withdrawn question is gone, and an answer is delivered
//! with its text, even after a restart.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aither_agent::ask_user::{AnswerValue, AskUserArgs, Question, QuestionAnswer};
use aither_core::llm::{Tool, ToolContext};
use tempfile::TempDir;

use super::{
    AnswerError, AskUserAnswer, PendingUserMessageMode, SessionRuntimeService, SessionRuntimeState,
};
use super::{PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::agent::ask_user_tool::{AskUserTool, WithdrawAskUserArgs, WithdrawAskUserTool};
use crate::db::ChatDb;
use crate::ui_event_bus::UiEventBus;

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
    path: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        init_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("open database");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        Self { _dir: dir, path }
    }

    /// A runtime service over a fresh connection to the file, as a process
    /// start opens it.
    async fn service(&self) -> Arc<SessionRuntimeService> {
        let db = ChatDb::open(&self.path).await.expect("open handle");
        Arc::new(SessionRuntimeService::new(db, UiEventBus::new()))
    }
}

fn which_database() -> AskUserArgs {
    AskUserArgs {
        question: Some("Which database?".to_string()),
        option: vec!["PostgreSQL".to_string(), "SQLite".to_string()],
        multi_select: false,
        questions: Vec::new(),
    }
}

fn two_questions() -> AskUserArgs {
    AskUserArgs {
        question: None,
        option: Vec::new(),
        multi_select: false,
        questions: vec![
            Question {
                section: "database".to_string(),
                question: "Which database?".to_string(),
                option: vec!["PostgreSQL".to_string(), "SQLite".to_string()],
                multi_select: false,
            },
            Question {
                section: "notes".to_string(),
                question: "Anything else?".to_string(),
                option: Vec::new(),
                multi_select: false,
            },
        ],
    }
}

/// Ask through the command and return the request id it reports.
async fn ask(service: &Arc<SessionRuntimeService>, args: AskUserArgs) -> String {
    let tool = AskUserTool::new(service.clone(), "session");
    let result = bounded(
        "ask_user to return its request id",
        tool.call(args, ToolContext::new()),
    )
    .await
    .expect("ask_user succeeds");
    let text = result
        .as_text()
        .expect("ask_user answers in text")
        .to_string();
    let open = service
        .get("session")
        .await
        .expect("runtime loads")
        .open_questions;
    open.iter()
        .map(|request| request.id.clone())
        .find(|id| text.contains(id.as_str()))
        .unwrap_or_else(|| {
            panic!("the result must name an open question's id: {text:?} / {open:?}")
        })
}

fn selected(section: &str, value: &str) -> QuestionAnswer {
    QuestionAnswer {
        section: section.to_string(),
        answer: AnswerValue::Selected(vec![value.to_string()]),
    }
}

/// `ask_user` persists the question and returns its request id with the
/// blocking instruction at once; no run is left waiting.
#[tokio::test]
async fn ask_user_returns_its_request_id_without_waiting() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let tool = AskUserTool::new(service.clone(), "session");

    let result = bounded(
        "ask_user to return without an answer",
        tool.call(which_database(), ToolContext::new()),
    )
    .await
    .expect("ask_user succeeds");
    let text = result.as_text().expect("text result");

    let runtime = service.get("session").await.expect("runtime loads");
    assert_eq!(runtime.open_questions.len(), 1, "the question is persisted");
    let open = &runtime.open_questions[0];
    assert!(
        text.contains(&format!("BLOCKING {}", open.id)),
        "the result tells the agent how to mark the blocked tasks: {text:?}"
    );
    assert_eq!(open.questions[0].question, "Which database?");
    assert_eq!(open.questions[0].option, ["PostgreSQL", "SQLite"]);
    assert_eq!(runtime.state, SessionRuntimeState::Idle, "no run waits");
    assert!(runtime.waiting_ask_user.is_none());
}

/// A withdrawn question leaves the user's view, and an answer to it is
/// neither committed nor delivered.
#[tokio::test]
async fn a_withdrawn_question_is_gone_and_its_answer_is_not_delivered() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let kept = ask(&service, which_database()).await;
    let withdrawn = ask(&service, which_database()).await;

    let withdraw = WithdrawAskUserTool::new(service.clone(), "session");
    bounded(
        "withdraw_ask_user to return",
        withdraw.call(
            WithdrawAskUserArgs {
                request_id: withdrawn.clone(),
            },
            ToolContext::new(),
        ),
    )
    .await
    .expect("the open question is withdrawn");

    let runtime = service.get("session").await.expect("runtime loads");
    assert_eq!(
        runtime
            .open_questions
            .iter()
            .map(|request| request.id.as_str())
            .collect::<Vec<_>>(),
        [kept.as_str()],
        "only the question that was not withdrawn stays in the user's view"
    );

    let late = service
        .answer_ask_user(&withdrawn, &[selected("", "SQLite")])
        .await
        .expect("answering a withdrawn question is not a storage failure");
    assert!(late.is_none(), "the withdrawn question takes no answer");
    let runtime = service.get("session").await.expect("runtime loads");
    assert!(
        !runtime.has_pending_messages(),
        "nothing is delivered for a withdrawn question"
    );
    assert_eq!(runtime.open_questions.len(), 1);

    let again = withdraw
        .call(
            WithdrawAskUserArgs {
                request_id: withdrawn,
            },
            ToolContext::new(),
        )
        .await;
    assert!(
        again.is_err(),
        "withdrawing twice reports the question is not open"
    );
}

/// An answer commits the response and queues a steer message carrying the
/// request id, each question and the full answer.
#[tokio::test]
async fn an_answer_is_queued_as_a_steer_message_with_its_text() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let id = ask(&service, two_questions()).await;

    let committed = service
        .answer_ask_user(
            &id,
            &[
                selected("database", "SQLite"),
                QuestionAnswer {
                    section: "notes".to_string(),
                    answer: AnswerValue::Custom("keep it small".to_string()),
                },
            ],
        )
        .await
        .expect("the answer commits")
        .expect("the question was open");
    let AskUserAnswer::Committed(committed) = committed else {
        panic!("the answer is committed by this call: {committed:?}");
    };
    assert!(committed.delivered);

    let runtime = service.get("session").await.expect("runtime loads");
    assert!(
        runtime.open_questions.is_empty(),
        "an answered question is no longer open"
    );
    let message = runtime
        .pending_immediate
        .expect("the answer is queued as a steer message");
    assert_eq!(message.delivery_mode, PendingUserMessageMode::Steer);
    let text = message
        .hidden_context_xml
        .expect("the answer rides hidden context");
    for expected in [
        id.as_str(),
        "Question: Which database?",
        "Answer: SQLite",
        "Question: Anything else?",
        "Answer: keep it small",
        "BLOCKING",
    ] {
        assert!(text.contains(expected), "{expected:?} missing from {text}");
    }

    let responses = service
        .db()
        .get_messages_by_role("session", "ask_user_response")
        .await
        .expect("history loads");
    assert_eq!(responses.len(), 1, "the response joined the history");

    let retry = service
        .answer_ask_user(
            &id,
            &[selected("database", "SQLite"), selected("notes", "x")],
        )
        .await
        .expect("a retry is not a storage failure");
    assert!(
        matches!(retry, Some(AskUserAnswer::AlreadyAnswered { ref session_id }) if session_id == "session"),
        "a retry finds the committed state: {retry:?}"
    );
    let runtime = service.get("session").await.expect("runtime loads");
    assert!(
        runtime.queued_messages.is_empty(),
        "a retry delivers nothing more"
    );
}

/// Answers that do not match the questions are refused and commit nothing.
#[tokio::test]
async fn answers_that_do_not_fit_the_question_commit_nothing() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let id = ask(&service, two_questions()).await;

    let refused = service
        .answer_ask_user(&id, &[selected("database", "SQLite")])
        .await;
    assert!(
        matches!(refused, Err(super::AnswerError::Mismatch(_))),
        "{refused:?}"
    );
    let runtime = service.get("session").await.expect("runtime loads");
    assert_eq!(runtime.open_questions.len(), 1, "the question stays open");
    assert!(!runtime.has_pending_messages());
}

/// A question outlives the process: after a restart (every handle dropped and
/// the file opened afresh) the answer is delivered with its text.
#[tokio::test]
async fn an_answer_after_a_restart_is_delivered_with_its_text() {
    let fixture = Fixture::new().await;
    let id = {
        let before_restart = fixture.service().await;
        let id = ask(&before_restart, which_database()).await;
        drop(before_restart);
        id
    };

    let after_restart = fixture.service().await;
    let runtime = after_restart.get("session").await.expect("runtime loads");
    assert_eq!(
        runtime.open_questions.len(),
        1,
        "the question survived the restart"
    );
    assert_eq!(runtime.open_questions[0].id, id);

    after_restart
        .answer_ask_user(&id, &[selected("", "PostgreSQL")])
        .await
        .expect("the answer commits")
        .expect("the question was still open");
    let delivered = after_restart
        .get("session")
        .await
        .expect("runtime loads")
        .pending_immediate
        .expect("the answer is queued");
    let text = delivered.hidden_context_xml.expect("hidden context");
    assert!(text.contains(&id), "{text}");
    assert!(text.contains("Question: Which database?"), "{text}");
    assert!(text.contains("Answer: PostgreSQL"), "{text}");
}

/// A question another session asked cannot be withdrawn from this one.
#[tokio::test]
async fn a_session_cannot_withdraw_another_sessions_question() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    service
        .db()
        .create_session("other", Some("Other"), Some("test-model"))
        .await
        .expect("create the other session");
    let id = ask(&service, which_database()).await;

    let withdrawn = service
        .withdraw_question("other", &id)
        .await
        .expect("withdraw is not a storage failure");
    assert!(!withdrawn);
    assert_eq!(
        service
            .get("session")
            .await
            .expect("runtime loads")
            .open_questions
            .len(),
        1
    );
}

/// An answer that chooses nothing is no answer: it commits nothing, and the
/// question stays open. There is no skipped state.
#[tokio::test]
async fn an_answer_that_chooses_nothing_commits_nothing_and_leaves_the_question_open() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let id = ask(&service, which_database()).await;

    for empty in [
        AnswerValue::Selected(Vec::new()),
        AnswerValue::Selected(vec![" ".to_string()]),
        AnswerValue::Custom("  ".to_string()),
        AnswerValue::Skipped,
    ] {
        let refused = service
            .answer_ask_user(
                &id,
                &[QuestionAnswer {
                    section: String::new(),
                    answer: empty.clone(),
                }],
            )
            .await;
        assert!(
            matches!(refused, Err(AnswerError::Mismatch(_))),
            "{empty:?} is refused: {refused:?}"
        );
    }

    let runtime = service.get("session").await.expect("runtime loads");
    assert_eq!(runtime.open_questions.len(), 1, "the question stays open");
    assert!(!runtime.has_pending_messages(), "nothing is delivered");
    let responses = service
        .db()
        .get_messages_by_role("session", "ask_user_response")
        .await
        .expect("history loads");
    assert!(responses.is_empty(), "no response joined the history");
}

/// The delivered answer renders without the blank lines the template's loop
/// used to add around every entry.
#[tokio::test]
async fn the_delivered_answer_has_no_blank_lines() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let id = ask(&service, two_questions()).await;
    service
        .answer_ask_user(
            &id,
            &[selected("database", "SQLite"), selected("notes", "none")],
        )
        .await
        .expect("the answer commits");
    let text = service
        .get("session")
        .await
        .expect("runtime loads")
        .pending_immediate
        .expect("the answer is queued")
        .hidden_context_xml
        .expect("hidden context")
        .to_string();
    assert!(!text.contains("\n\n"), "no blank line in {text:?}");
    assert!(
        text.contains("Answer: SQLite\nQuestion: Anything else?"),
        "{text:?}"
    );
}

/// A prompt a run blocked on is an internal decision, not a question the
/// agent asked: answering it queues nothing for the agent.
#[tokio::test]
async fn an_answer_to_a_blocking_prompt_is_not_delivered_as_an_agent_question() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    service
        .set_waiting_ask_user(
            "session",
            Some(PersistedAskUserRequest {
                id: "ssh-prompt".to_string(),
                questions: vec![PersistedAskUserQuestion {
                    section: String::new(),
                    question: "Allow SSH to host?".to_string(),
                    option: vec!["Allow".to_string(), "Deny".to_string()],
                    multi_select: false,
                }],
            }),
        )
        .await
        .expect("the prompt is persisted");

    let answered = service
        .answer_ask_user("ssh-prompt", &[selected("", "Allow")])
        .await
        .expect("the answer commits")
        .expect("the prompt was pending");
    let AskUserAnswer::Committed(committed) = answered else {
        panic!("committed by this call: {answered:?}");
    };
    assert!(committed.blocking);
    assert!(!committed.delivered, "nothing is queued for the agent");
    assert!(
        !service
            .get("session")
            .await
            .expect("runtime loads")
            .has_pending_messages()
    );
}

/// Deleting a session deletes its open questions with it.
#[tokio::test]
async fn deleting_a_session_deletes_its_open_questions() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let id = ask(&service, which_database()).await;

    service
        .db()
        .delete_session("session")
        .await
        .expect("the session is deleted");

    assert!(
        service
            .db()
            .pending_ask_user_request(&id)
            .await
            .expect("lookup works")
            .is_none(),
        "the open question did not outlive its session"
    );
}

/// Reading the open questions while they are being answered never shows a
/// request without its questions.
#[tokio::test(flavor = "multi_thread")]
async fn open_questions_are_read_whole_while_they_are_answered() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let mut ids = Vec::new();
    for _ in 0..12 {
        ids.push(ask(&service, which_database()).await);
    }

    let reader = {
        let service = service.clone();
        tokio::spawn(async move {
            for _ in 0..60 {
                for request in service
                    .db()
                    .open_ask_user_questions("session")
                    .await
                    .expect("read")
                {
                    assert!(
                        !request.questions.is_empty(),
                        "request {} was read without its questions",
                        request.id
                    );
                }
            }
        })
    };
    for id in &ids {
        service
            .answer_ask_user(id, &[selected("", "SQLite")])
            .await
            .expect("the answer commits");
    }
    bounded("the reader to finish", reader)
        .await
        .expect("the reader saw whole requests");
}
