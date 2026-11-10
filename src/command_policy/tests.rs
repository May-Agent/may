use aither_sandbox::{CommandRole, ScriptLanguage, ShellParseError};

use super::*;

fn check_with<'a>(
    task_id: &'a str,
    script: &'a str,
    shell: Option<Result<&'a ShellScript, &'a ShellParseError>>,
    origin: ScriptOrigin,
    working_dir: &'a Path,
) -> ScriptCheck<'a> {
    ScriptCheck {
        task_id,
        script,
        language: ScriptLanguage::Shell,
        shell,
        origin,
        role: CommandRole::Main,
        working_dir,
        call: None,
    }
}

async fn decide<P: CommandPolicy>(
    policy: &P,
    state: &P::State,
    script: &str,
    origin: ScriptOrigin,
) -> PolicyDecision {
    let shell = ShellScript::parse(script);
    let check = check_with(
        "task-a",
        script,
        Some(shell.as_ref()),
        origin,
        Path::new("/"),
    );
    policy.check(&check, state).await
}

fn refusal(decision: &PolicyDecision) -> Option<&str> {
    match decision {
        PolicyDecision::Allow => None,
        PolicyDecision::Refuse(refusal) => Some(refusal.text()),
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
        assert_eq!(decision, PolicyDecision::Allow, "`{script}` runs");
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
    let mut python = check_with(
        "task-a",
        "import os",
        None,
        ScriptOrigin::Foreground,
        Path::new("/"),
    );
    python.language = ScriptLanguage::Python;
    assert_eq!(KillByName.check(&python, &()).await, PolicyDecision::Allow);
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
            PolicyDecision::Allow,
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
    let shell = ShellScript::parse("tail -50 log.txt").expect("parses");
    let check = |task_id| {
        check_with(
            task_id,
            "tail -50 log.txt",
            Some(Ok(&shell)),
            ScriptOrigin::Foreground,
            dir.path(),
        )
    };

    assert_eq!(
        UnchangedFileReads.check(&check("task-a"), &state).await,
        PolicyDecision::Allow
    );
    UnchangedFileReads
        .ended(
            &ScriptEnd {
                task_id: "task-a",
                outcome: ScriptOutcome::Exited { code: 1 },
            },
            &state,
        )
        .await;
    assert_eq!(
        UnchangedFileReads.check(&check("task-b"), &state).await,
        PolicyDecision::Allow,
        "a failed read records nothing"
    );
    UnchangedFileReads
        .ended(
            &ScriptEnd {
                task_id: "task-b",
                outcome: ScriptOutcome::Exited { code: 0 },
            },
            &state,
        )
        .await;

    let PolicyDecision::Refuse(refused) = UnchangedFileReads.check(&check("task-c"), &state).await
    else {
        panic!("the unchanged re-read is refused");
    };
    assert!(refused.is_unchanged_read());
    assert!(
        refused.text().contains(&format!(
            "`{}`, unchanged since the last read",
            log.display()
        )),
        "{}",
        refused.text()
    );

    std::fs::write(&log, "v1 and more").expect("rewrite");
    assert_eq!(
        UnchangedFileReads.check(&check("task-d"), &state).await,
        PolicyDecision::Allow,
        "a changed file reads again"
    );
}

#[tokio::test]
async fn a_follow_or_a_missing_file_is_no_reread() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("log.txt"), "v1").expect("write");
    let state = FileReads::default();
    for script in ["tail -f log.txt", "cat missing.txt"] {
        let shell = ShellScript::parse(script).expect("parses");
        let check = check_with(
            "task-a",
            script,
            Some(Ok(&shell)),
            ScriptOrigin::Foreground,
            dir.path(),
        );
        assert_eq!(
            UnchangedFileReads.check(&check, &state).await,
            PolicyDecision::Allow
        );
        assert!(
            state.inner().pending.is_empty(),
            "`{script}` tracks no read"
        );
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
            PolicyDecision::Allow,
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
            PolicyDecision::Allow,
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
            PolicyDecision::Allow,
            "`{script}` runs"
        );
    }
}
