//! A rules import session driven by a scripted model: `global-rules` exists
//! only in it, `keep` installs the source byte for byte, and `write` installs
//! a rewrite only after the user answered the agent's question.

use aither_agent::ask_user::{AnswerValue, QuestionAnswer};

use super::compact_tool_tests::{ScriptedModel, Step, config_for, lease, run, terminal};
use super::idle_compaction_tests::Session;
use crate::config::ContextConfig;
use crate::rules_import::RulesImport;
use crate::session::import::ForeignAgent;

/// Rules with the bytes a careless copy would change.
const AWKWARD_RULES: &[u8] =
    "# Rules\r\n- Answer briefly.  \n\tIndented — ✓\n\nNo final newline".as_bytes();

/// The command table entry of `global-rules`.
const COMMAND_ENTRY: &str = "- global-rules: ";

/// Starts a session of `model`, writes `rules` as the user's Claude Code
/// rules and rebuilds the agent as that rules import session.
async fn import_session(model: &ScriptedModel, rules: &[u8]) -> Session {
    let config = config_for(model, ContextConfig::default());
    let mut session = Box::pin(Session::start_with_runtime(&config)).await;
    let home = session.temp.path().join("home");
    let rules_file = ForeignAgent::Claude.rules_file(&home);
    std::fs::create_dir_all(rules_file.parent().expect("a parent")).expect("rules dir");
    std::fs::write(&rules_file, rules).expect("write CLAUDE.md");
    let import = RulesImport::new(
        ForeignAgent::Claude,
        &home,
        &session.temp.path().join(".may"),
    );
    session.parts.rules_import = Some(import);
    Box::pin(session.rebuild(&config)).await;
    session
}

async fn system_prompt(session: &mut Session) -> String {
    session
        .agent
        .preview_request_messages("Say hello.", [])
        .await
        .expect("the request previews")
        .first()
        .expect("a system message")
        .content()
        .to_string()
}

fn global_rules(session: &Session) -> std::path::PathBuf {
    session.temp.path().join(".may/AGENTS.md")
}

fn keep(index: usize) -> Step {
    match index {
        0 => terminal("global-rules keep"),
        _ => Step::Text("Installed unchanged."),
    }
}

#[tokio::test]
async fn a_source_with_nothing_to_change_is_installed_byte_identical() {
    let model = ScriptedModel::spawn(keep).await;
    let mut plain = Box::pin(Session::start_with_runtime(&config_for(
        &model,
        ContextConfig::default(),
    )))
    .await;
    assert!(
        !system_prompt(&mut plain).await.contains(COMMAND_ENTRY),
        "only an import session has global-rules"
    );

    let mut session = Box::pin(import_session(&model, AWKWARD_RULES)).await;
    assert!(system_prompt(&mut session).await.contains(COMMAND_ENTRY));
    let _lease = lease(&session).await;
    let brief = RulesImport::new(
        ForeignAgent::Claude,
        &session.temp.path().join("home"),
        &session.temp.path().join(".may"),
    )
    .brief();

    run(&mut session, 0, &brief).await;

    let installed = std::fs::read(global_rules(&session));
    let last = model.last_request().await;
    assert_eq!(
        installed.unwrap_or_else(|error| panic!("AGENTS.md is not installed ({error}): {last}")),
        AWKWARD_RULES
    );
}

fn ask_then_write(index: usize) -> Step {
    match index {
        0 => terminal("printf 'Always use bun.\\n' > rules.md && global-rules write rules.md"),
        // The question holds the turn open until a `tasks.md` row waits on
        // it, so the agent marks the row the way the command's result says.
        1 => terminal(
            "out=$(ask_user --question 'Change `npm` to `bun`?') && id=${out#Question } && \
             printf '## Doing\\n- import the rules BLOCKING %s\\n' \"${id%% *}\" > tasks.md && \
             echo \"$out\"",
        ),
        2 => Step::Text("Waiting for your answer."),
        3 => terminal("global-rules write rules.md"),
        _ => Step::Text("Installed."),
    }
}

#[tokio::test]
async fn a_conflicting_rule_changes_only_after_the_users_answer() {
    let model = ScriptedModel::spawn(ask_then_write).await;
    let mut session = Box::pin(import_session(&model, b"Always use npm.\n")).await;
    let _lease = lease(&session).await;

    run(
        &mut session,
        0,
        "Import my Claude Code global rules into May.",
    )
    .await;

    assert!(
        !global_rules(&session).exists(),
        "nothing is installed before the answer"
    );
    let runtime = session.parts.runtime.clone().expect("a runtime");
    let session_id = session.agent.session_id().as_str().to_string();
    let request_id = runtime
        .get(&session_id)
        .await
        .expect("runtime loads")
        .open_questions
        .first()
        .expect("the agent asked")
        .id
        .clone();
    runtime
        .answer_ask_user(
            &request_id,
            &[QuestionAnswer {
                section: String::new(),
                answer: AnswerValue::Custom("Yes, use bun.".to_string()),
            }],
        )
        .await
        .expect("the answer commits")
        .expect("the question was open");

    run(&mut session, 0, "Continue.").await;

    let last = model.last_request().await;
    assert!(
        last.contains("Refused"),
        "the early write was refused: {last}"
    );
    assert_eq!(
        std::fs::read(global_rules(&session)).expect("AGENTS.md installed"),
        b"Always use bun.\n"
    );
}
