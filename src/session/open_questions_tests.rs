//! Open `ask_user` questions against real SQLite database files: the command
//! returns at once, a withdrawn question is gone, and an answer is delivered
//! with its text, even after a restart.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::harness::ask_user::{AnswerValue, QuestionAnswer};
use aither_core::llm::{Tool, ToolContext};
use tempfile::TempDir;

use super::{
    AnswerError, AskUserAnswer, PendingUserMessageMode, SessionRuntimeService, SessionRuntimeState,
};
use super::{PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::agent::ask_user_tool::{
    AskUserArgs, AskUserBlockerResolver, AskUserTool, AskedQuestion, WithdrawAskUserArgs,
    WithdrawAskUserTool,
};
use crate::db::ChatDb;
use crate::harness::working_document::BlockerResolver;
use crate::ui_event_bus::UiEventBus;

/// Upper bound on any single wait in this module.
const WAIT_BOUND: Duration = Duration::from_secs(20);

/// Bound `future` by [`WAIT_BOUND`], naming `what` it waits for on expiry.
async fn bounded<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT_BOUND, future)
        .await
        .unwrap_or_else(|_| panic!("timed out after {WAIT_BOUND:?} waiting for {what}"))
}

struct Fixture {
    dir: TempDir,
    path: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = ChatDb::open(&path).await.expect("open database");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        Self { dir, path }
    }

    /// A runtime service over a fresh connection to the file, as a process
    /// start opens it.
    async fn service(&self) -> Arc<SessionRuntimeService> {
        let db = ChatDb::open(&self.path).await.expect("open handle");
        Arc::new(SessionRuntimeService::new(
            db,
            UiEventBus::new(),
            self.home(),
        ))
    }

    /// The May home the fixture's sessions live under.
    fn home(&self) -> crate::session::MayHomePaths {
        crate::session::MayHomePaths::new(self.dir.path().join("home"))
    }

    /// A runtime service and the `tasks.md` path of "session", whose
    /// directory exists.
    async fn service_with_home(&self) -> (Arc<SessionRuntimeService>, std::path::PathBuf) {
        let tasks = self.home().session_paths("session").tasks_path();
        std::fs::create_dir_all(tasks.parent().expect("a session directory"))
            .expect("session directory");
        (self.service().await, tasks)
    }
}

/// The text the answer queued for the agent.
async fn queued_reminder(service: &SessionRuntimeService) -> String {
    service
        .get("session")
        .await
        .expect("runtime loads")
        .pending_immediate
        .expect("the answer is queued")
        .hidden_context_xml
        .expect("hidden context")
        .to_string()
}

/// Answering a question removes every mark naming it from `tasks.md`, leaves
/// other marks, names the rows it unblocked in the reminder and raises no
/// dead-blocker alert; a question blocking no row says so.
#[tokio::test]
async fn an_answer_clears_the_marks_naming_it_and_names_the_rows() {
    let fixture = Fixture::new().await;
    let (service, tasks) = fixture.service_with_home().await;
    let blocking = ask(&service, which_database()).await;
    let other = ask(&service, which_database()).await;
    std::fs::write(
        &tasks,
        format!(
            "## Doing\n- 迁移数据库\n  BLOCKING {blocking}\n- データベース設計 BLOCKING {blocking}\n- 문서\n  BLOCKING {other}\n\n## Queued\n- tests\n",
            blocking = blocking.word,
            other = other.word,
        ),
    )
    .expect("tasks.md");

    service
        .answer_ask_user(&blocking.id, &[selected("", "SQLite")])
        .await
        .expect("the answer commits");

    assert_eq!(
        std::fs::read_to_string(&tasks).expect("tasks.md"),
        format!(
            "## Doing\n- 迁移数据库\n- データベース設計\n- 문서\n  BLOCKING {other}\n\n## Queued\n- tests\n",
            other = other.word,
        )
    );
    let reminder = queued_reminder(&service).await;
    assert!(
        reminder.contains(&format!(
            "no longer marks these rows BLOCKING {blocking}; they are pending again:\n1. 迁移数据库\n2. データベース設計\nContinue",
            blocking = blocking.word,
        )),
        "{reminder}"
    );
    assert!(!reminder.contains("\n\n"), "{reminder:?}");

    // The document no longer names the question: no alert.
    let mut document = crate::tasks_md::schema::working_document(
        tasks.parent().expect("a session directory"),
        service.task_events("session"),
    );
    aither_agent::ContextSource::blocks(
        &mut document,
        aither_agent::Moment::Request { reassembled: false },
    )
    .await;
    let alert =
        aither_agent::ContextSource::blocks(&mut document, aither_agent::Moment::ToolTurnEnd).await;
    assert!(
        alert.is_empty(),
        "{:?}",
        alert
            .iter()
            .map(|block| block.block().content().to_string())
            .collect::<Vec<_>>()
    );
}

/// A question that blocks no row edits nothing and says so.
#[tokio::test]
async fn an_answer_to_a_question_that_blocks_nothing_edits_nothing() {
    let fixture = Fixture::new().await;
    let (service, tasks) = fixture.service_with_home().await;
    let id = ask(&service, which_database()).await;
    let document = "## Doing\n- wire it\n  BLOCKING waiting on review\n";
    std::fs::write(&tasks, document).expect("tasks.md");

    service
        .answer_ask_user(&id.id, &[selected("", "SQLite")])
        .await
        .expect("the answer commits");

    assert_eq!(std::fs::read_to_string(&tasks).expect("tasks.md"), document);
    let reminder = queued_reminder(&service).await;
    assert!(
        reminder.contains(&format!(
            "No row of tasks.md was marked BLOCKING {}.",
            id.word
        )),
        "{reminder}"
    );
}

fn which_database() -> AskUserArgs {
    AskUserArgs {
        question: Some("Which database?".to_string()),
        question_text: None,
        option: vec!["PostgreSQL".to_string(), "SQLite".to_string()],
        multi_select: false,
        secret: None,
        secret_scope: None,
        questions: Vec::new(),
    }
}

fn two_questions() -> AskUserArgs {
    AskUserArgs {
        question: None,
        question_text: None,
        option: Vec::new(),
        multi_select: false,
        secret: None,
        secret_scope: None,
        questions: vec![
            AskedQuestion {
                section: "database".to_string(),
                question: "Which database?".to_string(),
                option: vec!["PostgreSQL".to_string(), "SQLite".to_string()],
                multi_select: false,
                secret: None,
            },
            AskedQuestion {
                section: "notes".to_string(),
                question: "Anything else?".to_string(),
                option: Vec::new(),
                multi_select: false,
                secret: None,
            },
        ],
    }
}

/// A question asked through the command.
struct Asked {
    /// Its request id, which the user's answer names.
    id: String,
    /// The one word the model knows it by.
    word: String,
}

/// Ask through the command and return the question it reports.
async fn ask(service: &Arc<SessionRuntimeService>, args: AskUserArgs) -> Asked {
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
    for request in &open {
        let word = service
            .db()
            .open_ask_user_question_word(&request.id)
            .await
            .expect("the question loads")
            .expect("an open question has a word");
        if text.contains(&format!("Question {word} is open")) {
            assert!(
                !text.contains(&request.id),
                "the model is shown the word, not the request id: {text:?}"
            );
            return Asked {
                id: request.id.clone(),
                word,
            };
        }
    }
    panic!("the result must name an open question's word: {text:?} / {open:?}")
}

fn selected(section: &str, value: &str) -> QuestionAnswer {
    QuestionAnswer {
        section: section.to_string(),
        answer: AnswerValue::Selected(vec![value.to_string()]),
    }
}

/// A `BLOCKING` mark holds while its question is open; one naming an
/// answered, withdrawn or fabricated request does not — while a payload
/// naming no request the session issued is free text and trusted.
#[tokio::test]
async fn blocker_marks_hold_only_while_their_question_is_open() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let resolver = AskUserBlockerResolver::new(service.clone(), "session");
    let open = ask(&service, which_database()).await;
    let answered = ask(&service, which_database()).await;
    service
        .answer_ask_user(&answered.id, &[selected("", "SQLite")])
        .await
        .expect("the answer commits");
    let withdrawn = ask(&service, which_database()).await;
    WithdrawAskUserTool::new(service.clone(), "session")
        .call(
            WithdrawAskUserArgs {
                request_id: withdrawn.word.clone(),
            },
            ToolContext::new(),
        )
        .await
        .expect("the open question is withdrawn");

    assert_eq!(resolver.is_open(&open.word).await, Ok(true));
    assert_eq!(resolver.is_open(&answered.word).await, Ok(false));
    assert_eq!(resolver.is_open(&withdrawn.word).await, Ok(false));
    assert_eq!(resolver.is_open("waiting on the user").await, Ok(true));
    assert_eq!(resolver.is_open("never-issued-words").await, Ok(true));

    let distinct = [&open.word, &answered.word, &withdrawn.word];
    assert_eq!(
        distinct
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "questions open together never share a word: {distinct:?}"
    );
}

/// An answer to a question the agent asked reaches the session's `tasks.md`
/// inbox, whichever agent build takes it.
#[tokio::test]
async fn an_answer_is_recorded_on_the_session_tasks_inbox() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let Asked { id, word: asked } = ask(&service, which_database()).await;
    service
        .answer_ask_user(&id, &[selected("", "SQLite")])
        .await
        .expect("the answer commits");

    let sandbox = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        sandbox.path().join("tasks.md"),
        format!("## Doing\n- pick a db BLOCKING {asked}\n"),
    )
    .expect("tasks.md");
    let mut tasks =
        crate::tasks_md::schema::working_document(sandbox.path(), service.task_events("session"));
    aither_agent::ContextSource::blocks(
        &mut tasks,
        aither_agent::Moment::Request { reassembled: false },
    )
    .await;
    let alert = aither_agent::ContextSource::blocks(&mut tasks, aither_agent::Moment::ToolTurnEnd)
        .await
        .into_iter()
        .next()
        .expect("the row waiting on it is told");
    assert!(
        alert
            .block()
            .content()
            .contains(&format!("Question {asked} was answered")),
        "{}",
        alert.block().content()
    );
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
    let word = service
        .db()
        .open_ask_user_question_word(&open.id)
        .await
        .expect("the question loads")
        .expect("the question has a word");
    assert!(
        text.contains(&format!("BLOCKING {word}")),
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
                request_id: withdrawn.word.clone(),
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
        [kept.id.as_str()],
        "only the question that was not withdrawn stays in the user's view"
    );

    let late = service
        .answer_ask_user(&withdrawn.id, &[selected("", "SQLite")])
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
                request_id: withdrawn.word,
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
    let Asked { id, word } = ask(&service, two_questions()).await;

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
        word.as_str(),
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
    let Asked { id, .. } = ask(&service, two_questions()).await;

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
    let Asked { id, word } = {
        let before_restart = fixture.service().await;
        let asked = ask(&before_restart, which_database()).await;
        drop(before_restart);
        asked
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
    assert!(
        text.contains(&format!("question {word}")),
        "the word survives the restart: {text}"
    );
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
    let Asked { id, .. } = ask(&service, which_database()).await;

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
    let Asked { id, .. } = ask(&service, which_database()).await;

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
    let Asked { id, .. } = ask(&service, two_questions()).await;
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
                    secret: None,
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
    let Asked { id, .. } = ask(&service, which_database()).await;

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
        ids.push(ask(&service, which_database()).await.id);
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

/// The command reports the question as the card of the terminal call that
/// asked it, and the committed answer joins the card by the request id.
#[tokio::test]
async fn an_asked_question_is_the_card_of_its_call_and_takes_its_answer() {
    use crate::command_reading::Card;
    use aither_core::llm::{AgentIdentity, CallIdentity, ToolCallId};

    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let cards = crate::agent::command_cards::CommandCards::default();
    let updates = cards.claim_updates().expect("the first reader claims");
    let tool = AskUserTool::new(service.clone(), "session").with_cards(cards);
    let cx = ToolContext::new().with_identity(CallIdentity::new(
        ToolCallId::new("c1"),
        AgentIdentity::Main,
    ));
    bounded("ask_user to return", tool.call(which_database(), cx))
        .await
        .expect("ask_user succeeds");

    let update = updates.try_recv().expect("the question is reported");
    assert_eq!(update.call_id, "c1");
    let card = update.card.expect("a card");
    let Card::AskUser {
        request_id,
        questions,
        answers,
    } = &card
    else {
        panic!("an ask_user card, got {card:?}");
    };
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].question, "Which database?");
    assert_eq!(questions[0].options, ["PostgreSQL", "SQLite"]);
    assert_eq!(answers, &None);

    service
        .answer_ask_user(request_id, &[selected("", "SQLite")])
        .await
        .expect("the answer commits");
    let committed = service
        .committed_ask_user_answers(request_id)
        .await
        .expect("read")
        .expect("committed");
    let Card::AskUser {
        answers: Some(answers),
        ..
    } = card.clone().answered(&committed)
    else {
        panic!("the answered card carries its answers");
    };
    assert_eq!(answers[0].text, "SQLite");
}

/// A call outside a terminal call has no card.
#[tokio::test]
async fn a_question_outside_a_terminal_call_reports_no_card() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let cards = crate::agent::command_cards::CommandCards::default();
    let updates = cards.claim_updates().expect("the first reader claims");
    let tool = AskUserTool::new(service.clone(), "session").with_cards(cards);
    tool.call(which_database(), ToolContext::new())
        .await
        .expect("ask_user succeeds");
    assert!(updates.try_recv().is_err());
}
