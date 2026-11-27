//! One-shot alert policy observing attempts to modify existing files outside
//! `edit` while experimental hashline mode is enabled.
//!
//! Every checked script returns [`ScriptDecision::Pass`] so legitimate edge
//! cases are never hard-blocked; detected bypasses are recorded per
//! [`AgentPath`] in [`EditBypassNotices`] for one-shot delivery on the agent's
//! next model request.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use aither_agent::AgentPath;
use aither_sandbox::{Interpreter, ScriptCheck, ScriptDecision, ScriptOrigin, ScriptPolicy};
use may_protocol::command_reading::{ShellAction, ShellActionKind, inspect_shell_actions};

use crate::write_lease::{
    inline_flag_value, is_python_program, normalize_path, positional_args, resolve_path,
};

/// Session scratch directory prefixes under which direct file writes are allowed.
const SCRATCH_PREFIXES: &[&str] = &["tmp", "files", "outputs", "artifacts", "data"];

/// A detected bypass of `edit` in hashline mode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EditBypassUsage {
    /// Bypassed command or mechanism (for example `sed -i`, `apply_patch`,
    /// `python3 -c`, `shell redirection`, `tee`, `cp`, `mv`).
    pub mechanism: String,
    /// Existing non-scratch file targeted, when statically known.
    pub target: Option<String>,
}

impl EditBypassUsage {
    /// Constructs a bypass notice naming `mechanism` and an optional `target`.
    #[must_use]
    pub fn new(mechanism: impl Into<String>, target: Option<&str>) -> Self {
        Self {
            mechanism: mechanism.into(),
            target: target.map(str::to_owned),
        }
    }
}

/// Pending hashline `edit`-bypass notices observed by [`HashlineEditBypassPolicy`],
/// keyed by [`AgentPath`] so the main agent and concurrent subagents drain only
/// their own alerts. Every clone shares the same store.
#[derive(Debug, Clone, Default)]
pub struct EditBypassNotices {
    pending: Arc<Mutex<HashMap<AgentPath, Vec<EditBypassUsage>>>>,
    sandbox_dir: Option<Arc<PathBuf>>,
}

impl EditBypassNotices {
    /// Creates a notice store scoped to `sandbox_dir` so absolute paths inside
    /// the session's scratch subdirectories (`tmp/`, `files/`, `outputs/`,
    /// `artifacts/`, `data/`) are recognized as scratch.
    #[must_use]
    pub fn with_sandbox_dir(sandbox_dir: &Path) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            sandbox_dir: Some(Arc::new(normalize_path(sandbox_dir))),
        }
    }

    /// Records `usage` for the main agent if not already queued.
    ///
    /// # Panics
    /// Panics when the notice store lock is poisoned.
    pub fn record(&self, usage: EditBypassUsage) {
        self.record_for(&AgentPath::main(), usage);
    }

    /// Records `usage` for `agent` if not already queued.
    ///
    /// # Panics
    /// Panics when the notice store lock is poisoned.
    pub fn record_for(&self, agent: &AgentPath, usage: EditBypassUsage) {
        let mut map = self
            .pending
            .lock()
            .expect("the edit bypass notice map is never held across a panic");
        let queue = map.entry(agent.clone()).or_default();
        if !queue.contains(&usage) {
            queue.push(usage);
        }
        drop(map);
    }

    /// Drains all queued notices for the main agent.
    ///
    /// # Panics
    /// Panics when the notice store lock is poisoned.
    #[must_use]
    pub fn take_all(&self) -> Vec<EditBypassUsage> {
        self.take_all_for(&AgentPath::main())
    }

    /// Drains all queued notices for `agent`.
    ///
    /// # Panics
    /// Panics when the notice store lock is poisoned.
    #[must_use]
    pub fn take_all_for(&self, agent: &AgentPath) -> Vec<EditBypassUsage> {
        self.pending
            .lock()
            .expect("the edit bypass notice map is never held across a panic")
            .remove(agent)
            .unwrap_or_default()
    }

    /// Peeks at all queued notices for the main agent without consuming them.
    ///
    /// # Panics
    /// Panics when the notice store lock is poisoned.
    #[must_use]
    pub fn peek_all(&self) -> Vec<EditBypassUsage> {
        self.peek_all_for(&AgentPath::main())
    }

    /// Peeks at all queued notices for `agent` without consuming them.
    ///
    /// # Panics
    /// Panics when the notice store lock is poisoned.
    #[must_use]
    pub fn peek_all_for(&self, agent: &AgentPath) -> Vec<EditBypassUsage> {
        self.pending
            .lock()
            .expect("the edit bypass notice map is never held across a panic")
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }

    fn sandbox_dir(&self) -> Option<&Path> {
        self.sandbox_dir.as_deref().map(PathBuf::as_path)
    }
}

/// Observes scripts in hashline mode that bypass `edit` to modify existing
/// non-scratch files, allowing the script to run while queueing a one-shot
/// notice in [`EditBypassNotices`].
#[derive(Debug, Clone)]
pub struct HashlineEditBypassPolicy {
    notices: EditBypassNotices,
}

impl HashlineEditBypassPolicy {
    /// Creates a policy recording bypass events into `notices`.
    #[must_use]
    pub const fn new(notices: EditBypassNotices) -> Self {
        Self { notices }
    }

    async fn inspect(&self, script: &ScriptCheck<'_>) -> Vec<EditBypassUsage> {
        if script.origin == ScriptOrigin::Run {
            return Vec::new();
        }
        let start = script.start.host();
        let sandbox_dir = self.notices.sandbox_dir();
        if script.interpreter == Interpreter::Python {
            return check_scripted_mutation(
                "python",
                &python_file_write_targets(script.script),
                start,
                sandbox_dir,
            )
            .await;
        }
        let mut found = Vec::new();
        if let Some(actions) = inspect_shell_actions(script.script) {
            for action in &actions {
                let effective_cwd = start.map(|base| apply_cd_steps(base, &action.cd_steps));
                self.inspect_action(action, effective_cwd.as_deref(), sandbox_dir, &mut found)
                    .await;
            }
        }
        // Also scan `ShellScript` commands to catch wrappers like `xargs` or `find -exec`
        // that `inspect_shell_actions` does not unwrap.
        if let Some(shell) = crate::parsed(script) {
            for command in shell.commands() {
                if command.wrapper().is_some()
                    && let Some(usage) =
                        check_always_flagged_command(command.program(), command.arguments())
                {
                    push_unique(&mut found, usage);
                }
            }
        }
        found
    }

    async fn inspect_action(
        &self,
        action: &ShellAction,
        cwd: Option<&Path>,
        sandbox_dir: Option<&Path>,
        out: &mut Vec<EditBypassUsage>,
    ) {
        match &action.kind {
            ShellActionKind::OutputRedirect { target } => {
                if target_is_existing_non_scratch(target, cwd, sandbox_dir).await {
                    push_unique(out, EditBypassUsage::new("shell redirection", Some(target)));
                }
            }
            ShellActionKind::StdinScript {
                program,
                args,
                stdin,
            } => {
                let (eff_program, eff_args) = unwrap_uv_run(program, args);
                if let Some((mechanism, targets)) =
                    inspect_stdin_interpreter(eff_program, eff_args, stdin)
                {
                    for usage in
                        check_scripted_mutation(&mechanism, &targets, cwd, sandbox_dir).await
                    {
                        push_unique(out, usage);
                    }
                }
            }
            ShellActionKind::Command { program, args } => {
                let (eff_program, eff_args) = unwrap_uv_run(program, args);
                if let Some(usage) = check_always_flagged_command(eff_program, eff_args) {
                    push_unique(out, usage);
                    return;
                }
                if let Some((mechanism, targets)) =
                    inspect_inline_interpreter(eff_program, eff_args)
                {
                    for usage in
                        check_scripted_mutation(&mechanism, &targets, cwd, sandbox_dir).await
                    {
                        push_unique(out, usage);
                    }
                    return;
                }
                match eff_program {
                    "tee" => {
                        for target in tee_targets(eff_args) {
                            if target_is_existing_non_scratch(target, cwd, sandbox_dir).await {
                                push_unique(out, EditBypassUsage::new("tee", Some(target)));
                            }
                        }
                    }
                    "cp" | "mv" => {
                        for target in cp_or_mv_overwritten_targets(eff_program, eff_args, cwd).await
                        {
                            if target_is_existing_non_scratch(&target, cwd, sandbox_dir).await {
                                push_unique(
                                    out,
                                    EditBypassUsage::new(eff_program, Some(target.as_str())),
                                );
                            }
                        }
                    }
                    "dd" => {
                        for arg in eff_args {
                            if let Some(target) = arg.strip_prefix("of=")
                                && target_is_existing_non_scratch(target, cwd, sandbox_dir).await
                            {
                                push_unique(out, EditBypassUsage::new("dd", Some(target)));
                            }
                        }
                    }
                    "truncate" => {
                        for target in truncate_targets(eff_args) {
                            if target_is_existing_non_scratch(target, cwd, sandbox_dir).await {
                                push_unique(out, EditBypassUsage::new("truncate", Some(target)));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl ScriptPolicy for HashlineEditBypassPolicy {
    type State = ();

    async fn check(&self, script: &ScriptCheck<'_>, (): &()) -> ScriptDecision {
        let usages = self.inspect(script).await;
        if !usages.is_empty() {
            let agent = script
                .call
                .as_ref()
                .map_or_else(AgentPath::main, |call| AgentPath::of(call.agent()));
            for usage in usages {
                self.notices.record_for(&agent, usage);
            }
        }
        ScriptDecision::Pass
    }
}

fn push_unique(out: &mut Vec<EditBypassUsage>, usage: EditBypassUsage) {
    if !out.contains(&usage) {
        out.push(usage);
    }
}

fn apply_cd_steps(start: &Path, steps: &[String]) -> PathBuf {
    let mut cwd = normalize_path(start);
    for step in steps {
        cwd = resolve_path(&cwd, step);
    }
    cwd
}

/// Unwraps `uv run [flags] <cmd> <args...>` so `uv run python3 -c ...` and
/// `uv run python3 - <<'EOF'` are checked like direct `python3`.
fn unwrap_uv_run<'a>(program: &'a str, args: &'a [String]) -> (&'a str, &'a [String]) {
    if program != "uv" {
        return (program, args);
    }
    let Some((first, rest)) = args.split_first() else {
        return (program, args);
    };
    if first != "run" {
        return (program, args);
    }
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if arg == "--" {
            i += 1;
            break;
        }
        if matches!(
            arg,
            "--with"
                | "--with-editable"
                | "--with-requirements"
                | "--python"
                | "-p"
                | "--directory"
                | "--project"
                | "--group"
                | "--extra"
                | "--env-file"
                | "--script"
                | "-m"
        ) {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    let Some((inner, inner_args)) = rest.get(i..).and_then(<[String]>::split_first) else {
        return (program, args);
    };
    let inner_base = inner.rsplit(['/', '\\']).next().unwrap_or(inner.as_str());
    (inner_base, inner_args)
}

fn is_help_or_version(args: &[String]) -> bool {
    args.len() == 1 && matches!(args[0].as_str(), "-h" | "--help" | "-V" | "--version")
}

/// Commands unconditionally flagged as `edit` bypasses in hashline mode:
/// `apply_patch`, `sed -i`, `perl -i`/`-pi`/`-pie`, `ruby -i`, `ed`, `ex`,
/// `patch`, and `sponge`.
fn check_always_flagged_command(program: &str, args: &[String]) -> Option<EditBypassUsage> {
    if is_help_or_version(args) {
        return None;
    }
    match program {
        "apply_patch" => Some(EditBypassUsage::new("apply_patch", None)),
        "sed" => sed_in_place_usage(args),
        "perl" => perl_or_ruby_in_place_usage("perl", args),
        "ruby" => perl_or_ruby_in_place_usage("ruby", args),
        "ed" | "ex" => Some(EditBypassUsage::new(
            program,
            positional_args(args).last().copied(),
        )),
        "patch" => {
            if args.iter().any(|arg| arg == "--dry-run") {
                return None;
            }
            Some(EditBypassUsage::new("patch", None))
        }
        "sponge" => Some(EditBypassUsage::new(
            "sponge",
            positional_args(args).first().copied(),
        )),
        _ => None,
    }
}

fn sed_in_place_usage(args: &[String]) -> Option<EditBypassUsage> {
    let mut in_place_flag: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if arg == "--in-place" || arg.starts_with("--in-place=") {
            in_place_flag = Some("sed --in-place");
            i += 1;
            continue;
        }
        if matches!(arg, "-e" | "--expression" | "-f" | "--file") {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            for ch in cluster.chars() {
                if ch == 'i' {
                    in_place_flag = Some("sed -i");
                    break;
                }
                if matches!(ch, 'e' | 'f') {
                    // Remainder of cluster is the expression/filename, not flags.
                    break;
                }
            }
            i += 1;
            continue;
        }
        break;
    }
    let mechanism = in_place_flag?;
    let target = sed_target_file(args);
    Some(EditBypassUsage::new(mechanism, target))
}

fn sed_target_file(args: &[String]) -> Option<&str> {
    let mut has_explicit_script = false;
    let mut positionals = Vec::new();
    let mut i = 0;
    let mut after_double_dash = false;
    while i < args.len() {
        let arg = args[i].as_str();
        if !after_double_dash && arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if !after_double_dash {
            if matches!(arg, "-e" | "--expression" | "-f" | "--file") {
                has_explicit_script = true;
                i += 2;
                continue;
            }
            if arg.starts_with("--expression=") || arg.starts_with("--file=") {
                has_explicit_script = true;
                i += 1;
                continue;
            }
            if arg == "-i" {
                // BSD `sed -i '' 's/a/b/' file`: skip empty backup suffix argument.
                if args.get(i + 1).is_some_and(String::is_empty) {
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if arg.starts_with('-') && arg != "-" {
                if arg.starts_with("-e") || arg.starts_with("-f") {
                    has_explicit_script = true;
                }
                i += 1;
                continue;
            }
        }
        positionals.push(arg);
        i += 1;
    }
    let file_index = usize::from(!has_explicit_script);
    positionals.get(file_index).copied()
}

fn perl_or_ruby_in_place_usage(program: &'static str, args: &[String]) -> Option<EditBypassUsage> {
    let mut in_place = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if matches!(arg, "-e" | "-E" | "-I" | "-M" | "-m" | "-r") {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            for ch in cluster.chars() {
                if ch == 'i' {
                    in_place = true;
                    break;
                }
                if matches!(ch, 'e' | 'E' | 'I' | 'M' | 'm' | 'r' | 'x' | 'd' | 'V') {
                    break;
                }
            }
            i += 1;
            continue;
        }
        break;
    }
    if !in_place {
        return None;
    }
    Some(EditBypassUsage::new(format!("{program} -i"), None))
}

fn inspect_inline_interpreter(
    program: &str,
    args: &[String],
) -> Option<(String, Vec<Option<String>>)> {
    if is_python_program(program) {
        let code = inline_flag_value(args, "-c")?;
        return Some((format!("{program} -c"), python_file_write_targets(code)));
    }
    if matches!(program, "node" | "bun" | "deno") {
        let code = inline_flag_value(args, "-e").or_else(|| inline_flag_value(args, "--eval"))?;
        return Some((format!("{program} -e"), js_file_write_targets(code)));
    }
    if program == "ruby" {
        let code = inline_flag_value(args, "-e")?;
        return Some(("ruby -e".to_string(), ruby_file_write_targets(code)));
    }
    None
}

fn inspect_stdin_interpreter(
    program: &str,
    args: &[String],
    stdin: &str,
) -> Option<(String, Vec<Option<String>>)> {
    // Only inspect stdin when the interpreter reads code from stdin (no `-c`/`-e`
    // and no script file argument other than `-`).
    if is_python_program(program) {
        if inline_flag_value(args, "-c").is_some() || inline_flag_value(args, "-m").is_some() {
            return None;
        }
        if has_script_file_arg(args) {
            return None;
        }
        return Some((program.to_string(), python_file_write_targets(stdin)));
    }
    if matches!(program, "node" | "bun" | "deno") {
        if inline_flag_value(args, "-e").is_some() || inline_flag_value(args, "--eval").is_some() {
            return None;
        }
        if has_script_file_arg(args) {
            return None;
        }
        return Some((program.to_string(), js_file_write_targets(stdin)));
    }
    if program == "ruby" {
        if inline_flag_value(args, "-e").is_some() || has_script_file_arg(args) {
            return None;
        }
        return Some(("ruby".to_string(), ruby_file_write_targets(stdin)));
    }
    None
}

fn has_script_file_arg(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" || arg == "-" {
            return false;
        }
        if !arg.starts_with('-') {
            return true;
        }
    }
    false
}

async fn check_scripted_mutation(
    mechanism: &str,
    targets: &[Option<String>],
    cwd: Option<&Path>,
    sandbox_dir: Option<&Path>,
) -> Vec<EditBypassUsage> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for target in targets {
        if let Some(raw) = target {
            if is_scratch_or_allowed_path(raw, cwd, sandbox_dir) {
                continue;
            }
            if let Some(host_cwd) = cwd {
                if path_exists_as_file(&resolve_path(host_cwd, raw)).await {
                    push_unique(&mut found, EditBypassUsage::new(mechanism, Some(raw)));
                }
            } else {
                push_unique(&mut found, EditBypassUsage::new(mechanism, Some(raw)));
            }
        } else {
            // Unknown/dynamic target: allow only when cwd itself is clearly scratch.
            if cwd.is_some_and(|dir| is_scratch_directory(dir, sandbox_dir)) {
                continue;
            }
            push_unique(&mut found, EditBypassUsage::new(mechanism, None));
        }
    }
    found
}

fn tee_targets(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut after_double_dash = false;
    for arg in args {
        if !after_double_dash && arg == "--" {
            after_double_dash = true;
            continue;
        }
        if !after_double_dash && arg.starts_with('-') {
            continue;
        }
        if arg != "-" && !arg.is_empty() {
            out.push(arg.as_str());
        }
    }
    out
}

fn truncate_targets(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut after_double_dash = false;
    while i < args.len() {
        let arg = args[i].as_str();
        if !after_double_dash && arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if !after_double_dash {
            if matches!(arg, "-s" | "--size" | "-r" | "--reference") {
                i += 2;
                continue;
            }
            if arg.starts_with('-') {
                i += 1;
                continue;
            }
        }
        if !arg.is_empty() {
            out.push(arg);
        }
        i += 1;
    }
    out
}

async fn cp_or_mv_overwritten_targets(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> Vec<String> {
    let mut no_clobber = false;
    let mut target_dir: Option<&str> = None;
    let mut positionals: Vec<&str> = Vec::new();
    let mut i = 0;
    let mut after_double_dash = false;
    while i < args.len() {
        let arg = args[i].as_str();
        if !after_double_dash && arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if !after_double_dash && arg.starts_with('-') && arg.len() > 1 {
            if arg == "-n" || arg == "--no-clobber" {
                no_clobber = true;
                i += 1;
                continue;
            }
            if arg == "-t" || arg == "--target-directory" {
                target_dir = args.get(i + 1).map(String::as_str);
                i += 2;
                continue;
            }
            if let Some(dir) = arg.strip_prefix("--target-directory=") {
                target_dir = Some(dir);
                i += 1;
                continue;
            }
            if matches!(arg, "-S" | "--suffix")
                || (program == "cp" && matches!(arg, "--preserve" | "--no-preserve" | "--context"))
            {
                i += 2;
                continue;
            }
            if !arg.starts_with("--") && arg.contains('n') {
                no_clobber = true;
            }
            i += 1;
            continue;
        }
        positionals.push(arg);
        i += 1;
    }
    if no_clobber {
        return Vec::new();
    }
    if let Some(dir) = target_dir {
        return positionals
            .into_iter()
            .filter_map(|src| {
                let name = Path::new(src).file_name()?.to_str()?;
                Some(Path::new(dir).join(name).display().to_string())
            })
            .collect();
    }
    if positionals.len() < 2 {
        return Vec::new();
    }
    let dest = *positionals.last().expect("at least 2 positionals");
    let sources = &positionals[..positionals.len() - 1];
    if let Some(host_cwd) = cwd {
        let resolved_dest = resolve_path(host_cwd, dest);
        if async_fs::metadata(&resolved_dest)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            return sources
                .iter()
                .filter_map(|src| {
                    let name = Path::new(src).file_name()?.to_str()?;
                    Some(Path::new(dest).join(name).display().to_string())
                })
                .collect();
        }
    }
    vec![dest.to_string()]
}

async fn target_is_existing_non_scratch(
    raw: &str,
    cwd: Option<&Path>,
    sandbox_dir: Option<&Path>,
) -> bool {
    if is_scratch_or_allowed_path(raw, cwd, sandbox_dir) {
        return false;
    }
    let Some(host_cwd) = cwd else {
        return false;
    };
    path_exists_as_file(&resolve_path(host_cwd, raw)).await
}

async fn path_exists_as_file(path: &Path) -> bool {
    async_fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

fn is_scratch_or_allowed_path(raw: &str, cwd: Option<&Path>, sandbox_dir: Option<&Path>) -> bool {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return true;
    }
    // Special devices and temp/tasks variables.
    if trimmed == "/dev/null"
        || trimmed.starts_with("/dev/")
        || trimmed == "/tmp"
        || trimmed.starts_with("/tmp/")
        || trimmed == "/var/tmp"
        || trimmed.starts_with("/var/tmp/")
        || trimmed == "/private/tmp"
        || trimmed.starts_with("/private/tmp/")
        || trimmed == "$MAY_TASKS"
        || trimmed == "${MAY_TASKS}"
        || trimmed.starts_with("$TMPDIR/")
        || trimmed.starts_with("${TMPDIR}/")
    {
        return true;
    }
    // Unexpanded dynamic variable path cannot be reliably checked as an existing file.
    if trimmed.contains('$') {
        return true;
    }
    let path = Path::new(trimmed);
    // `tasks.md` is managed separately from source code edits.
    if path.file_name().and_then(std::ffi::OsStr::to_str) == Some("tasks.md") {
        return true;
    }
    // Relative paths beginning with a session scratch prefix (`tmp/`, `files/`, `outputs/`,
    // `artifacts/`, `data/`).
    if !path.is_absolute()
        && let Some(Component::Normal(first)) = normalize_path(path).components().next()
        && first
            .to_str()
            .is_some_and(|name| SCRATCH_PREFIXES.contains(&name))
    {
        return true;
    }
    // Resolved path inside `/tmp`, `/var/tmp`, `/private/tmp`, or `<sandbox_dir>/{tmp,...}`.
    let resolved = if path.is_absolute() {
        normalize_path(path)
    } else if let Some(host_cwd) = cwd {
        resolve_path(host_cwd, trimmed)
    } else {
        return false;
    };
    is_scratch_directory(&resolved, sandbox_dir)
}

fn is_scratch_directory(resolved: &Path, sandbox_dir: Option<&Path>) -> bool {
    if resolved.starts_with("/dev/")
        || resolved.starts_with("/tmp/")
        || resolved == Path::new("/tmp")
        || resolved.starts_with("/var/tmp/")
        || resolved.starts_with("/private/tmp/")
    {
        return true;
    }
    if let Some(sandbox) = sandbox_dir {
        for prefix in SCRATCH_PREFIXES {
            let scratch = sandbox.join(prefix);
            if resolved == scratch || resolved.starts_with(&scratch) {
                return true;
            }
        }
    }
    false
}

/// Shared Python file-mutation detector used by both [`HashlineEditBypassPolicy`]
/// and [`crate::WorkspaceWriteLeasePolicy`].
pub fn python_looks_mutating(source: &str) -> bool {
    // Directory/removal/subprocess calls that still mutate a leased workspace tree.
    const EXTRA_WORKSPACE_PATTERNS: &[&str] = &[
        "os.remove(",
        "os.unlink(",
        "os.mkdir(",
        "os.makedirs(",
        "os.rmdir(",
        "shutil.rmtree(",
        "Path.unlink(",
        ".unlink(",
        ".mkdir(",
        "subprocess.",
        "os.system(",
    ];
    if !python_file_write_targets(source).is_empty() {
        return true;
    }
    let stripped = strip_python_strings_and_comments(source);
    EXTRA_WORKSPACE_PATTERNS
        .iter()
        .any(|pat| stripped.contains(pat))
}

/// Shared JS/TS file-mutation detector used by both [`HashlineEditBypassPolicy`]
/// and [`crate::WorkspaceWriteLeasePolicy`].
pub fn js_looks_mutating(source: &str) -> bool {
    const EXTRA_WORKSPACE_PATTERNS: &[&str] = &["unlink", "rmSync", "rmdir", "mkdir"];
    if !js_file_write_targets(source).is_empty() {
        return true;
    }
    let stripped = strip_c_style_strings_and_comments(source);
    EXTRA_WORKSPACE_PATTERNS
        .iter()
        .any(|pat| stripped.contains(pat))
}

/// Extracts file targets from Python file-writing / file-replacing operations.
/// Each entry is `Some(path)` when a literal path (or single-assignment string/
/// `Path` variable) can be resolved, or `None` for a dynamic target.
pub fn python_file_write_targets(source: &str) -> Vec<Option<String>> {
    let bindings = collect_python_path_bindings(source);
    let has_non_stdio_write_method = has_python_file_write_method(source);
    let mut targets = Vec::new();

    for call in scan_calls(source, CommentStyle::Hash) {
        if call.name == "open" {
            let args = split_top_level_args(call.args);
            let is_method = call.dot_receiver.is_some();
            if python_open_is_mutating(&args, is_method, has_non_stdio_write_method) {
                let target = call.dot_receiver.map_or_else(
                    || {
                        python_kwarg(&args, "file")
                            .or_else(|| args.first().copied())
                            .and_then(|expr| resolve_python_path_expr(expr, &bindings))
                    },
                    |recv| resolve_python_path_expr(recv, &bindings),
                );
                targets.push(target);
            }
            continue;
        }
        if matches!(call.name, "write_text" | "write_bytes") && call.dot_receiver.is_some() {
            let target = call
                .dot_receiver
                .and_then(|recv| resolve_python_path_expr(recv, &bindings));
            targets.push(target);
            continue;
        }
        if matches!(call.name, "replace" | "rename")
            && let Some(recv) = call.dot_receiver
            && (recv == "os" || looks_like_python_path_receiver(recv, &bindings))
        {
            let args = split_top_level_args(call.args);
            let dest_index = usize::from(recv == "os");
            let target = args
                .get(dest_index)
                .and_then(|expr| resolve_python_path_expr(expr, &bindings));
            targets.push(target);
            continue;
        }
        if call.name == "truncate" && call.dot_receiver == Some("os") {
            let args = split_top_level_args(call.args);
            let target = args
                .first()
                .and_then(|expr| resolve_python_path_expr(expr, &bindings));
            targets.push(target);
            continue;
        }
        if call.dot_receiver == Some("shutil")
            && (call.name.starts_with("copy") || call.name == "move")
        {
            let args = split_top_level_args(call.args);
            let target = args
                .get(1)
                .and_then(|expr| resolve_python_path_expr(expr, &bindings));
            targets.push(target);
            continue;
        }
        if call.dot_receiver == Some("fileinput")
            && call.name == "input"
            && call.args.contains("inplace")
        {
            targets.push(None);
        }
    }
    targets
}

/// Extracts file targets from JavaScript/TypeScript file-writing operations.
pub fn js_file_write_targets(source: &str) -> Vec<Option<String>> {
    let mut targets = Vec::new();
    for call in scan_calls(source, CommentStyle::Slash) {
        let args = split_top_level_args(call.args);
        match call.name {
            "writeFile" | "writeFileSync" | "appendFile" | "appendFileSync" | "writeTextFile"
            | "writeTextFileSync" | "truncate" | "truncateSync" => {
                targets.push(args.first().and_then(|expr| unquote_literal(expr)));
            }
            "write" if call.dot_receiver == Some("Bun") => {
                targets.push(args.first().and_then(|expr| unquote_literal(expr)));
            }
            "copyFile" | "copyFileSync" | "rename" | "renameSync" => {
                targets.push(args.get(1).and_then(|expr| unquote_literal(expr)));
            }
            _ => {}
        }
    }
    targets
}

/// Extracts file targets from Ruby inline/heredoc file-writing operations.
fn ruby_file_write_targets(source: &str) -> Vec<Option<String>> {
    let mut targets = Vec::new();
    for call in scan_calls(source, CommentStyle::Hash) {
        let args = split_top_level_args(call.args);
        match (call.dot_receiver, call.name) {
            (Some("File" | "IO"), "write" | "binwrite" | "truncate") => {
                targets.push(args.first().and_then(|expr| unquote_literal(expr)));
            }
            (Some("File" | "IO"), "open") => {
                let is_mutating = args
                    .get(1)
                    .and_then(|arg| unquote_literal(arg))
                    .is_some_and(|mode| is_mutating_open_mode(&mode));
                if is_mutating {
                    targets.push(args.first().and_then(|expr| unquote_literal(expr)));
                }
            }
            (Some("FileUtils"), "cp" | "mv" | "copy" | "move") | (Some("File"), "rename") => {
                targets.push(args.get(1).and_then(|expr| unquote_literal(expr)));
            }
            _ => {}
        }
    }
    targets
}

fn python_open_is_mutating(args: &[&str], is_method: bool, has_write_method: bool) -> bool {
    let mode_arg = python_kwarg(args, "mode").or_else(|| {
        if is_method {
            args.first().copied()
        } else {
            args.get(1).copied()
        }
    });
    mode_arg.is_some_and(|expr| {
        unquote_literal(expr).map_or(has_write_method, |mode| is_mutating_open_mode(&mode))
    })
}

fn is_mutating_open_mode(mode: &str) -> bool {
    !mode.is_empty() && mode.bytes().any(|b| matches!(b, b'w' | b'a' | b'x' | b'+'))
}

fn has_python_file_write_method(source: &str) -> bool {
    scan_calls(source, CommentStyle::Hash)
        .into_iter()
        .any(|call| {
            matches!(call.name, "write" | "writelines" | "truncate")
                && !matches!(
                    call.dot_receiver,
                    Some("stdout" | "stderr" | "sys.stdout" | "sys.stderr")
                )
        })
}

fn python_kwarg<'a>(args: &[&'a str], key: &str) -> Option<&'a str> {
    for arg in args {
        if let Some((lhs, rhs)) = arg.split_once('=')
            && !rhs.starts_with('=')
            && lhs.trim() == key
        {
            return Some(rhs.trim());
        }
    }
    None
}

fn collect_python_path_bindings(source: &str) -> HashMap<String, Option<String>> {
    let mut bindings: HashMap<String, Option<String>> = HashMap::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = trimmed.split_once('=') else {
            continue;
        };
        if rhs.starts_with('=') {
            continue;
        }
        let name = lhs.trim();
        if !is_ident(name) {
            continue;
        }
        // Strip trailing inline comment outside quotes when inspecting the RHS.
        let rhs = strip_trailing_hash_comment(rhs.trim());
        let resolved = extract_direct_python_path(rhs);
        bindings
            .entry(name.to_string())
            .and_modify(|existing| {
                if *existing != resolved {
                    *existing = None;
                }
            })
            .or_insert(resolved);
    }
    bindings
}

fn strip_trailing_hash_comment(expr: &str) -> &str {
    let bytes = expr.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if b == b'\\' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'#' {
            return expr[..i].trim_end();
        }
        i += 1;
    }
    expr
}

fn extract_direct_python_path(expr: &str) -> Option<String> {
    let expr = expr.trim();
    if let Some(lit) = unquote_literal(expr) {
        return Some(lit);
    }
    for prefix in ["Path(", "pathlib.Path("] {
        if let Some(rest) = expr.strip_prefix(prefix)
            && let Some(inner) = rest.strip_suffix(')')
        {
            let args = split_top_level_args(inner);
            if args.len() == 1 {
                return unquote_literal(args[0]);
            }
        }
    }
    None
}

fn resolve_python_path_expr(
    expr: &str,
    bindings: &HashMap<String, Option<String>>,
) -> Option<String> {
    let expr = expr.trim();
    if let Some(direct) = extract_direct_python_path(expr) {
        return Some(direct);
    }
    if is_ident(expr) {
        return bindings.get(expr).cloned().flatten();
    }
    for prefix in ["Path(", "pathlib.Path("] {
        if let Some(rest) = expr.strip_prefix(prefix)
            && let Some(inner) = rest.strip_suffix(')')
        {
            let args = split_top_level_args(inner);
            if args.len() == 1 {
                return resolve_python_path_expr(args[0], bindings);
            }
        }
    }
    None
}

fn looks_like_python_path_receiver(recv: &str, bindings: &HashMap<String, Option<String>>) -> bool {
    let recv = recv.trim();
    recv.starts_with("Path(")
        || recv.starts_with("pathlib.Path(")
        || bindings.get(recv).is_some_and(Option::is_some)
}

fn unquote_literal(expr: &str) -> Option<String> {
    let expr = expr.trim();
    // Handle Python raw strings `r"..."` / `r'...'`.
    let expr = expr
        .strip_prefix('r')
        .or_else(|| expr.strip_prefix('R'))
        .filter(|rest| rest.starts_with('"') || rest.starts_with('\''))
        .unwrap_or(expr);
    let quote = expr.bytes().next()?;
    if !matches!(quote, b'"' | b'\'') || expr.len() < 2 || expr.as_bytes().last() != Some(&quote) {
        return None;
    }
    let inner = &expr[1..expr.len() - 1];
    // Reject expressions with unescaped internal quotes like `"a" + "b"`.
    let mut escaped = false;
    for b in inner.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' {
            escaped = true;
            continue;
        }
        if b == quote {
            return None;
        }
    }
    Some(inner.to_string())
}

fn is_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[derive(Clone, Copy)]
enum CommentStyle {
    Hash,
    Slash,
}

struct ParsedCall<'a> {
    dot_receiver: Option<&'a str>,
    name: &'a str,
    args: &'a str,
}

/// Scans function/method calls `name(args)` outside string literals and comments.
fn scan_calls(source: &str, comments: CommentStyle) -> Vec<ParsedCall<'_>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, i, comments) {
            i = next;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let name_start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name_end = i;
            let mut open = i;
            while open < bytes.len() && matches!(bytes[open], b' ' | b'\t') {
                open += 1;
            }
            if open < bytes.len()
                && bytes[open] == b'('
                && let Some(close) = find_matching_paren(bytes, open, comments)
            {
                let dot_receiver = extract_dot_receiver(source, name_start);
                out.push(ParsedCall {
                    dot_receiver,
                    name: &source[name_start..name_end],
                    args: &source[open + 1..close],
                });
            }
            continue;
        }
        i += 1;
    }
    out
}

fn extract_dot_receiver(source: &str, name_start: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut dot = name_start;
    while dot > 0 && matches!(bytes[dot - 1], b' ' | b'\t') {
        dot -= 1;
    }
    if dot == 0 || bytes[dot - 1] != b'.' {
        return None;
    }
    let end = dot - 1;
    let mut start = end;
    let mut paren_depth = 0usize;
    let mut quote: Option<u8> = None;
    while start > 0 {
        let b = bytes[start - 1];
        if let Some(q) = quote {
            if b == q && (start < 2 || bytes[start - 2] != b'\\') {
                quote = None;
            }
            start -= 1;
            continue;
        }
        if paren_depth > 0 {
            match b {
                b'\'' | b'"' => quote = Some(b),
                b')' => paren_depth += 1,
                b'(' => paren_depth -= 1,
                _ => {}
            }
            start -= 1;
            continue;
        }
        if b == b')' {
            paren_depth += 1;
            start -= 1;
            continue;
        }
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' {
            start -= 1;
            continue;
        }
        break;
    }
    let recv = source[start..end].trim();
    (!recv.is_empty()).then_some(recv)
}

fn find_matching_paren(bytes: &[u8], open: usize, comments: CommentStyle) -> Option<usize> {
    let mut i = open + 1;
    let mut depth = 1usize;
    while i < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, i, comments) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_args(args: &str) -> Vec<&str> {
    let bytes = args.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_string(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                let piece = args[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn skip_comment_or_string(bytes: &[u8], i: usize, comments: CommentStyle) -> Option<usize> {
    if let Some(next) = skip_string(bytes, i) {
        return Some(next);
    }
    match comments {
        CommentStyle::Hash if bytes[i] == b'#' => {
            let mut end = i + 1;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            Some(end)
        }
        CommentStyle::Slash if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') => {
            let mut end = i + 2;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            Some(end)
        }
        CommentStyle::Slash if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') => {
            let mut end = i + 2;
            while end + 1 < bytes.len() && !(bytes[end] == b'*' && bytes[end + 1] == b'/') {
                end += 1;
            }
            Some((end + 2).min(bytes.len()))
        }
        _ => None,
    }
}

fn skip_string(bytes: &[u8], i: usize) -> Option<usize> {
    let q = bytes[i];
    if !matches!(q, b'\'' | b'"' | b'`') {
        return None;
    }
    // Python triple-quoted string `'''...'''` or `"""..."""`.
    if q != b'`' && bytes.get(i + 1) == Some(&q) && bytes.get(i + 2) == Some(&q) {
        let mut end = i + 3;
        while end + 2 < bytes.len() {
            if bytes[end] == b'\\' {
                end += 2;
                continue;
            }
            if bytes[end] == q && bytes[end + 1] == q && bytes[end + 2] == q {
                return Some(end + 3);
            }
            end += 1;
        }
        return Some(bytes.len());
    }
    let mut end = i + 1;
    while end < bytes.len() {
        if bytes[end] == b'\\' {
            end += 2;
            continue;
        }
        if bytes[end] == q {
            return Some(end + 1);
        }
        end += 1;
    }
    Some(bytes.len())
}

fn strip_python_strings_and_comments(source: &str) -> String {
    strip_code(source, CommentStyle::Hash)
}

fn strip_c_style_strings_and_comments(source: &str) -> String {
    strip_code(source, CommentStyle::Slash)
}

fn strip_code(source: &str, comments: CommentStyle) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, i, comments) {
            out.push(' ');
            i = next;
            continue;
        }
        let ch = source[i..].chars().next().expect("valid UTF-8");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
