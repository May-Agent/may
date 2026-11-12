//! `MEMORY.md` and `AGENTS.md` as aither live content, through May's own
//! agent against a stub model: a change made after a request carried the
//! prefix reaches the model once, as a diff, while the prefix stays as the
//! provider cached it; a compaction folds the clean version into the prefix
//! and drops the diff.

use std::ops::ControlFlow;

use serde_json::Value;

use super::idle_compaction_tests::{
    COMPACTION_MARKER, SESSION_MODEL, Session, StubModel, saved_config,
};

/// Runs one user turn the way the daemon does: the runtime context is
/// brought up to date, then the run is driven to its end.
async fn turn(session: &mut Session) {
    session.agent.sync_runtime_context().await;
    let _ = session
        .agent
        .run("Say hello.", [])
        .drive((), std::future::pending::<()>(), |(), _| async {
            ControlFlow::Continue(())
        })
        .await
        .expect("the turn runs");
}

/// The text of every message of a request body, in order.
fn messages(body: &str) -> Vec<String> {
    let request: Value = serde_json::from_str(body).expect("the request is JSON");
    request["messages"]
        .as_array()
        .expect("the request has messages")
        .iter()
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect(),
            other => panic!("unexpected message content: {other}"),
        })
        .collect()
}

/// The turn requests the stub served, compactions left out.
async fn turn_requests(stub: &StubModel) -> Vec<Vec<String>> {
    stub.requests
        .lock()
        .await
        .iter()
        .filter(|body| !body.contains(COMPACTION_MARKER))
        .map(|body| messages(body))
        .collect()
}

/// The messages after the system prefix that carry a diff wrapped in `tag`.
fn diff_alerts<'a>(request: &'a [String], tag: &str) -> Vec<&'a String> {
    let open = format!("<{tag} boundary=");
    request[1..]
        .iter()
        .filter(|message| message.contains(&open))
        .collect()
}

/// A session whose memory index holds `memory` and whose launch directory
/// holds `agents_md`, read when the agent is built as a fresh session does.
async fn session_with(stub: &StubModel, memory: &str, agents_md: &str) -> Session {
    let config = saved_config(stub, None);
    let mut session = Box::pin(Session::start(&config)).await;
    std::fs::write(memory_file(&session), memory).expect("write MEMORY.md");
    std::fs::write(session.temp.path().join("cwd/AGENTS.md"), agents_md).expect("write AGENTS.md");
    Box::pin(session.rebuild(&config)).await;
    session
}

fn memory_file(session: &Session) -> std::path::PathBuf {
    let file = crate::session::MayHomePaths::new(session.temp.path().join(".may")).memory_file();
    std::fs::create_dir_all(file.parent().expect("a memories dir")).expect("memories dir");
    file
}

/// Compacts the conversation, as `/compact` does.
async fn compact(session: &mut Session) {
    session
        .agent
        .compact(None)
        .await
        .expect("/compact succeeds")
        .expect("there is a conversation to compact");
}

#[tokio::test]
async fn a_memory_change_mid_session_is_one_diff_then_the_clean_version() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = session_with(&stub, "first fact\n", "rules\n").await;

    turn(&mut session).await;
    std::fs::write(memory_file(&session), "first fact\nsecond fact\n").expect("update MEMORY.md");
    turn(&mut session).await;
    // Re-reading the same version restates nothing.
    turn(&mut session).await;

    let requests = turn_requests(&stub).await;
    let [first, second, third] = requests.as_slice() else {
        panic!("three turns: {requests:?}");
    };
    assert_eq!(second[0], first[0], "the cached prefix stays as sent");
    assert!(!second[0].contains("second fact"), "{}", second[0]);
    let alerts = diff_alerts(second, "memory_diff");
    assert_eq!(alerts.len(), 1, "{second:?}");
    assert!(alerts[0].starts_with("MEMORY.md changed."), "{}", alerts[0]);
    assert!(alerts[0].contains("\n+second fact\n"), "{}", alerts[0]);
    assert!(!alerts[0].contains("+first fact"), "{}", alerts[0]);
    assert_eq!(third[0], first[0]);
    assert_eq!(diff_alerts(third, "memory_diff").len(), 1, "{third:?}");

    compact(&mut session).await;
    turn(&mut session).await;
    let requests = turn_requests(&stub).await;
    let after = requests.last().expect("the turn after compaction");
    assert!(after[0].contains("first fact\nsecond fact"), "{}", after[0]);
    assert_eq!(diff_alerts(after, "memory_diff"), Vec::<&String>::new());
}

#[tokio::test]
async fn an_agents_md_change_mid_session_is_one_diff_then_the_clean_version() {
    let stub = StubModel::spawn(SESSION_MODEL).await;
    let mut session = session_with(&stub, "", "rule one\n").await;

    turn(&mut session).await;
    std::fs::write(
        session.temp.path().join("cwd/AGENTS.md"),
        "rule one\nrule two\n",
    )
    .expect("update AGENTS.md");
    turn(&mut session).await;
    turn(&mut session).await;

    let requests = turn_requests(&stub).await;
    let [first, second, third] = requests.as_slice() else {
        panic!("three turns: {requests:?}");
    };
    assert_eq!(second[0], first[0], "the cached prefix stays as sent");
    assert!(!second[0].contains("rule two"), "{}", second[0]);
    let alerts = diff_alerts(second, "agents_md_diff");
    assert_eq!(alerts.len(), 1, "{second:?}");
    assert!(
        alerts[0].starts_with("The AGENTS.md files changed."),
        "{}",
        alerts[0]
    );
    assert!(alerts[0].contains("\n+rule two\n"), "{}", alerts[0]);
    assert_eq!(diff_alerts(third, "agents_md_diff").len(), 1, "{third:?}");

    compact(&mut session).await;
    turn(&mut session).await;
    let requests = turn_requests(&stub).await;
    let after = requests.last().expect("the turn after compaction");
    assert!(after[0].contains("rule one\nrule two"), "{}", after[0]);
    assert_eq!(diff_alerts(after, "agents_md_diff"), Vec::<&String>::new());
}
