use std::path::Path;
use std::sync::Arc;

use crate::harness::ask_user::{AnswerValue, QuestionAnswer};
use aither_core::llm::{Tool, ToolContext};

use super::{GlobalRulesArgs, GlobalRulesTool, RulesImport, SkillCopyReport, copy_skills};
use crate::agent::ask_user_tool::{AskUserArgs, AskUserTool};
use crate::session::import::ForeignAgent;
use crate::skill::SkillManager;

/// Rules with the bytes a careless copy would change: CRLF, trailing blanks,
/// a tab, non-ASCII text and no final newline.
const AWKWARD_RULES: &[u8] =
    "# Rules\r\n- Answer briefly.  \n\tIndented — ✓\n\nNo final newline".as_bytes();

fn write(path: &Path, content: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("parent dir");
    std::fs::write(path, content).expect("write file");
}

fn skill(dir: &Path, name: &str) {
    write(
        &dir.join(name).join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\n\n# {name}\n").as_bytes(),
    );
}

#[tokio::test]
async fn skills_are_copied_and_a_taken_name_is_skipped() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let source = ForeignAgent::Claude.skills_dir(&home);
    skill(&source, "alpha");
    skill(&source, "beta");
    // A directory without SKILL.md is not a skill.
    write(&source.join("notes/README.md"), b"not a skill");
    let may_skills = temp.path().join("may-skills");
    skill(&may_skills, "beta");
    let manager = SkillManager::from_skills_dir(may_skills.clone());

    let report = copy_skills(&source, &manager).await.expect("skills copy");

    assert_eq!(
        report,
        SkillCopyReport {
            imported: vec!["alpha".to_string()],
            skipped: vec!["beta".to_string()],
        }
    );
    assert!(may_skills.join("alpha/SKILL.md").is_file());
    assert!(!may_skills.join("notes").exists());
    // An agent with no skills directory has nothing to copy.
    assert_eq!(
        copy_skills(&temp.path().join("none"), &manager)
            .await
            .expect("a missing directory copies nothing"),
        SkillCopyReport::default()
    );
}

/// A session row, its runtime and the `global-rules` command of an import
/// from `home` into `may_dir`.
struct ImportSession {
    runtime: Arc<crate::session::SessionRuntimeService>,
    session_id: String,
    tool: GlobalRulesTool,
    sandbox: std::path::PathBuf,
    global_rules: std::path::PathBuf,
    _temp: tempfile::TempDir,
}

async fn import_session(rules: &[u8]) -> ImportSession {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let may_dir = temp.path().join(".may");
    std::fs::create_dir_all(&may_dir).expect("may dir");
    write(&ForeignAgent::Claude.rules_file(&home), rules);
    let db = crate::db::ChatDb::open(&temp.path().join("history.db"))
        .await
        .expect("open db");
    let session_id = "import-session".to_string();
    db.create_session(&session_id, Some("Import"), Some("test-model"))
        .await
        .expect("session row");
    let runtime = Arc::new(crate::session::SessionRuntimeService::new(
        db.clone(),
        crate::ui_event_bus::UiEventBus::new(),
        crate::session::MayHomePaths::new(may_dir.clone()),
    ));
    let sandbox = temp.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).expect("sandbox");
    let import = RulesImport::new(ForeignAgent::Claude, &home, &may_dir);
    ImportSession {
        runtime,
        tool: GlobalRulesTool::new(import, session_id.clone(), db, sandbox.clone(), None),
        session_id,
        sandbox,
        global_rules: may_dir.join("AGENTS.md"),
        _temp: temp,
    }
}

async fn run(session: &ImportSession, args: GlobalRulesArgs) -> String {
    let result = session
        .tool
        .call(args, ToolContext::new())
        .await
        .expect("global-rules runs");
    format!("{result:?}")
}

#[tokio::test]
async fn keep_installs_the_source_byte_identical() {
    let session = import_session(AWKWARD_RULES).await;

    let result = run(&session, GlobalRulesArgs::Keep).await;

    assert!(result.contains("unchanged"), "{result}");
    assert_eq!(
        std::fs::read(&session.global_rules).expect("AGENTS.md installed"),
        AWKWARD_RULES
    );
}

#[tokio::test]
async fn write_is_refused_until_the_user_answers_a_question() {
    let session = import_session(b"Always use npm.\n").await;
    write(&session.sandbox.join("rules.md"), b"Always use bun.\n");
    let write_rules = || GlobalRulesArgs::Write {
        file: "rules.md".to_string(),
    };

    let refused = run(&session, write_rules()).await;
    assert!(refused.contains("Refused"), "{refused}");
    assert!(!session.global_rules.exists());

    // An open question approves nothing.
    AskUserTool::new(session.runtime.clone(), session.session_id.clone())
        .call(
            AskUserArgs {
                question: Some("Change `npm` to `bun`?".to_string()),
                positional_question: None,
                option: vec!["Change it".to_string(), "Keep npm".to_string()],
                multi_select: false,
                secret: None,
                secret_scope: None,
                questions: Vec::new(),
            },
            ToolContext::new(),
        )
        .await
        .expect("ask_user succeeds");
    let still_refused = run(&session, write_rules()).await;
    assert!(still_refused.contains("Refused"), "{still_refused}");
    assert!(!session.global_rules.exists());

    let request_id = session
        .runtime
        .get(&session.session_id)
        .await
        .expect("runtime loads")
        .open_questions
        .first()
        .expect("the question is open")
        .id
        .clone();
    session
        .runtime
        .answer_ask_user(
            &request_id,
            &[QuestionAnswer {
                section: String::new(),
                answer: AnswerValue::Selected(vec!["Change it".to_string()]),
            }],
        )
        .await
        .expect("the answer commits")
        .expect("the question was open");

    let installed = run(&session, write_rules()).await;
    assert!(installed.contains("Installed rules.md"), "{installed}");
    assert_eq!(
        std::fs::read(&session.global_rules).expect("AGENTS.md installed"),
        b"Always use bun.\n"
    );
}

#[test]
fn the_brief_names_the_source_and_both_commands() {
    let import = RulesImport::new(
        ForeignAgent::Codex,
        Path::new("/home/user"),
        Path::new("/home/user/.may"),
    );
    let brief = import.brief();
    assert!(brief.starts_with("Import my Codex global rules"), "{brief}");
    assert!(
        brief.contains("Read /home/user/.codex/AGENTS.md."),
        "{brief}"
    );
    assert!(brief.contains("`global-rules keep`"), "{brief}");
    assert!(brief.contains("`global-rules write <file>`"), "{brief}");
}
