use std::path::PathBuf;
use std::time::Duration;

use aither_core::llm::CommandStream;
use aither_sandbox::{
    Interpreter, SandboxMode, Script, ScriptDir, ScriptOrigin, ScriptResult, ShellScript,
};

use super::*;
use crate::test_sandbox::TestTerminal;

fn treatment_with(
    policy: &FileReadsAreVerbatim,
    script: &str,
    interpreter: Interpreter,
) -> OutputTreatment {
    let shell = (interpreter == Interpreter::Shell).then(|| ShellScript::parse(script));
    let start = ScriptDir::Host(PathBuf::from("/"));
    let check = ScriptCheck {
        launch: aither_sandbox::LaunchLease::fresh().id(),
        script,
        interpreter,
        shell: shell.as_ref().map(Result::as_ref),
        origin: ScriptOrigin::Foreground,
        sandbox: SandboxMode::Sandboxed,
        location: None,
        start: &start,
        call: None,
    };
    policy.treatment(&check)
}

fn treatment_of(script: &str, interpreter: Interpreter) -> OutputTreatment {
    treatment_with(&FileReadsAreVerbatim::default(), script, interpreter)
}

#[test]
fn targeted_reads_saved_outputs_and_searches_are_verbatim() {
    let policy = FileReadsAreVerbatim::with_outputs_dir("/session/outputs");
    for script in [
        "sed -n '1,20p' notes.md",
        "sed -n '10,20p;30,40p' notes.md",
        "sed -n -e '1,5p' -e '10,15p' notes.md",
        "head -n 5 notes.md",
        "tail -n 20 notes.md",
        "nl -ba big.rs | sed -n '100,300p'",
        "cat -n big.rs | sed -n '100,300p'",
        "cat big.rs | head -n 200",
        "cat big.rs | tail -n 200",
        "cat outputs/alfa.txt",
        "cat ./outputs/alfa.txt",
        "nl -ba outputs/alfa.txt",
        "cat /session/outputs/alfa.txt",
        "rg needle .",
        "rg -n --multiline 'foo > bar' src/",
        "rg needle . | head -n 50",
        "grep -rn needle src/",
        "ast-grep -p 'fn $NAME($$$)' src/",
        "sg -p 'struct $S { $$$ }' src/",
        "fd '*.rs' src/",
        "find src -name '*.rs'",
        "sed -n '1,20p' a.md && head -n 10 b.md",
        "rg foo src/ && rg bar tests/",
    ] {
        assert_eq!(
            treatment_with(&policy, script, Interpreter::Shell),
            OutputTreatment::Verbatim,
            "{script}"
        );
    }
}

#[test]
fn whole_file_reads_are_verbatim_foldable() {
    let policy = FileReadsAreVerbatim::with_outputs_dir("/session/outputs");
    for script in [
        "cat notes.md",
        "cat -n notes.md",
        "cat -b notes.md",
        "nl notes.md",
        "nl -ba notes.md",
        "nl -ba notes.md 2>/dev/null",
        "cat src/outputs/notes.md",
        "cat notes.md | nl -ba",
        "cat a.md; cat b.md",
        "cat a.md && sed -n '2,3p' b.md",
    ] {
        assert_eq!(
            treatment_with(&policy, script, Interpreter::Shell),
            OutputTreatment::VerbatimFoldable,
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
        "cat \"$FILE\"",
        "nl -ba \"$FILE\"",
        "cat notes.md > copy.md",
        "nl -ba notes.md > copy.md",
        "sed -i 's/a/b/' notes.md",
        "sed 's/a/b/' notes.md",
        "ast-grep -p 'foo' -r 'bar' -U src/",
        "sg -p 'foo' -r 'bar' -i src/",
        "find src -name '*.rs' -delete",
        "find src -name '*.rs' -exec echo {} +",
        "git log | head -n 20",
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

/// Real Rust source file in this crate, large enough (> 200 lines) to overflow
/// the default inline limit and exercise structural code folding.
const LARGE_RUST_SOURCE: &str = include_str!("../output_treatment.rs");

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

#[tokio::test(flavor = "multi_thread")]
async fn oversized_whole_file_cat_folds_while_targeted_reads_and_outputs_do_not() {
    let session = tempfile::tempdir().expect("session dir");
    let root = session.path().canonicalize().expect("session root");
    let big = LARGE_RUST_SOURCE;
    std::fs::write(root.join("big.rs"), big).expect("big.rs");
    std::fs::create_dir_all(root.join("outputs")).expect("outputs dir");
    std::fs::write(root.join("outputs/saved.rs"), big).expect("saved.rs");
    let terminal = TestTerminal::new(&root).await;

    let whole = run(&terminal, &root, "cat big.rs").await;
    let CommandStream::Stored { preview, .. } = &whole.stdout else {
        panic!("oversized cat should store and fold: {:?}", whole.stdout);
    };
    assert!(
        preview.contains("Code folded"),
        "whole-file cat should fold oversized source: {preview}"
    );
    assert!(
        preview.contains("sed -n") && preview.contains("raw: true"),
        "folded notice should guide targeted reads and raw bypass: {preview}"
    );

    let targeted = run(&terminal, &root, "sed -n '1,400p' big.rs").await;
    let CommandStream::Stored { preview, .. } = &targeted.stdout else {
        panic!(
            "oversized sed should store without folding: {:?}",
            targeted.stdout
        );
    };
    assert!(
        !preview.contains("Code folded"),
        "targeted sed -n read must never fold: {preview}"
    );

    let saved = run(&terminal, &root, "cat outputs/saved.rs").await;
    let CommandStream::Stored { preview, .. } = &saved.stdout else {
        panic!(
            "oversized saved-output read should store without folding: {:?}",
            saved.stdout
        );
    };
    assert!(
        !preview.contains("Code folded"),
        "reading a saved file under outputs/ must never fold: {preview}"
    );
}
