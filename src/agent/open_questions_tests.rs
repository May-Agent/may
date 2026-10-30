//! Open `ask_user` questions across compaction: the request after a
//! compaction lists exactly the questions still open, and the user's view
//! (the runtime summary) agrees with it.

use aither_agent::ask_user::{AnswerValue, AskUserArgs, QuestionAnswer};
use aither_core::llm::{Tool, ToolContext};

use super::ask_user_tool::{AskUserTool, WithdrawAskUserArgs, WithdrawAskUserTool};
use super::idle_compaction_tests::{SESSION_MODEL, Session, StubModel, run_turn, saved_config};

fn question(text: &str) -> AskUserArgs {
    AskUserArgs {
        question: Some(text.to_string()),
        option: Vec::new(),
        multi_select: false,
        questions: Vec::new(),
    }
}

/// Asks `text` and returns the request id of the question it opened.
async fn ask(session: &Session, text: &str) -> String {
    let runtime = session.parts.runtime.clone().expect("a runtime");
    let session_id = session.agent.session_id().as_str().to_string();
    let before = runtime
        .get(&session_id)
        .await
        .expect("runtime loads")
        .open_questions;
    AskUserTool::new(runtime.clone(), session_id.clone())
        .call(question(text), ToolContext::new())
        .await
        .expect("ask_user succeeds");
    runtime
        .get(&session_id)
        .await
        .expect("runtime loads")
        .open_questions
        .into_iter()
        .map(|request| request.id)
        .find(|id| !before.iter().any(|known| &known.id == id))
        .expect("the new question is open")
}

/// After a compaction the next request names every open question by id and
/// text, and neither the answered nor the withdrawn one; the runtime summary
/// shown to the user lists the same set.
#[tokio::test]
async fn open_questions_survive_compaction_in_the_next_request() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = Box::pin(Session::start_with_runtime(&saved_config(&stub, None))).await;
    let session_id = session.agent.session_id().as_str().to_string();
    let runtime = session.parts.runtime.clone().expect("a runtime");
    // The agent checkpoints under the session's run lease, as a served run does.
    let _lease = runtime
        .runs()
        .acquire(&session_id)
        .await
        .expect("the run lease is free");

    let kept = ask(&session, "Which cache layer?").await;
    let answered = ask(&session, "Which license?").await;
    let withdrawn = ask(&session, "Which logo?").await;

    runtime
        .answer_ask_user(
            &answered,
            &[QuestionAnswer {
                section: String::new(),
                answer: AnswerValue::Custom("MIT".to_string()),
            }],
        )
        .await
        .expect("the answer commits")
        .expect("the question was open");
    WithdrawAskUserTool::new(runtime.clone(), session_id.clone())
        .call(
            WithdrawAskUserArgs {
                request_id: withdrawn.clone(),
            },
            ToolContext::new(),
        )
        .await
        .expect("the question is withdrawn");

    run_turn(&mut session.agent).await;
    session
        .agent
        .compact(None)
        .await
        .expect("the session compacts");
    run_turn(&mut session.agent).await;

    let requests = stub.requests.lock().await;
    let after = requests.last().expect("a request after the compaction");
    assert!(
        !after.contains("Your context window is being compacted"),
        "the last request is a turn, not the compaction"
    );
    assert!(after.contains(&kept), "open question id missing: {after}");
    assert!(
        after.contains(&format!("{kept}:\\n- Which cache layer?\\n\"")),
        "the entry has no blank line around it: {after}"
    );
    assert!(
        !after.contains("Which logo?") && !after.contains(&withdrawn),
        "a withdrawn question is listed"
    );
    assert!(
        !after.contains("Which license?") && !after.contains(&answered),
        "an answered question is listed as open"
    );
    drop(requests);

    let shown: Vec<_> = runtime
        .get(&session_id)
        .await
        .expect("runtime loads")
        .open_questions
        .into_iter()
        .map(|request| request.id)
        .collect();
    assert_eq!(shown, [kept], "the user's view lists the same open set");
}
