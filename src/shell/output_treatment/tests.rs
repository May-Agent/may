use std::path::PathBuf;
use std::time::Duration;

use aither_core::llm::CommandStream;
use aither_sandbox::{
    Interpreter, SandboxMode, Script, ScriptDir, ScriptOrigin, ScriptResult, ShellScript,
};

use super::*;
use crate::repo_clone::test_sandbox::TestTerminal;

fn treatment_of(script: &str, interpreter: Interpreter) -> OutputTreatment {
    let shell = (interpreter == Interpreter::Shell).then(|| ShellScript::parse(script));
    let start = ScriptDir::Host(PathBuf::from("/"));
    let check = ScriptCheck {
        task_id: "task-a",
        script,
        interpreter,
        shell: shell.as_ref().map(Result::as_ref),
        origin: ScriptOrigin::Foreground,
        sandbox: SandboxMode::Sandboxed,
        location: None,
        start: &start,
        call: None,
    };
    FileReadsAreVerbatim.treatment(&check)
}

#[test]
fn a_script_of_file_reads_is_verbatim() {
    for script in [
        "cat notes.md",
        "sed -n '1,20p' notes.md",
        "head -n 5 notes.md",
        "cat a.md; cat b.md",
        "cat a.md && sed -n '2,3p' b.md",
    ] {
        assert_eq!(
            treatment_of(script, Interpreter::Shell),
            OutputTreatment::Verbatim,
            "{script}"
        );
    }
}

#[test]
fn any_other_script_is_command_output() {
    for script in [
        "cat notes.md; echo done",
        "echo hi",
        "cat notes.md | wc -l",
        "rg needle .",
        "cat \"$FILE\"",
        "if [ ; then",
        "",
    ] {
        assert_eq!(
            treatment_of(script, Interpreter::Shell),
            OutputTreatment::Command,
            "{script:?}"
        );
    }
    assert_eq!(
        treatment_of("cat notes.md", Interpreter::Python),
        OutputTreatment::Command,
        "a script in another language"
    );
}

/// A file whose blank-line runs command-output cleaning collapses.
const NOTES: &str = "# Notes\n\n\n\nfirst\n\n\nsecond\n";

async fn run(terminal: &TestTerminal, dir: &std::path::Path, script: &str) -> ScriptResult {
    terminal
        .shell()
        .run(Script::new(script, dir, Duration::from_secs(60)))
        .await
        .expect("the script runs")
}

fn stdout_of(result: &ScriptResult) -> &str {
    match &result.stdout {
        CommandStream::Inline { text } => text,
        other => panic!("a short answer is inline: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn file_reads_keep_their_blank_lines_and_other_scripts_collapse_them() {
    let session = tempfile::tempdir().expect("session dir");
    let root = session.path().canonicalize().expect("session root");
    std::fs::write(root.join("notes.md"), NOTES).expect("notes");
    let terminal = TestTerminal::new(&root).await;

    let cat = run(&terminal, &root, "cat notes.md").await;
    assert_eq!(stdout_of(&cat), NOTES);

    let sed = run(&terminal, &root, "sed -n '1,20p' notes.md").await;
    assert_eq!(stdout_of(&sed), NOTES);

    let mixed = run(&terminal, &root, "cat notes.md; echo done").await;
    assert!(
        stdout_of(&mixed).contains("blank lines collapsed"),
        "{mixed:?}"
    );
}
