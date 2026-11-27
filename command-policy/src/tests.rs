use aither_sandbox::{Carry, Interpreter, SandboxMode, ScriptDir, ShellParseError};

use super::*;

fn check_with<'a>(
    launch: aither_sandbox::LaunchId,
    script: &'a str,
    shell: Option<Result<&'a ShellScript, &'a ShellParseError>>,
    origin: ScriptOrigin,
    start: &'a ScriptDir,
) -> ScriptCheck<'a> {
    ScriptCheck {
        launch,
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
        read_epochs: FileReadEpochs::default(),
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
    let launch = aither_sandbox::LaunchLease::fresh().id();
    let check = check_with(launch, script, Some(shell.as_ref()), origin, &start);
    policy.check(&check, state).await
}

fn refusal(decision: &ScriptDecision) -> Option<&str> {
    match decision {
        ScriptDecision::Pass => None,
        ScriptDecision::Refuse(refusal) => Some(refusal.text()),
        other => panic!("a May policy decides only Pass or Refuse, not {other:?}"),
    }
}

fn ended(launch: aither_sandbox::LaunchId, code: i32, end: &ScriptDir) -> ScriptEnd<'_> {
    ScriptEnd {
        launch,
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
        let decision = decide(&KillByName::new(), &(), script, ScriptOrigin::Foreground).await;
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
        let decision = decide(&KillByName::new(), &(), script, ScriptOrigin::Run).await;
        assert_eq!(decision, ScriptDecision::Pass, "`{script}` runs");
    }
}

#[tokio::test]
async fn a_shell_script_that_does_not_parse_is_refused_and_python_is_not_checked() {
    let decision = decide(
        &KillByName::new(),
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
        aither_sandbox::LaunchLease::fresh().id(),
        "import os",
        None,
        ScriptOrigin::Foreground,
        &start,
    );
    python.interpreter = Interpreter::Python;
    assert_eq!(
        KillByName::new().check(&python, &()).await,
        ScriptDecision::Pass
    );
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
    let check = |launch| {
        check_with(
            launch,
            "tail -50 log.txt",
            Some(Ok(&shell)),
            ScriptOrigin::Foreground,
            &start,
        )
    };
    let (launch_a, launch_b, launch_c, launch_d) = (
        aither_sandbox::LaunchLease::fresh().id(),
        aither_sandbox::LaunchLease::fresh().id(),
        aither_sandbox::LaunchLease::fresh().id(),
        aither_sandbox::LaunchLease::fresh().id(),
    );

    assert_eq!(
        policy.check(&check(launch_a), &state).await,
        ScriptDecision::Pass
    );
    policy.ended(&ended(launch_a, 1, &start), &state).await;
    assert_eq!(
        policy.check(&check(launch_b), &state).await,
        ScriptDecision::Pass,
        "a failed read records nothing"
    );
    policy.ended(&ended(launch_b, 0, &start), &state).await;

    let mut repeated = check(launch_c);
    repeated.call = Some(&call);
    let ScriptDecision::Refuse(refused) = policy.check(&repeated, &state).await else {
        panic!("the unchanged re-read is refused");
    };
    assert!(
        policy.refused.take(call.call()),
        "the refused call is recorded for the turn boundary"
    );
    assert!(!policy.refused.take(call.call()), "and answered once");
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
        policy.check(&check(launch_d), &state).await,
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
            aither_sandbox::LaunchLease::fresh().id(),
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
    let launch = aither_sandbox::LaunchLease::fresh().id();
    let check = check_with(
        launch,
        script,
        Some(shell.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    let decision = policy.check(&check, state).await;
    policy.ended(&ended(launch, 0, &start), state).await;
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
        // Each pair starts with nothing read.
        let state = FileReads::default();
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
    let state = FileReads::default();
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
async fn the_scripts_own_shell_allows_host_grep_awk_or_find_and_records_notices() {
    for (script, name) in [
        ("grep x file", "grep"),
        ("cat file | awk '{ print $1 }'", "awk"),
        ("echo $(find . -name x)", "find"),
        ("time grep x file", "grep"),
        ("eval 'grep x file'", "grep"),
        ("'grep' x file", "grep"),
    ] {
        let notices = HostCommandNotices::default();
        let policy = ManagedShellCommands::new(notices.clone());
        let decision = decide(&policy, &(), script, ScriptOrigin::Foreground).await;
        assert_eq!(
            decision,
            ScriptDecision::Pass,
            "`{script}` runs instead of hard-failing"
        );
        let recorded = notices.take_all();
        assert_eq!(
            recorded.len(),
            1,
            "`{script}` records one host-command notice"
        );
        assert_eq!(recorded[0].name, name);
    }
    // In hashline mode, `grep` steers toward `search rg` so results carry `N#HH` anchors.
    let notices = HostCommandNotices::default();
    let policy = ManagedShellCommands::with_hashline(notices.clone(), true);
    assert_eq!(
        decide(&policy, &(), "grep x file", ScriptOrigin::Foreground).await,
        ScriptDecision::Pass
    );
    let recorded = notices.take_all();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].name, "grep");
    assert_eq!(recorded[0].replacement, "`search rg`");
    for script in [
        "rg x file",
        "/usr/bin/grep x file",
        "env grep x file",
        "command grep x file",
        "printf 'a\\n' | xargs grep a",
        "sh -c 'grep x file'",
        "echo grep",
    ] {
        let notices = HostCommandNotices::default();
        let policy = ManagedShellCommands::new(notices.clone());
        assert_eq!(
            decide(&policy, &(), script, ScriptOrigin::Foreground).await,
            ScriptDecision::Pass,
            "`{script}` runs without a notice"
        );
        assert!(
            notices.take_all().is_empty(),
            "`{script}` should not record a host-command notice"
        );
    }
}

#[tokio::test]
async fn the_session_find_subcommand_runs_in_the_managed_bash() {
    for script in [
        "session find canary",
        "session \"find canary\" --ignore-case --context 2",
        "session \"find deploy\" --count | rg alpha",
        "echo $(session \"find canary\" --session-id alpha)",
    ] {
        let notices = HostCommandNotices::default();
        let policy = ManagedShellCommands::new(notices.clone());
        let decision = decide(&policy, &(), script, ScriptOrigin::Foreground).await;
        assert_eq!(decision, ScriptDecision::Pass, "`{script}` runs");
        assert_eq!(notices.take_all(), [] as [HostCommandUsage; 0]);
    }
}

#[tokio::test]
async fn sed_in_every_form_is_left_to_the_managed_sed() {
    for script in [
        "sed -n '2,3p' file",
        "sed -n '/start/,/end/p' file",
        "sed -n '/^def foo/,/^def /p' file",
        "sed -n '/start/,+3p' file",
        "sed -E 's/(a)b/\\1/' file",
        "rg -n x | sed '/x/d'",
        "sed -i 's/a/b/' file",
    ] {
        let notices = HostCommandNotices::default();
        let policy = ManagedShellCommands::new(notices.clone());
        assert_eq!(
            decide(&policy, &(), script, ScriptOrigin::Foreground).await,
            ScriptDecision::Pass,
            "`{script}` reaches the managed sed, which refuses what writes"
        );
        assert_eq!(notices.take_all(), [] as [HostCommandUsage; 0]);
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
        aither_sandbox::LaunchLease::fresh().id(),
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

#[tokio::test]
async fn bumping_read_epoch_allows_post_compaction_recall_without_clearing_other_agents() {
    use aither_core::llm::tool::{AgentIdentity, SubagentIdentity};

    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("lib.rs");
    std::fs::write(&file, "pub fn answer() -> u32 { 42 }\n").expect("write");

    let epochs = FileReadEpochs::default();
    let policy = UnchangedFileReads {
        refused: RefusedReads::default(),
        read_epochs: epochs.clone(),
    };
    let main_state = FileReads::default();
    let child_state = FileReads::default();
    let start = ScriptDir::Host(dir.path().to_path_buf());
    let script = "cat lib.rs";
    let parsed_shell = ShellScript::parse(script);

    let main_call = CallIdentity::new(ToolCallId::new("c1"), AgentIdentity::Main);
    let child_call = CallIdentity::new(
        ToolCallId::new("c2"),
        AgentIdentity::Subagent(SubagentIdentity::new("explore", Some(main_call.clone()))),
    );
    let child_path = AgentPath::of(child_call.agent());

    // Initial reads succeed on both main and child, then identical repeats are refused.
    for (call, state) in [(&main_call, &main_state), (&child_call, &child_state)] {
        let launch = aither_sandbox::LaunchLease::fresh().id();
        let mut check = check_with(
            launch,
            script,
            Some(parsed_shell.as_ref()),
            ScriptOrigin::Foreground,
            &start,
        );
        check.call = Some(call);
        assert_eq!(policy.check(&check, state).await, ScriptDecision::Pass);
        policy.ended(&ended(launch, 0, &start), state).await;

        let mut repeat = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            script,
            Some(parsed_shell.as_ref()),
            ScriptOrigin::Foreground,
            &start,
        );
        repeat.call = Some(call);
        assert!(refusal(&policy.check(&repeat, state).await).is_some());
    }

    // Main compacts: main's post-compaction recall passes again while child's
    // unchanged-read guard stays intact.
    epochs.bump(&AgentPath::main());

    let mut child_repeat = check_with(
        aither_sandbox::LaunchLease::fresh().id(),
        script,
        Some(parsed_shell.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    child_repeat.call = Some(&child_call);
    assert!(
        refusal(&policy.check(&child_repeat, &child_state).await).is_some(),
        "child did not compact, so its unchanged read stays refused"
    );

    let main_recall = aither_sandbox::LaunchLease::fresh().id();
    let mut main_check = check_with(
        main_recall,
        script,
        Some(parsed_shell.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    main_check.call = Some(&main_call);
    assert_eq!(
        policy.check(&main_check, &main_state).await,
        ScriptDecision::Pass,
        "post-compaction recall read on main must pass"
    );
    policy
        .ended(&ended(main_recall, 0, &start), &main_state)
        .await;

    // Once recalled in the new epoch, an immediate duplicate read is refused again.
    let mut main_dup = check_with(
        aither_sandbox::LaunchLease::fresh().id(),
        script,
        Some(parsed_shell.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    main_dup.call = Some(&main_call);
    assert!(refusal(&policy.check(&main_dup, &main_state).await).is_some());

    // Bumping the child's path likewise re-enables the child's recall read.
    epochs.bump(&child_path);
    assert_eq!(
        policy.check(&child_repeat, &child_state).await,
        ScriptDecision::Pass
    );
}

#[tokio::test]
async fn hashline_edit_bypass_flags_bypasses_and_allows_legitimate_commands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("existing.py"), "x = 1\n").expect("write existing.py");
    std::fs::write(root.join("a.py"), "a = 1\n").expect("write a.py");
    std::fs::write(root.join("existing.rs"), "fn main() {}\n").expect("write existing.rs");
    std::fs::write(root.join("existing.json"), "{}\n").expect("write existing.json");
    std::fs::write(root.join("tasks.md"), "- task\n").expect("write tasks.md");
    std::fs::create_dir_all(root.join("tmp")).expect("mkdir tmp");
    std::fs::write(root.join("tmp/scratch.txt"), "old\n").expect("write tmp/scratch.txt");
    std::fs::write(root.join("tmp/x"), "replacement\n").expect("write tmp/x");
    std::fs::create_dir_all(root.join("repo/src")).expect("mkdir repo/src");
    std::fs::write(root.join("repo/src/lib.rs"), "pub fn a() {}\n").expect("write repo/src/lib.rs");

    let start = ScriptDir::Host(root.to_path_buf());
    let run_shell = |script: &str| {
        let notices = EditBypassNotices::with_sandbox_dir(root);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let parsed = ShellScript::parse(script);
        let start = start.clone();
        let script = script.to_string();
        async move {
            let check = check_with(
                aither_sandbox::LaunchLease::fresh().id(),
                &script,
                Some(parsed.as_ref()),
                ScriptOrigin::Foreground,
                &start,
            );
            let decision = policy.check(&check, &()).await;
            assert_eq!(
                decision,
                ScriptDecision::Pass,
                "`{script}` must pass while recording alerts"
            );
            notices.take_all()
        }
    };

    // Flagged bypasses.
    for (script, expected_mechanism) in [
        ("sed -i 's/1/2/' existing.py", "sed -i"),
        ("sed -i '' 's/1/2/' existing.py", "sed -i"),
        ("sed --in-place 's/1/2/' existing.py", "sed --in-place"),
        ("perl -i -pe 's/1/2/' existing.py", "perl -i"),
        ("perl -pi -e 's/1/2/' existing.py", "perl -i"),
        ("perl -pie 's/1/2/' existing.py", "perl -i"),
        ("ruby -i -pe 'gsub(/1/, \"2\")' existing.py", "ruby -i"),
        (
            "apply_patch <<'PATCH'\n*** Begin Patch\n*** End Patch\nPATCH",
            "apply_patch",
        ),
        ("ed existing.py", "ed"),
        ("ex -sc 'wq' existing.py", "ex"),
        ("patch -p1 < tmp/x", "patch"),
        ("jq . existing.json | sponge existing.json", "sponge"),
        (
            "python3 -c 'open(\"existing.py\", \"w\").write(\"x\")'",
            "python3 -c",
        ),
        (
            "python3 -c 'from pathlib import Path; Path(\"existing.py\").write_text(\"x\")'",
            "python3 -c",
        ),
        (
            "cd repo && python3 - <<'EOF'\nfrom pathlib import Path\np = Path(\"src/lib.rs\")\np.write_text(\"pub fn b() {}\")\nEOF",
            "python3",
        ),
        (
            "python3 <<'EOF'\nwith open('existing.py', 'w') as f:\n    f.write('y')\nEOF",
            "python3",
        ),
        (
            "node -e 'require(\"fs\").writeFileSync(\"existing.py\", \"x\")'",
            "node -e",
        ),
        ("bun -e 'await Bun.write(\"existing.py\", \"x\")'", "bun -e"),
        ("ruby -e 'File.write(\"existing.py\", \"x\")'", "ruby -e"),
        ("cat <<EOF > existing.py\nhi\nEOF", "shell redirection"),
        ("echo hi >> existing.py", "shell redirection"),
        ("echo hi >| existing.py", "shell redirection"),
        (
            "cd repo && cat <<'EOF' > src/lib.rs\npub fn c() {}\nEOF",
            "shell redirection",
        ),
        ("echo hi | tee existing.py", "tee"),
        ("echo hi | tee -a existing.py", "tee"),
        ("cp tmp/x existing.py", "cp"),
        ("mv tmp/x existing.py", "mv"),
    ] {
        let recorded = run_shell(script).await;
        assert!(
            recorded
                .iter()
                .any(|usage| usage.mechanism == expected_mechanism),
            "expected `{expected_mechanism}` alert for `{script}`, got: {recorded:?}"
        );
    }

    // Direct Interpreter::Python mutating an existing file is flagged, while read-only passes.
    {
        let notices = EditBypassNotices::with_sandbox_dir(root);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let mut py_write = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            "from pathlib import Path\nPath('existing.py').write_text('x')\n",
            None,
            ScriptOrigin::Foreground,
            &start,
        );
        py_write.interpreter = Interpreter::Python;
        assert_eq!(policy.check(&py_write, &()).await, ScriptDecision::Pass);
        let recorded = notices.take_all();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].mechanism, "python");
        assert_eq!(recorded[0].target.as_deref(), Some("existing.py"));

        let mut py_read = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            "print(open('existing.py').read())\n",
            None,
            ScriptOrigin::Foreground,
            &start,
        );
        py_read.interpreter = Interpreter::Python;
        assert_eq!(policy.check(&py_read, &()).await, ScriptDecision::Pass);
        assert_eq!(
            notices.take_all(),
            [] as [hashline_bypass::EditBypassUsage; 0]
        );
    }

    // Allowed commands (must NOT trigger any alert).
    for script in [
        "edit <<'PATCH'\n[existing.py]\nPUT >1#PM:\n+x > existing.py\nPATCH",
        "search rg x existing.py",
        "search rg -n x existing.py",
        "search sg 'fn $NAME()' --lang rust repo/src",
        "search sed -n '1,120p' existing.py",
        "cat <<EOF > brand_new.py\nhello\nEOF",
        "echo hi > /dev/null",
        "echo hi > tmp/scratch.txt",
        "echo hi > outputs/log.txt",
        "echo hi > files/notes.md",
        "echo hi > artifacts/report.md",
        "echo hi > data/sample.json",
        "echo hi > tasks.md",
        "echo hi > $MAY_TASKS",
        "echo hi > $TMPDIR/out.txt",
        "echo hi | tee brand_new.py",
        "echo hi | tee tmp/scratch.txt",
        "cp existing.py brand_new.py",
        "cp existing.py tmp/scratch.txt",
        "cp -n tmp/x existing.py",
        "python3 -c 'print(open(\"existing.py\").read())'",
        "python3 -c 'print(open(\"a.py\").read())'",
        "python3 -c 'print(open(\"existing.py\").read().replace(\"1\", \"2\") + \"\\n\")'",
        "python3 -c 'open(\"brand_new.py\", \"w\").write(\"x\")'",
        "python3 -c 'open(\"tmp/scratch.txt\", \"w\").write(\"x\")'",
        "cd repo && python3 - <<'EOF'\nfrom pathlib import Path\nPath(\"tmp/scratch.txt\").write_text(\"ok\")\nEOF",
        "sed -n '1,10p' existing.py",
        "sed -e '-i' existing.py",
        "rustfmt existing.rs",
        "prettier --write existing.json",
        "ruff format existing.py",
        "black existing.py",
        "git status",
        "cargo test 2>&1",
        "pytest",
    ] {
        let recorded = run_shell(script).await;
        assert!(
            recorded.is_empty(),
            "`{script}` must not trigger an edit-bypass alert, got: {recorded:?}"
        );
    }

    // Internal `ScriptOrigin::Run` scripts never record model alerts.
    {
        let notices = EditBypassNotices::with_sandbox_dir(root);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let parsed = ShellScript::parse("sed -i 's/1/2/' existing.py");
        let check = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            "sed -i 's/1/2/' existing.py",
            Some(parsed.as_ref()),
            ScriptOrigin::Run,
            &start,
        );
        assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
        assert_eq!(
            notices.take_all(),
            [] as [hashline_bypass::EditBypassUsage; 0]
        );
    }
}

#[tokio::test]
async fn hashline_edit_bypass_isolates_notices_by_agent_path() {
    use aither_core::llm::tool::{AgentIdentity, SubagentIdentity};

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("existing.py"), "x = 1\n").expect("write");
    let start = ScriptDir::Host(dir.path().to_path_buf());
    let notices = EditBypassNotices::with_sandbox_dir(dir.path());
    let policy = HashlineEditBypassPolicy::new(notices.clone());

    let main_call = CallIdentity::new(ToolCallId::new("c1"), AgentIdentity::Main);
    let sub_call = CallIdentity::new(
        ToolCallId::new("c2"),
        AgentIdentity::Subagent(SubagentIdentity::new("general", Some(main_call.clone()))),
    );
    let sub_path = AgentPath::of(sub_call.agent());

    let script = "sed -i 's/1/2/' existing.py";
    let parsed = ShellScript::parse(script);
    let mut sub_check = check_with(
        aither_sandbox::LaunchLease::fresh().id(),
        script,
        Some(parsed.as_ref()),
        ScriptOrigin::Foreground,
        &start,
    );
    sub_check.call = Some(&sub_call);
    assert_eq!(policy.check(&sub_check, &()).await, ScriptDecision::Pass);

    assert!(notices.take_all().is_empty(), "main queue stays empty");
    let sub_recorded = notices.take_all_for(&sub_path);
    assert_eq!(sub_recorded.len(), 1);
    assert_eq!(sub_recorded[0].mechanism, "sed -i");
    assert_eq!(
        notices.take_all_for(&sub_path),
        [] as [hashline_bypass::EditBypassUsage; 0]
    );
}

#[tokio::test]
async fn hashline_edit_bypass_still_flags_sandboxes_and_granted_repos_under_system_tmp() {
    let tmp_root = tempfile::tempdir_in("/tmp").expect("tempdir in /tmp");
    let sandbox = tmp_root.path().join("sandbox");
    let granted_repo = tmp_root.path().join("repo");
    let unrelated = tmp_root.path().join("unrelated.log");
    std::fs::create_dir_all(sandbox.join("tmp")).expect("mkdir sandbox/tmp");
    std::fs::create_dir_all(granted_repo.join("src")).expect("mkdir repo/src");
    std::fs::write(sandbox.join("existing.py"), "x = 1\n").expect("write sandbox/existing.py");
    std::fs::write(sandbox.join("tmp/scratch.txt"), "old\n").expect("write sandbox scratch");
    std::fs::write(granted_repo.join("src/app.py"), "y = 1\n").expect("write repo/src/app.py");
    std::fs::write(&unrelated, "old\n").expect("write unrelated /tmp file");

    let grants = aither_sandbox::Grants::new();
    grants.add_writable(&granted_repo);
    let notices = EditBypassNotices::with_sandbox_dir(&sandbox).with_grants(grants);
    let policy = HashlineEditBypassPolicy::new(notices.clone());
    let start = ScriptDir::Host(sandbox.clone());

    let run = |script: String| {
        let policy = policy.clone();
        let notices = notices.clone();
        let start = start.clone();
        async move {
            let parsed = ShellScript::parse(&script);
            let check = check_with(
                aither_sandbox::LaunchLease::fresh().id(),
                &script,
                Some(parsed.as_ref()),
                ScriptOrigin::Foreground,
                &start,
            );
            assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
            notices.take_all()
        }
    };

    // Relative and absolute writes to non-scratch files inside a `/tmp` sandbox must alert.
    for script in [
        "cat <<'EOF' > existing.py\nx = 2\nEOF".to_string(),
        format!(
            "cat <<'EOF' > {}\nx = 3\nEOF",
            sandbox.join("existing.py").display()
        ),
        format!(
            "cd {} && python3 - <<'EOF'\nfrom pathlib import Path\nPath('src/app.py').write_text('y = 2\\n')\nEOF",
            granted_repo.display()
        ),
    ] {
        let recorded = run(script.clone()).await;
        assert!(
            !recorded.is_empty(),
            "expected bypass alert for `{script}` under /tmp, got none"
        );
    }

    // Explicit sandbox scratch and unrelated `/tmp` files remain allowed.
    for script in [
        "echo ok > tmp/scratch.txt".to_string(),
        format!("echo ok > {}", sandbox.join("tmp/scratch.txt").display()),
        format!("echo ok > {}", unrelated.display()),
    ] {
        let recorded = run(script.clone()).await;
        assert!(
            recorded.is_empty(),
            "expected no alert for scratch path `{script}`, got: {recorded:?}"
        );
    }
}

#[tokio::test]
async fn hashline_edit_bypass_catches_cd_python_heredoc_and_edge_cases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sandbox = dir.path().join("sandbox");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&sandbox).expect("mkdir sandbox");
    std::fs::create_dir_all(repo.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(repo.join("src/_pytest")).expect("mkdir src/_pytest");
    std::fs::write(
        repo.join("src/_pytest/logging.py"),
        "def emit():\n    return 'a'\n",
    )
    .expect("write logging.py");

    let exact_script = format!(
        "cd {} && python3 - <<'EOF'\nimport re\np='src/_pytest/logging.py'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF",
        repo.display()
    );

    // 1. Standard host sandbox -> cd /path/to/repo && python3 - <<'EOF'
    {
        let notices = EditBypassNotices::with_sandbox_dir(&sandbox);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let start = ScriptDir::Host(sandbox.clone());
        let parsed = ShellScript::parse(&exact_script);
        let check = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            &exact_script,
            Some(parsed.as_ref()),
            ScriptOrigin::Foreground,
            &start,
        );
        assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
        let recorded = notices.take_all();
        assert_eq!(
            recorded,
            vec![EditBypassUsage::new(
                "python3",
                Some("src/_pytest/logging.py")
            )]
        );
    }

    // 2. Repo under /tmp with only a readable grant or unsandboxed `.git` checkout
    {
        let tmp_root = tempfile::tempdir_in("/tmp").expect("tempdir in /tmp");
        let tmp_repo = tmp_root.path().join("pytest-repo");
        std::fs::create_dir_all(tmp_repo.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(tmp_repo.join("src/_pytest")).expect("mkdir src/_pytest");
        std::fs::write(tmp_repo.join("src/_pytest/logging.py"), "x = 'a'\n").expect("write");

        let tmp_script = format!(
            "cd {} && python3 - <<'EOF'\nimport re\np='src/_pytest/logging.py'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF",
            tmp_repo.display()
        );
        let notices = EditBypassNotices::with_sandbox_dir(&sandbox);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let start = ScriptDir::Host(sandbox.clone());
        let parsed = ShellScript::parse(&tmp_script);
        let check = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            &tmp_script,
            Some(parsed.as_ref()),
            ScriptOrigin::Foreground,
            &start,
        );
        assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
        assert_eq!(
            notices.take_all(),
            vec![EditBypassUsage::new(
                "python3",
                Some("src/_pytest/logging.py")
            )]
        );
    }

    // 3. Container session: both `/workspace/...` mounted repo and `/testbed` unmounted repo
    {
        let container_repo = sandbox.join("repos/pytest");
        std::fs::create_dir_all(container_repo.join("src/_pytest")).expect("mkdir");
        std::fs::write(container_repo.join("src/_pytest/logging.py"), "x = 'a'\n").expect("write");

        let container_start = ScriptDir::Container {
            inside: aither_sandbox::ContainerPath::new("/workspace").expect("valid container path"),
            host: sandbox.clone(),
        };
        for script in [
            "cd /workspace/repos/pytest && python3 - <<'EOF'\nimport re\np='src/_pytest/logging.py'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF",
            "cd /testbed && python3 - <<'EOF'\nimport re\np='src/_pytest/logging.py'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF",
        ] {
            let notices = EditBypassNotices::with_sandbox_dir(&sandbox);
            let policy = HashlineEditBypassPolicy::new(notices.clone());
            let parsed = ShellScript::parse(script);
            let check = check_with(
                aither_sandbox::LaunchLease::fresh().id(),
                script,
                Some(parsed.as_ref()),
                ScriptOrigin::Foreground,
                &container_start,
            );
            assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
            assert_eq!(
                notices.take_all(),
                vec![EditBypassUsage::new(
                    "python3",
                    Some("src/_pytest/logging.py")
                )],
                "container script must alert: {script}"
            );
        }

        // Container `/tmp` scratch must stay silent.
        let notices = EditBypassNotices::with_sandbox_dir(&sandbox);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let scratch_script =
            "cd /tmp && python3 - <<'EOF'\nopen('scratch.txt','w').write('ok')\nEOF";
        let parsed = ShellScript::parse(scratch_script);
        let check = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            scratch_script,
            Some(parsed.as_ref()),
            ScriptOrigin::Foreground,
            &container_start,
        );
        assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
        assert_eq!(
            notices.take_all(),
            [] as [hashline_bypass::EditBypassUsage; 0]
        );
    }

    // 4. Semicolon-separated bindings, `io.open`, `-W ignore`, and read-only `str.replace`.
    {
        let start = ScriptDir::Host(repo.clone());
        for (script, expected_mech) in [
            (
                "python3 -c \"p='src/_pytest/logging.py'; s=open(p).read(); open(p,'w').write(s)\"",
                "python3 -c",
            ),
            (
                "python3 -c \"import io; io.open('src/_pytest/logging.py','w').write('x')\"",
                "python3 -c",
            ),
            (
                "python3 -W ignore - <<'EOF'\np='src/_pytest/logging.py'\nopen(p,'w').write('x')\nEOF",
                "python3",
            ),
        ] {
            let notices = EditBypassNotices::with_sandbox_dir(&sandbox);
            let policy = HashlineEditBypassPolicy::new(notices.clone());
            let parsed = ShellScript::parse(script);
            let check = check_with(
                aither_sandbox::LaunchLease::fresh().id(),
                script,
                Some(parsed.as_ref()),
                ScriptOrigin::Foreground,
                &start,
            );
            assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
            assert_eq!(
                notices.take_all(),
                vec![EditBypassUsage::new(
                    expected_mech,
                    Some("src/_pytest/logging.py")
                )],
                "script: {script}"
            );
        }

        // Read-only `str.replace` on a string literal variable must NOT alert.
        let notices = EditBypassNotices::with_sandbox_dir(&sandbox);
        let policy = HashlineEditBypassPolicy::new(notices.clone());
        let read_only = "python3 - <<'EOF'\ntext = 'src/_pytest/logging.py'\nprint(text.replace('a', 'b'))\nEOF";
        let parsed = ShellScript::parse(read_only);
        let check = check_with(
            aither_sandbox::LaunchLease::fresh().id(),
            read_only,
            Some(parsed.as_ref()),
            ScriptOrigin::Foreground,
            &start,
        );
        assert_eq!(policy.check(&check, &()).await, ScriptDecision::Pass);
        assert_eq!(
            notices.take_all(),
            [] as [hashline_bypass::EditBypassUsage; 0]
        );
    }
}

#[tokio::test]
async fn sed_in_place_refuses_with_mode_appropriate_editor_and_avoids_false_positives() {
    for script in [
        "sed -i 's/a/b/' file.py",
        "sed -i '' 's/a/b/' file.py",
        "sed -i.bak 's/a/b/' file.py",
        "sed -Ei 's/a/b/' file.py",
        "sed --in-place 's/a/b/' file.py",
        "sed --in-place=.bak 's/a/b/' file.py",
        "sed 's/a/b/' -i file.py",
        "cd repo && sed -i 's/a/b/' src/app.py",
    ] {
        let hashline_decision = decide(
            &SedInPlace::new(true),
            &(),
            script,
            ScriptOrigin::Foreground,
        )
        .await;
        let hashline_text = refusal(&hashline_decision)
            .unwrap_or_else(|| panic!("hashline SedInPlace must refuse `{script}`"));
        assert!(
            hashline_text.contains("`edit`") && !hashline_text.contains("`apply_patch`"),
            "hashline refusal should name `edit` for `{script}`: {hashline_text}"
        );

        let standard_decision = decide(
            &SedInPlace::new(false),
            &(),
            script,
            ScriptOrigin::Foreground,
        )
        .await;
        let standard_text = refusal(&standard_decision)
            .unwrap_or_else(|| panic!("standard SedInPlace must refuse `{script}`"));
        assert!(
            standard_text.contains("`apply_patch`") && !standard_text.contains("`edit`"),
            "standard refusal should name `apply_patch` for `{script}`: {standard_text}"
        );
    }

    for allowed in [
        "search sed -n '1,50p' file.py",
        "search sed -i 's/a/b/' file.py",
        "sed -n '1,50p' file.py",
        "sed -E 's/(a)b/\\1/' file.py",
        "sed -e '-i' file.py",
        "sed --expression=-i file.py",
        "sed -e-i file.py",
        "sed -n '/-i/p' file.py",
        "sed -- -i file.py",
        "echo \"sed -i 's/a/b/' file.py\"",
        "rg -i 'sed -i' file.py",
    ] {
        for hashline in [false, true] {
            assert_eq!(
                decide(
                    &SedInPlace::new(hashline),
                    &(),
                    allowed,
                    ScriptOrigin::Foreground
                )
                .await,
                ScriptDecision::Pass,
                "SedInPlace(hashline={hashline}) must not false-positive on `{allowed}`"
            );
        }
    }

    // Internal `ScriptOrigin::Run` scripts pass.
    assert_eq!(
        decide(
            &SedInPlace::new(true),
            &(),
            "sed -i 's/a/b/' file.py",
            ScriptOrigin::Run
        )
        .await,
        ScriptDecision::Pass
    );
}
