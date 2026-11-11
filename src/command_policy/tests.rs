use aither_sandbox::{Carry, Interpreter, SandboxMode, ScriptDir, ShellParseError};

use super::*;

fn check_with<'a>(
    task_id: &'a str,
    script: &'a str,
    shell: Option<Result<&'a ShellScript, &'a ShellParseError>>,
    origin: ScriptOrigin,
    start: &'a ScriptDir,
) -> ScriptCheck<'a> {
    ScriptCheck {
        task_id,
        script,
        interpreter: Interpreter::Shell,
        shell,
        origin,
        sandbox: SandboxMode::Sandboxed,
        location: None,
        start,
        call: None,
    }
}

fn unchanged_reads() -> UnchangedFileReads {
    UnchangedFileReads {
        refused: RefusedReads::default(),
    }
}

async fn decide<P: ScriptPolicy>(
    policy: &P,
    state: &P::State,
    script: &str,
    origin: ScriptOrigin,
) -> ScriptDecision {
    let shell = ShellScript::parse(script);
    let start = ScriptDir::Host(PathBuf::from("/"));
    let check = check_with("task-a", script, Some(shell.as_ref()), origin, &start);
    policy.check(&check, state).await
}

fn refusal(decision: &ScriptDecision) -> Option<&str> {
    match decision {
        ScriptDecision::Pass => None,
        ScriptDecision::Refuse(refusal) => Some(refusal.text()),
        other => panic!("a May policy decides only Pass or Refuse, not {other:?}"),
    }
}

fn ended<'a>(task_id: &'a str, code: i32, end: &'a ScriptDir) -> ScriptEnd<'a> {
    ScriptEnd {
        task_id,
        outcome: ScriptOutcome::Exited { code },
        end,
        carry: Carry::Unchanged,
    }
}

#[tokio::test]
async fn kills_by_name_are_refused_and_kills_by_pid_run() {
    for script in [
        "pkill foo",
        "sudo env nice timeout 3 /usr/bin/pkill foo",
        "sh -c 'killall foo'",
        "fuser -k 8080/tcp",
        "taskkill /IM foo.exe /F",
        "kill $(pgrep foo)",
        "pgrep -f foo | xargs -r kill -9",
    ] {
        let decision = decide(&KillByName, &(), script, ScriptOrigin::Foreground).await;
        let text = refusal(&decision).unwrap_or_else(|| panic!("`{script}` is refused"));
        assert!(
            text.contains("signals processes by name or pattern"),
            "{text}"
        );
    }
    for script in [
        "kill 123",
        "sleep 30 & kill $!",
        "echo pkill",
        "pgrep foo | wc -l",
    ] {
        let decision = decide(&KillByName, &(), script, ScriptOrigin::Run).await;
        assert_eq!(decision, ScriptDecision::Pass, "`{script}` runs");
    }
}

#[tokio::test]
async fn a_shell_script_that_does_not_parse_is_refused_and_python_is_not_checked() {
    let decision = decide(
        &KillByName,
        &(),
        "echo 'unterminated",
        ScriptOrigin::Foreground,
    )
    .await;
    assert!(
        refusal(&decision).is_some_and(|text| text.contains("is not valid shell")),
        "{decision:?}"
    );
    let start = ScriptDir::Host(PathBuf::from("/"));
    let mut python = check_with(
        "task-a",
        "import os",
        None,
        ScriptOrigin::Foreground,
        &start,
    );
    python.interpreter = Interpreter::Python;
    assert_eq!(KillByName.check(&python, &()).await, ScriptDecision::Pass);
}

#[tokio::test]
async fn only_a_foreground_sleep_is_refused() {
    let decision = decide(
        &ForegroundSleep,
        &(),
        "sleep 60; tail -50 out.txt",
        ScriptOrigin::Foreground,
    )
    .await;
    assert!(
        refusal(&decision).is_some_and(|text| text.contains("only serves polling")),
        "{decision:?}"
    );
    for (script, origin) in [
        ("sleep 60", ScriptOrigin::Background),
        ("sleep 60", ScriptOrigin::Run),
        (
            "sleep 30 & echo $! > child.pid; wait",
            ScriptOrigin::Foreground,
        ),
        ("echo sleep", ScriptOrigin::Foreground),
    ] {
        assert_eq!(
            decide(&ForegroundSleep, &(), script, origin).await,
            ScriptDecision::Pass,
            "`{script}` runs"
        );
    }
}

#[tokio::test]
async fn a_reread_of_an_unchanged_file_is_refused_once_a_read_succeeded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("log.txt");
    std::fs::write(&log, "v1").expect("write");
    let state = FileReads::default();
    let policy = unchanged_reads();
    let shell = ShellScript::parse("tail -50 log.txt").expect("parses");
    let start = ScriptDir::Host(dir.path().to_path_buf());
    let call = CallIdentity::new(
        aither_core::llm::tool::ToolCallId::new("call-c"),
        aither_core::llm::tool::AgentIdentity::Main,
    );
    let check = |task_id| {
        check_with(
            task_id,
            "tail -50 log.txt",
            Some(Ok(&shell)),
            ScriptOrigin::Foreground,
            &start,
        )
    };

    assert_eq!(
        policy.check(&check("task-a"), &state).await,
        ScriptDecision::Pass
    );
    policy.ended(&ended("task-a", 1, &start), &state).await;
    assert_eq!(
        policy.check(&check("task-b"), &state).await,
        ScriptDecision::Pass,
        "a failed read records nothing"
    );
    policy.ended(&ended("task-b", 0, &start), &state).await;

    let mut repeated = check("task-c");
    repeated.call = Some(&call);
    let ScriptDecision::Refuse(refused) = policy.check(&repeated, &state).await else {
        panic!("the unchanged re-read is refused");
    };
    assert!(
        policy.refused.take(&call),
        "the refused call is recorded for the turn boundary"
    );
    assert!(!policy.refused.take(&call), "and answered once");
    assert!(
        refused.text().contains(&format!(
            "repeats your last read of `{}`, unchanged since",
            log.display()
        )),
        "{}",
        refused.text()
    );

    std::fs::write(&log, "v1 and more").expect("rewrite");
    assert_eq!(
        policy.check(&check("task-d"), &state).await,
        ScriptDecision::Pass,
        "a changed file reads again"
    );
}

#[tokio::test]
async fn a_follow_or_a_missing_file_is_no_reread() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("log.txt"), "v1").expect("write");
    let state = FileReads::default();
    let start = ScriptDir::Host(dir.path().to_path_buf());
    for script in ["tail -f log.txt", "cat missing.txt"] {
        let shell = ShellScript::parse(script).expect("parses");
        let check = check_with(
            "task-a",
            script,
            Some(Ok(&shell)),
            ScriptOrigin::Foreground,
            &start,
        );
        assert_eq!(
            unchanged_reads().check(&check, &state).await,
            ScriptDecision::Pass
        );
        assert!(
            state.inner().pending.is_empty(),
            "`{script}` tracks no read"
        );
    }
}

/// Puts `script`, started in `dir`, to the unchanged-read policy and, when it
/// passes, ends it with exit code 0, as a read that succeeded.
async fn read_in(
    policy: &UnchangedFileReads,
    state: &FileReads,
    dir: &Path,
    script: &str,
) -> ScriptDecision {
    let shell = ShellScript::parse(script);
    let start = ScriptDir::Host(dir.to_path_buf());
    let check = check_with(
        script,
        script,
        Some(shell.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    let decision = policy.check(&check, state).await;
    policy.ended(&ended(script, 0, &start), state).await;
    decision
}

#[tokio::test]
async fn a_script_that_does_more_than_repeat_a_read_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("out.json"), "{}").expect("write");
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").expect("write");
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("write");
    std::fs::create_dir(dir.path().join("tasks")).expect("mkdir");
    std::fs::write(dir.path().join("tasks/Cargo.toml"), "[package]\n").expect("write");
    let state = FileReads::default();
    let policy = unchanged_reads();
    for (before, after) in [
        // The script rewrites the file it reads.
        ("cat out.json", "./gen.sh && cat out.json"),
        // Another part of the same file.
        ("head -n 50 a.rs", "head -n 200 a.rs"),
        ("cat a.rs | rg x", "cat a.rs | rg y"),
        ("cat a.rs", "cat -n a.rs"),
        // A file never read: `tasks/Cargo.toml`.
        ("cat Cargo.toml", "cd tasks && cat Cargo.toml"),
        // Output written or run rather than shown.
        ("cat a.rs", "cat a.rs > copy.rs"),
        ("cat a.rs", "cat a.rs | sh"),
        ("cat a.rs", "sudo cat a.rs"),
    ] {
        assert_eq!(
            read_in(&policy, &state, dir.path(), before).await,
            ScriptDecision::Pass,
            "`{before}` is a first read"
        );
        assert_eq!(
            read_in(&policy, &state, dir.path(), after).await,
            ScriptDecision::Pass,
            "`{after}` after `{before}` runs"
        );
    }
    // A script that does more than read is never a repeated read.
    for script in [
        "./gen.sh && cat out.json",
        "cd tasks && cat Cargo.toml",
        "cat a.rs > copy.rs",
        "cat a.rs | sh",
        "sudo cat a.rs",
    ] {
        assert_eq!(
            read_in(&policy, &state, dir.path(), script).await,
            ScriptDecision::Pass,
            "`{script}` runs again"
        );
    }
}

#[tokio::test]
async fn repeating_the_same_read_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("out.json"), "{}").expect("write");
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").expect("write");
    let state = FileReads::default();
    let policy = unchanged_reads();
    for script in [
        "cat out.json",
        "head -n 50 a.rs",
        "cat a.rs | rg x",
        "sed -n '1,5p' a.rs",
        "cat out.json a.rs 2>&1",
    ] {
        assert_eq!(
            read_in(&policy, &state, dir.path(), script).await,
            ScriptDecision::Pass,
            "`{script}` reads"
        );
        let decision = read_in(&policy, &state, dir.path(), script).await;
        let text = refusal(&decision).unwrap_or_else(|| panic!("`{script}` again is refused"));
        assert!(text.contains("repeats your last read of"), "{text}");
    }
}

#[tokio::test]
async fn the_scripts_own_shell_runs_no_host_grep_awk_or_find() {
    for (script, name) in [
        ("grep x file", "grep"),
        ("cat file | awk '{ print $1 }'", "awk"),
        ("echo $(find . -name x)", "find"),
        ("time grep x file", "grep"),
        ("eval 'grep x file'", "grep"),
        ("'grep' x file", "grep"),
    ] {
        let decision = decide(&ManagedShellCommands, &(), script, ScriptOrigin::Foreground).await;
        let text = refusal(&decision).unwrap_or_else(|| panic!("`{script}` is refused"));
        assert!(
            text.contains(&format!("`{name}` is not available in scripts")),
            "{text}"
        );
    }
    for script in [
        "rg x file",
        "/usr/bin/grep x file",
        "env grep x file",
        "command grep x file",
        "printf 'a\\n' | xargs grep a",
        "sh -c 'grep x file'",
        "echo grep",
    ] {
        assert_eq!(
            decide(&ManagedShellCommands, &(), script, ScriptOrigin::Foreground).await,
            ScriptDecision::Pass,
            "`{script}` runs the host's program"
        );
    }
}

#[tokio::test]
async fn sed_runs_only_in_the_forms_the_managed_sed_prints() {
    for script in [
        "sed -n '2,3p' file",
        "sed -n '$p' file",
        "rg -n x | sed -n '1,5p'",
    ] {
        assert_eq!(
            decide(&ManagedShellCommands, &(), script, ScriptOrigin::Foreground).await,
            ScriptDecision::Pass,
            "`{script}` prints a line range"
        );
    }
    for script in ["sed 's/a/b/' file", "sed -i -n '2p' file"] {
        let decision = decide(&ManagedShellCommands, &(), script, ScriptOrigin::Foreground).await;
        let text = refusal(&decision).unwrap_or_else(|| panic!("`{script}` is refused"));
        assert!(
            text.starts_with("Refused before running: `sed` in scripts only prints line ranges"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn pip_installs_and_virtual_environments_are_refused_with_the_uv_command() {
    for (script, command, replacement) in [
        ("pip install requests", "pip install", "`uv add <package>`"),
        (
            "pip3 install -r requirements.txt",
            "pip3 install",
            "`uv pip install <package>`",
        ),
        ("pip --quiet install x", "pip install", "`uv add <package>`"),
        (
            "/usr/bin/pip3.13 install x",
            "pip3.13 install",
            "`uv add <package>`",
        ),
        (
            "python -m pip install x",
            "python -m pip install",
            "`uv add <package>`",
        ),
        (
            "python3 -m pip install --user x",
            "python3 -m pip install",
            "`uv pip install <package>`",
        ),
        (
            "~/.venv/bin/python3.12 -m pip install x",
            "python3.12 -m pip install",
            "`uv add <package>`",
        ),
        (
            "cd app && sudo pip install x",
            "pip install",
            "`uv add <package>`",
        ),
        ("python -m venv .venv", "python -m venv", "`uv venv`"),
        ("python3 -m venv .venv", "python3 -m venv", "`uv venv`"),
        ("/opt/bin/virtualenv .venv", "virtualenv", "`uv venv`"),
    ] {
        let decision = decide(&PythonWithUv, &(), script, ScriptOrigin::Foreground).await;
        let text = refusal(&decision).unwrap_or_else(|| panic!("`{script}` is refused"));
        assert!(
            text.contains(&format!("`{command}` is not used")),
            "{script}: {text}"
        );
        assert!(text.contains(replacement), "{script}: {text}");
    }
}

#[tokio::test]
async fn the_refusal_text_is_exact() {
    let decision = decide(
        &PythonWithUv,
        &(),
        "pip install x",
        ScriptOrigin::Foreground,
    )
    .await;
    assert_eq!(
        refusal(&decision),
        Some(
            "Refused before running: `pip install` is not used in scripts; Python dependencies, virtual environments and interpreters are managed with uv. Use `uv add <package>` in a project or `uv pip install <package>` in an environment."
        )
    );
}

#[tokio::test]
async fn uv_and_read_only_pip_forms_run() {
    for script in [
        "uv add requests",
        "uv pip install requests",
        "uv venv && uv run pytest",
        "uvx ruff check .",
        "uv python install 3.12",
        "pip list",
        "pip3 show requests",
        "pip freeze > req.txt",
        "python -m pip list",
        "python3 -m pip --version",
        "python3 script.py install",
        "python -m pytest",
        "echo pip install",
    ] {
        assert_eq!(
            decide(&PythonWithUv, &(), script, ScriptOrigin::Foreground).await,
            ScriptDecision::Pass,
            "`{script}` runs"
        );
    }
}

fn patch_script(body: &str) -> String {
    format!("apply_patch <<'PATCH'\n*** Begin Patch\n{body}\n*** End Patch\nPATCH")
}

/// The whitespace policy's decision on a patch of `body`, run in a directory
/// holding `files`.
async fn patch_in(syntaxes: &Syntaxes, files: &[(&str, &str)], body: &str) -> ScriptDecision {
    let dir = tempfile::tempdir().expect("tempdir");
    for (path, text) in files {
        std::fs::write(dir.path().join(path), text).expect("write");
    }
    let script = patch_script(body);
    let shell = ShellScript::parse(&script);
    let start = ScriptDir::Host(dir.path().to_path_buf());
    let check = check_with(
        "task-a",
        &script,
        Some(shell.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    WhitespacePatch.check(&check, syntaxes).await
}

/// The whitespace policy's decision on `hunks` of `file`, whose text is
/// `text`.
async fn hunks_in(syntaxes: &Syntaxes, file: &str, text: &str, hunks: &str) -> ScriptDecision {
    patch_in(
        syntaxes,
        &[(file, text)],
        &format!("*** Update File: {file}\n{hunks}"),
    )
    .await
}

#[tokio::test]
async fn a_whitespace_only_hunk_is_refused() {
    let syntaxes = Syntaxes::default();
    for (file, text, hunks) in [
        (
            "a.rs",
            "fn  main() {}\n",
            "@@\n-fn  main() {}\n+fn main() {}",
        ),
        (
            "a.rs",
            "ctx\n    let x = 1;\n",
            "@@\n ctx\n-    let x = 1;\n+        let x = 1;",
        ),
        (
            "a.rs",
            "let x = f(a, b);\n",
            "@@\n-let x = f(a, b);\n+let x = f(\n+    a,\n+    b\n+);",
        ),
        ("a.rs", "x\nmid\n  z\n", "@@\n-x\n+y\n@@\n-  z\n+z"),
        (
            "a.rs",
            "let s = \"a b\";  // a  comment\n",
            "@@\n-let s = \"a b\";  // a  comment\n+let s = \"a b\"; // a comment",
        ),
        (
            "a.rs",
            "return a\n    + b;\n",
            "@@\n-return a\n-    + b;\n+return a + b;",
        ),
        (
            "a.rs",
            "let x = y as\n    u32;\n",
            "@@\n-let x = y as\n-    u32;\n+let x = y as u32;",
        ),
        ("a.py", "x  = f(a,b)\n", "@@\n-x  = f(a,b)\n+x = f(a, b)"),
        (
            "a.py",
            "def f():\n    return  1\n",
            "@@\n def f():\n-    return  1\n+    return 1",
        ),
        (
            "main.go",
            "func  main() {}\n",
            "@@\n-func  main() {}\n+func main() {}",
        ),
        ("a.json", "{\"a\":1}\n", "@@\n-{\"a\":1}\n+{ \"a\": 1 }"),
        // Code below a multi-line string that closed above the hunk.
        (
            "a.rs",
            "let s = r#\"\nfirst\n\"#;\na b\n",
            "@@\n-a b\n+a  b",
        ),
        ("a.js", "const s = `\nfirst\n`;\na b\n", "@@\n-a b\n+a  b"),
    ] {
        let decision = hunks_in(&syntaxes, file, text, hunks).await;
        let refused =
            refusal(&decision).unwrap_or_else(|| panic!("`{hunks}` of {file} is refused"));
        assert!(refused.contains("Do not format by hand"), "{refused}");
        assert!(refused.contains(&format!("`{file}`")), "{refused}");
    }
    let decision = patch_in(
        &syntaxes,
        &[("a.rs", "fn a() {}\n"), ("b.rs", "x\n")],
        "*** Update File: a.rs\n@@\n-fn a() {}\n+fn a() {}\n*** Update File: b.rs\n@@\n-x\n+y",
    )
    .await;
    assert!(
        refusal(&decision).is_some_and(|text| text.contains("`a.rs`")),
        "a patch with one whitespace-only hunk is refused: {decision:?}"
    );
}

#[tokio::test]
async fn a_patch_that_changes_more_than_whitespace_runs() {
    let syntaxes = Syntaxes::default();
    for body in [
        "*** Update File: a.rs\n@@\n-let x = 1;\n+let x = 2;",
        "*** Add File: a.rs\n+  x\n+y",
        "*** Update File: a.rs\n@@\n-x\n*** Delete File: b.rs",
        "*** Update File: a.rs\n@@\n+  inserted",
    ] {
        assert_eq!(
            patch_in(&syntaxes, &[("a.rs", "x\n"), ("b.rs", "y\n")], body).await,
            ScriptDecision::Pass,
            "`{body}` runs"
        );
    }
}

#[tokio::test]
async fn whitespace_that_carries_meaning_is_not_formatting() {
    let syntaxes = Syntaxes::default();
    for (file, text, hunks) in [
        // Re-indenting Python under a new `if`.
        (
            "a.py",
            "def f():\n    x = 1\n",
            "@@\n def f():\n+    if c:\n@@\n-    x = 1\n+        x = 1",
        ),
        (
            "a.py",
            "for x in xs:\n    a(x)\n    b(x)\n",
            "@@\n for x in xs:\n     a(x)\n-    b(x)\n+b(x)",
        ),
        // Re-nesting YAML.
        ("ci.yml", "jobs:\ntest:\n", "@@\n jobs:\n-test:\n+  test:"),
        (
            "Makefile",
            "all:\n  cargo build\n",
            "@@\n all:\n-  cargo build\n+\tcargo build",
        ),
        // Whitespace inside a string literal.
        (
            "a.rs",
            "let s = \"a b\";\n",
            "@@\n-let s = \"a b\";\n+let s = \"a  b\";",
        ),
        ("a.py", "x = 'a b'\n", "@@\n-x = 'a b'\n+x = 'a  b'"),
        (
            "a.js",
            "const s = `a\nb`;\n",
            "@@\n-const s = `a\n-b`;\n+const s = `a\n+  b`;",
        ),
        // Inside a multi-line string that opens above the hunk.
        (
            "a.rs",
            "let s = r#\"\nfirst\na b\n\"#;\n",
            "@@\n-a b\n+a  b",
        ),
        ("a.js", "const s = `\nfirst\na b\n`;\n", "@@\n-a b\n+a  b"),
        ("main.go", "var s = `\nfirst\na b\n`\n", "@@\n-a b\n+a  b"),
        // Words joined into one.
        ("a.rs", "return x;\n", "@@\n-return x;\n+returnx;"),
        // A language May cannot lex.
        ("a.ts", "let  x = 1;\n", "@@\n-let  x = 1;\n+let x = 1;"),
        // A hunk that does not place fails on its own.
        (
            "a.rs",
            "fn other() {}\n",
            "@@\n-fn  main() {}\n+fn main() {}",
        ),
    ] {
        assert_eq!(
            hunks_in(&syntaxes, file, text, hunks).await,
            ScriptDecision::Pass,
            "`{hunks}` of {file} runs"
        );
    }
    assert_eq!(
        patch_in(
            &syntaxes,
            &[],
            "*** Update File: a.rs\n@@\n-fn  main() {}\n+fn main() {}"
        )
        .await,
        ScriptDecision::Pass,
        "a file that cannot be read is not judged"
    );
}
