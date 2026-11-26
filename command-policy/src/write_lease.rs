//! Exclusive workspace write lease held by an asynchronous Fusion `sidekick` turn.
//!
//! While an async sidekick runs in the shared repository clones, the lead's shell
//! checks every script against [`WorkspaceWriteLeasePolicy`]: writes to workspace
//! repository working trees are refused, while read-only commands, `sidekick`
//! steers, `$MAY_TASKS`, and `<session>/` scratch files (outside `<session>/repos/`)
//! remain allowed.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aither_sandbox::{
    Interpreter, ScriptCheck, ScriptDecision, ScriptPolicy, ShellCommand, ShellScript,
};
use askama::Template;
use may_protocol::command_reading::{CommandReading, Intent};

use crate::{parsed, refuse};

#[derive(Template)]
#[template(path = "refused_workspace_write_lease.txt", escape = "none")]
struct WorkspaceWriteLeaseRefused<'a> {
    detail: &'a str,
}

#[derive(Debug)]
struct WorkspaceWriteLeaseInner {
    held: AtomicUsize,
    sandbox_dir: PathBuf,
    repos_dir: PathBuf,
    tasks_path: PathBuf,
}

/// Shared lease handle toggled while an async Fusion `sidekick` turn owns
/// workspace repository working-tree writes.
#[derive(Debug, Clone)]
pub struct WorkspaceWriteLease {
    inner: Arc<WorkspaceWriteLeaseInner>,
}

/// RAII guard releasing a [`WorkspaceWriteLease`] acquisition on drop.
#[derive(Debug)]
pub struct WorkspaceWriteLeaseGuard {
    inner: Arc<WorkspaceWriteLeaseInner>,
}

impl Drop for WorkspaceWriteLeaseGuard {
    fn drop(&mut self) {
        self.inner.held.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WorkspaceWriteLease {
    /// Creates an unheld lease for a lead session rooted at `sandbox_dir` with
    /// `$MAY_TASKS` at `tasks_path`.
    #[must_use]
    pub fn new(sandbox_dir: &Path, tasks_path: &Path) -> Self {
        let sandbox_dir = normalize_path(sandbox_dir);
        let repos_dir = normalize_path(&sandbox_dir.join("repos"));
        let tasks_path = normalize_path(tasks_path);
        Self {
            inner: Arc::new(WorkspaceWriteLeaseInner {
                held: AtomicUsize::new(0),
                sandbox_dir,
                repos_dir,
                tasks_path,
            }),
        }
    }

    /// Acquires the exclusive workspace write lease until the returned guard drops.
    #[must_use]
    pub fn acquire(&self) -> WorkspaceWriteLeaseGuard {
        self.inner.held.fetch_add(1, Ordering::AcqRel);
        WorkspaceWriteLeaseGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Whether an asynchronous `sidekick` turn currently holds the workspace write lease.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.inner.held.load(Ordering::Acquire) > 0
    }

    /// Whether `path` (absolute or resolved against `cwd`) is inside the session's
    /// scratch space (excluding `<session>/repos/`), `$MAY_TASKS`, or `/dev/null`.
    #[must_use]
    pub fn is_allowed_write_path(&self, cwd: &Path, raw: &str) -> bool {
        let expanded = raw.trim().trim_matches('"').trim_matches('\'');
        if expanded.is_empty() || expanded == "/dev/null" {
            return true;
        }
        if expanded == "$MAY_TASKS" || expanded == "${MAY_TASKS}" {
            return true;
        }
        if expanded.starts_with("$TMPDIR/") || expanded.starts_with("${TMPDIR}/") {
            return true;
        }
        let path = Path::new(expanded);
        let resolved = if path.is_absolute() {
            normalize_path(path)
        } else {
            normalize_path(&cwd.join(path))
        };
        self.is_normalized_scratch_or_tasks(&resolved)
    }

    fn is_normalized_scratch_or_tasks(&self, resolved: &Path) -> bool {
        if resolved == self.inner.tasks_path || resolved == Path::new("/dev/null") {
            return true;
        }
        resolved.starts_with(&self.inner.sandbox_dir)
            && !resolved.starts_with(&self.inner.repos_dir)
    }

    fn is_cwd_workspace_tree(&self, cwd: &Path) -> bool {
        let resolved = normalize_path(cwd);
        !self.is_normalized_scratch_or_tasks(&resolved)
    }
}

/// Script policy refusing lead workspace-tree writes while [`WorkspaceWriteLease`] is held.
#[derive(Debug, Clone)]
pub struct WorkspaceWriteLeasePolicy {
    lease: WorkspaceWriteLease,
}

impl WorkspaceWriteLeasePolicy {
    /// Creates a policy enforcing `lease`.
    #[must_use]
    pub const fn new(lease: WorkspaceWriteLease) -> Self {
        Self { lease }
    }

    fn check_script(&self, script: &ScriptCheck<'_>) -> ScriptDecision {
        if !self.lease.is_held() {
            return ScriptDecision::Pass;
        }
        let Some(start) = script.start.host() else {
            return ScriptDecision::Pass;
        };
        if script.interpreter == Interpreter::Python {
            return self.check_python(script.script, start);
        }
        let Some(shell) = parsed(script) else {
            return ScriptDecision::Pass;
        };
        if let Some(detail) = self.find_workspace_write(script.script, shell, start) {
            return refuse(&WorkspaceWriteLeaseRefused { detail: &detail });
        }
        ScriptDecision::Pass
    }

    fn check_python(&self, source: &str, start: &Path) -> ScriptDecision {
        let mutates = python_looks_mutating(source);
        if !mutates {
            return ScriptDecision::Pass;
        }
        if self.lease.is_cwd_workspace_tree(start) {
            return refuse(&WorkspaceWriteLeaseRefused {
                detail: "python script writing in workspace tree",
            });
        }
        // Even from scratch cwd, refuse if the script references repos/ or an explicit
        // non-scratch absolute path alongside a write call.
        if source.contains("repos/") || source.contains("/repos") {
            return refuse(&WorkspaceWriteLeaseRefused {
                detail: "python script writing to workspace repository tree",
            });
        }
        ScriptDecision::Pass
    }

    fn find_workspace_write(
        &self,
        raw_script: &str,
        shell: &ShellScript,
        start: &Path,
    ) -> Option<String> {
        let mut cwd = normalize_path(start);
        // 1. Check `apply_patch` / `edit` targets via `CommandReading`.
        let reading = CommandReading::of(raw_script);
        for step in &reading.steps {
            if let Intent::Edit { files } = &step.intent {
                for file in files {
                    if !self.lease.is_allowed_write_path(&cwd, &file.path) {
                        return Some(format!("edit {}", file.path));
                    }
                }
            }
        }

        // 2. Check shell output redirections (`>`, `>>`, `>|`, `&>`).
        for target in extract_shell_redirect_targets(raw_script) {
            if !self.lease.is_allowed_write_path(&cwd, &target) {
                return Some(format!("redirect to {target}"));
            }
        }

        // 3. Walk parsed shell commands tracking `cd`.
        for command in shell.commands() {
            let program = command.program();
            let args = command.arguments();
            if program == "cd" {
                if let Some(dest) = first_positional(args) {
                    cwd = resolve_path(&cwd, dest);
                }
                continue;
            }
            if let Some(detail) = self.command_writes_workspace(command, &cwd) {
                return Some(detail);
            }
        }
        None
    }

    fn command_writes_workspace(&self, command: &ShellCommand, cwd: &Path) -> Option<String> {
        let program = command.program();
        let args = command.arguments();
        match program {
            // Always allowed lead coordination, read-only, or `$MAY_TASKS` commands.
            "sidekick" | "tasks" | "ask_user" | "withdraw_ask_user" | "issue" | "session"
            | "models" | "board" | "agent" | "btw" | "pipe" | "search" | "rag" | "explore"
            | "subagent" | "cat" | "head" | "tail" | "nl" | "wc" | "ls" | "pwd" | "echo"
            | "printf" | "rg" | "fd" | "sg" | "ast-grep" | "jq" | "stat" | "file" | "readlink"
            | "realpath" | "basename" | "dirname" | "which" | "type" | "command" | "env"
            | "printenv" | "true" | "false" | "test" | "[" | "[[" | "diff" | "cmp" | "comm"
            | "sort" | "uniq" | "cut" | "tr" | "paste" | "column" | "rev" | "tac" | "fold"
            | "shasum" | "sha256sum" | "md5sum" | "cksum" | "du" | "df" | "date" | "uname"
            | "id" | "whoami" | "ps" | "tree" | "bat" | "less" | "more" | "strings" | "hexdump"
            | "xxd" | "od" | "nm" | "objdump" | "otool" | "readelf" => None,

            "apply_patch" | "edit" | "patch" => {
                if self.lease.is_cwd_workspace_tree(cwd) {
                    Some(format!("{program} in {}", cwd.display()))
                } else {
                    None
                }
            }

            "tee" => {
                for target in positional_args(args) {
                    if !self.lease.is_allowed_write_path(cwd, target) {
                        return Some(format!("tee {target}"));
                    }
                }
                None
            }

            "rm" | "rmdir" | "mkdir" | "touch" | "truncate" | "chmod" | "chown" | "chgrp"
            | "ed" | "ex" | "vi" | "vim" | "nano" => {
                let targets = positional_args(args);
                if targets.is_empty() {
                    if self.lease.is_cwd_workspace_tree(cwd) {
                        return Some(program.to_string());
                    }
                } else {
                    for target in targets {
                        if !self.lease.is_allowed_write_path(cwd, target) {
                            return Some(format!("{program} {target}"));
                        }
                    }
                }
                None
            }

            "cp" | "mv" | "ln" | "install" => {
                let targets = positional_args(args);
                if let Some(dest) = targets.last()
                    && !self.lease.is_allowed_write_path(cwd, dest)
                {
                    return Some(format!("{program} -> {dest}"));
                }
                if program == "mv" {
                    for src in targets.iter().take(targets.len().saturating_sub(1)) {
                        if !self.lease.is_allowed_write_path(cwd, src) {
                            return Some(format!("mv {src}"));
                        }
                    }
                }
                None
            }

            "dd" => {
                for arg in args {
                    if let Some(target) = arg.strip_prefix("of=")
                        && !self.lease.is_allowed_write_path(cwd, target)
                    {
                        return Some(format!("dd of={target}"));
                    }
                }
                None
            }

            "sed" | "perl" => {
                let in_place = args.iter().any(|arg| {
                    arg == "-i"
                        || arg == "--in-place"
                        || arg.starts_with("-i")
                        || arg.starts_with("--in-place=")
                });
                if !in_place {
                    return None;
                }
                let targets = positional_args(args);
                for target in targets {
                    if !self.lease.is_allowed_write_path(cwd, target) {
                        return Some(format!("{program} -i {target}"));
                    }
                }
                if self.lease.is_cwd_workspace_tree(cwd) {
                    return Some(format!("{program} -i"));
                }
                None
            }

            "git" => self.git_writes_workspace(args, cwd),

            "cargo" => self.cargo_writes_workspace(args, cwd),

            "rustfmt" | "gofmt" | "clang-format" | "prettier" | "eslint" | "ruff" | "black" => {
                let check_only = args.iter().any(|arg| arg == "--check" || arg == "-l");
                if check_only {
                    return None;
                }
                let targets = positional_args(args);
                if targets.is_empty() {
                    if self.lease.is_cwd_workspace_tree(cwd) {
                        return Some(program.to_string());
                    }
                } else {
                    for target in targets {
                        if !self.lease.is_allowed_write_path(cwd, target) {
                            return Some(format!("{program} {target}"));
                        }
                    }
                }
                None
            }

            name if is_python_program(name) => {
                if let Some(code) = inline_flag_value(args, "-c")
                    && python_looks_mutating(code)
                    && (self.lease.is_cwd_workspace_tree(cwd)
                        || code.contains("repos/")
                        || code.contains("/repos"))
                {
                    return Some(format!("{name} -c"));
                }
                None
            }

            "node" | "bun" | "deno" => {
                if let Some(code) =
                    inline_flag_value(args, "-e").or_else(|| inline_flag_value(args, "--eval"))
                    && js_looks_mutating(code)
                    && (self.lease.is_cwd_workspace_tree(cwd)
                        || code.contains("repos/")
                        || code.contains("/repos"))
                {
                    return Some(format!("{program} -e"));
                }
                None
            }

            _ => None,
        }
    }

    fn git_writes_workspace(&self, args: &[String], cwd: &Path) -> Option<String> {
        let mut effective_cwd = cwd.to_path_buf();
        let mut subcmd: Option<&str> = None;
        let mut subcmd_index = 0usize;
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "-C" {
                if let Some(dir) = args.get(i + 1) {
                    effective_cwd = resolve_path(&effective_cwd, dir);
                }
                i += 2;
                continue;
            }
            if let Some(dir) = arg.strip_prefix("-C") {
                if !dir.is_empty() {
                    effective_cwd = resolve_path(&effective_cwd, dir);
                }
                i += 1;
                continue;
            }
            if arg.starts_with('-') {
                if matches!(arg, "-c" | "--git-dir" | "--work-tree") {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            subcmd = Some(arg);
            subcmd_index = i;
            break;
        }
        let subcmd = subcmd?;
        if !self.lease.is_cwd_workspace_tree(&effective_cwd) {
            return None;
        }
        let rest = &args[subcmd_index + 1..];
        let mutates = match subcmd {
            "add" | "am" | "apply" | "checkout" | "cherry-pick" | "clean" | "commit" | "merge"
            | "mv" | "pull" | "rebase" | "reset" | "restore" | "revert" | "rm" | "switch" => true,
            "stash" => !matches!(rest.first().map(String::as_str), Some("list" | "show")),
            "branch" => rest.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--move" | "--copy"
                )
            }),
            "tag" => rest.iter().any(|a| a == "-d" || a == "--delete"),
            _ => false,
        };
        mutates.then(|| format!("git {subcmd}"))
    }

    fn cargo_writes_workspace(&self, args: &[String], cwd: &Path) -> Option<String> {
        if !self.lease.is_cwd_workspace_tree(cwd) {
            return None;
        }
        let subcmd = first_positional(args)?;
        let mutates = match subcmd {
            "fmt" => !args.iter().any(|a| a == "--check"),
            "fix" | "add" | "remove" | "rm" | "upgrade" => true,
            "clippy" => args.iter().any(|a| a == "--fix"),
            _ => false,
        };
        mutates.then(|| format!("cargo {subcmd}"))
    }
}

impl ScriptPolicy for WorkspaceWriteLeasePolicy {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        std::future::ready(self.check_script(script))
    }
}

fn is_python_program(name: &str) -> bool {
    name.strip_prefix("python")
        .is_some_and(|suffix| suffix.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
}

fn python_looks_mutating(source: &str) -> bool {
    const PATTERNS: &[&str] = &[
        ".write_text(",
        ".write_bytes(",
        "open(",
        "shutil.",
        "os.remove(",
        "os.unlink(",
        "os.rename(",
        "os.replace(",
        "os.mkdir(",
        "os.makedirs(",
        "os.rmdir(",
        "Path.unlink(",
        ".unlink(",
        ".rename(",
        ".replace(",
        ".mkdir(",
        "subprocess.",
        "os.system(",
    ];
    if !PATTERNS.iter().any(|pat| source.contains(pat)) {
        return false;
    }
    // `open(path, "r")` / `open(path)` alone is read-only unless a write mode is passed.
    if source.contains("open(")
        && !PATTERNS
            .iter()
            .filter(|&&pat| pat != "open(")
            .any(|pat| source.contains(pat))
    {
        return source.contains("\"w")
            || source.contains("'w")
            || source.contains("\"a")
            || source.contains("'a")
            || source.contains("\"x")
            || source.contains("'x")
            || source.contains('+')
            || source.contains(".write(");
    }
    true
}

fn js_looks_mutating(source: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "writeFile",
        "appendFile",
        "unlink",
        "rmSync",
        "rmdir",
        "mkdir",
        "rename",
        "copyFile",
        "Bun.write",
    ];
    PATTERNS.iter().any(|pat| source.contains(pat))
}

fn first_positional(args: &[String]) -> Option<&str> {
    args.iter()
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-'))
}

fn positional_args(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut after_double_dash = false;
    for arg in args {
        if after_double_dash {
            out.push(arg.as_str());
        } else if arg == "--" {
            after_double_dash = true;
        } else if !arg.starts_with('-') {
            out.push(arg.as_str());
        }
    }
    out
}

fn inline_flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let pos = args.iter().position(|arg| arg == flag)?;
    args.get(pos + 1).map(String::as_str)
}

fn resolve_path(cwd: &Path, target: &str) -> PathBuf {
    let path = Path::new(target);
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&cwd.join(path))
    }
}

/// Lexically normalizes `.` and `..` components without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Extracts file paths targeted by shell output redirections (`>`, `>>`, `>|`, `&>`, `1>`, `2>`)
/// outside single/double quotes and heredoc bodies, skipping fd duplications like `2>&1`.
fn extract_shell_redirect_targets(script: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = script.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' => {
                escaped = true;
                i += 1;
            }
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'#' if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'>' => {
                // Skip fd duplication `>&1`, `>&2`, `>&-`.
                if bytes.get(i + 1) == Some(&b'&')
                    && bytes
                        .get(i + 2)
                        .is_some_and(|next| next.is_ascii_digit() || *next == b'-')
                {
                    i += 3;
                    continue;
                }
                i += 1;
                if matches!(bytes.get(i), Some(b'>' | b'|' | b'&')) {
                    i += 1;
                }
                while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                    i += 1;
                }
                let start = i;
                let mut quote: Option<u8> = None;
                while i < bytes.len() {
                    let ch = bytes[i];
                    if let Some(q) = quote {
                        if ch == q {
                            quote = None;
                        }
                        i += 1;
                    } else if ch == b'\'' || ch == b'"' {
                        quote = Some(ch);
                        i += 1;
                    } else if ch.is_ascii_whitespace()
                        || matches!(ch, b';' | b'|' | b'&' | b')' | b'(' | b'<')
                    {
                        break;
                    } else {
                        i += 1;
                    }
                }
                if i > start {
                    let raw = &script[start..i];
                    let cleaned = raw.trim_matches('"').trim_matches('\'');
                    if !cleaned.is_empty() && !cleaned.starts_with('&') {
                        targets.push(cleaned.to_string());
                    }
                }
            }
            _ => i += 1,
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_sandbox::{LaunchLease, SandboxMode, ScriptDir, ScriptOrigin};

    async fn check_shell(
        policy: &WorkspaceWriteLeasePolicy,
        start: &Path,
        script: &str,
    ) -> ScriptDecision {
        let parsed = ShellScript::parse(script);
        let dir = ScriptDir::Host(start.to_path_buf());
        let check = ScriptCheck {
            launch: LaunchLease::fresh().id(),
            script,
            interpreter: Interpreter::Shell,
            shell: Some(parsed.as_ref()),
            origin: ScriptOrigin::Foreground,
            sandbox: SandboxMode::Sandboxed,
            location: None,
            start: &dir,
            call: None,
        };
        policy.check(&check, &()).await
    }

    #[tokio::test]
    async fn lease_allows_everything_when_unheld() {
        let sandbox = PathBuf::from("/tmp/session");
        let tasks = sandbox.join("tasks.md");
        let lease = WorkspaceWriteLease::new(&sandbox, &tasks);
        let policy = WorkspaceWriteLeasePolicy::new(lease);
        let repo = sandbox.join("repos/demo");
        assert!(matches!(
            check_shell(&policy, &repo, "echo hi > src/lib.rs").await,
            ScriptDecision::Pass
        ));
    }

    #[tokio::test]
    async fn lease_refuses_workspace_writes_but_allows_reads_scratch_and_tasks() {
        let sandbox = PathBuf::from("/tmp/session");
        let tasks = sandbox.join("tasks.md");
        let lease = WorkspaceWriteLease::new(&sandbox, &tasks);
        let _guard = lease.acquire();
        let policy = WorkspaceWriteLeasePolicy::new(lease);
        let repo = sandbox.join("repos/demo");

        // Workspace repo writes are refused.
        assert!(matches!(
            check_shell(&policy, &repo, "echo hi > src/lib.rs").await,
            ScriptDecision::Refuse(_)
        ));
        assert!(matches!(
            check_shell(&policy, &repo, "rm src/lib.rs").await,
            ScriptDecision::Refuse(_)
        ));
        assert!(matches!(
            check_shell(&policy, &repo, "git commit -m 'x'").await,
            ScriptDecision::Refuse(_)
        ));
        assert!(matches!(
            check_shell(&policy, &sandbox, "echo hi > repos/demo/src/lib.rs").await,
            ScriptDecision::Refuse(_)
        ));
        assert!(matches!(
            check_shell(
                &policy,
                &repo,
                "python3 -c 'open(\"a.txt\", \"w\").write(\"x\")'"
            )
            .await,
            ScriptDecision::Refuse(_)
        ));

        // Read-only commands in workspace repo are allowed.
        assert!(matches!(
            check_shell(&policy, &repo, "git status && git diff && rg foo src/").await,
            ScriptDecision::Pass
        ));
        assert!(matches!(
            check_shell(&policy, &repo, "cat src/lib.rs 2>&1 | head -n 20").await,
            ScriptDecision::Pass
        ));

        // Scratch files under `<session>/` (outside `repos/`), `$MAY_TASKS`, and `sidekick` are allowed.
        assert!(matches!(
            check_shell(&policy, &sandbox, "echo notes > tmp/plan.md").await,
            ScriptDecision::Pass
        ));
        assert!(matches!(
            check_shell(&policy, &repo, "echo notes > /tmp/session/tmp/plan.md").await,
            ScriptDecision::Pass
        ));
        assert!(matches!(
            check_shell(&policy, &repo, "tasks add 'review sidekick diff'").await,
            ScriptDecision::Pass
        ));
        assert!(matches!(
            check_shell(&policy, &repo, "sidekick 'also check edge case'").await,
            ScriptDecision::Pass
        ));
    }
}
