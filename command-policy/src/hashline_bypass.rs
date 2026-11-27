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
use aither_sandbox::{
    Grants, Interpreter, ScriptCheck, ScriptDecision, ScriptDir, ScriptOrigin, ScriptPolicy,
};
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
    grants: Option<Grants>,
}

impl EditBypassNotices {
    /// Creates a notice store scoped to `sandbox_dir` so absolute paths inside
    /// the session's scratch subdirectories (`tmp/`, `files/`, `outputs/`,
    /// `artifacts/`, `data/`) are recognized as scratch.
    #[must_use]
    pub fn with_sandbox_dir(sandbox_dir: &Path) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            sandbox_dir: Some(Arc::new(canonical_tmp_path(sandbox_dir))),
            grants: None,
        }
    }

    /// Attaches live session [`Grants`] so approved writable directories — even
    /// when located under `/tmp` — are never misclassified as system temp scratch.
    #[must_use]
    pub fn with_grants(mut self, grants: Grants) -> Self {
        self.grants = Some(grants);
        self
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

    const fn grants(&self) -> Option<&Grants> {
        self.grants.as_ref()
    }
}

#[derive(Clone, Copy)]
struct PathScope<'a> {
    start: Option<&'a Path>,
    cwd: Option<&'a Path>,
    /// True when a container/remote command changed into an unmounted scratch
    /// directory (such as container `/tmp` or `/dev`) that cannot be statted
    /// on the host.
    unmounted_scratch: bool,
    sandbox_dir: Option<&'a Path>,
    grants: Option<&'a Grants>,
}

enum EffectiveCwd {
    /// Host path that can be resolved and statted directly.
    Host(PathBuf),
    /// Container or remote directory outside the host workspace mount.
    Unmounted { scratch: bool },
}

/// Resolves `cd_steps` starting from `start`, translating container workspace
/// paths back onto the host mount when inside the mounted workspace and
/// falling back to [`EffectiveCwd::Unmounted`] for container-only directories
/// (such as `/testbed`) that do not exist on the host.
fn resolve_effective_cwd(start: &ScriptDir, cd_steps: &[String]) -> EffectiveCwd {
    match start {
        ScriptDir::Host(dir) => EffectiveCwd::Host(apply_cd_steps(dir, cd_steps)),
        ScriptDir::Remote { .. } => EffectiveCwd::Unmounted { scratch: false },
        ScriptDir::Container { inside, host } => {
            let inside_path = normalize_path(Path::new(inside.as_str()));
            let host_path = normalize_path(host);
            let inside_cwd = apply_cd_steps(&inside_path, cd_steps);
            let (mount_inside, mount_host) = infer_container_mount_root(&inside_path, &host_path);
            inside_cwd.strip_prefix(&mount_inside).map_or_else(
                |_| {
                    let canon = canonical_tmp_path(&inside_cwd);
                    EffectiveCwd::Unmounted {
                        scratch: canon.starts_with("/dev") || is_under_system_temp(&canon),
                    }
                },
                |rel| EffectiveCwd::Host(normalize_path(&mount_host.join(rel))),
            )
        }
    }
}

/// Walks matching trailing path components backwards from `(inside, host)` to
/// recover the container workspace mount root (stopping before stripping the
/// container path down to `/`).
fn infer_container_mount_root(inside: &Path, host: &Path) -> (PathBuf, PathBuf) {
    let mut inside_parts: Vec<Component<'_>> = inside.components().collect();
    let mut host_parts: Vec<Component<'_>> = host.components().collect();
    while inside_parts.len() > 2 && host_parts.len() > 1 {
        let (Some(Component::Normal(in_last)), Some(Component::Normal(host_last))) =
            (inside_parts.last(), host_parts.last())
        else {
            break;
        };
        if in_last != host_last {
            break;
        }
        inside_parts.pop();
        host_parts.pop();
    }
    let mount_inside: PathBuf = inside_parts.into_iter().collect();
    let mount_host: PathBuf = host_parts.into_iter().collect();
    (mount_inside, mount_host)
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
        let grants = self.notices.grants();
        if script.interpreter == Interpreter::Python {
            let scope = PathScope {
                start,
                cwd: start,
                unmounted_scratch: false,
                sandbox_dir,
                grants,
            };
            return check_scripted_mutation(
                "python",
                &python_file_write_targets(script.script),
                scope,
            )
            .await;
        }
        let mut found = Vec::new();
        if let Some(actions) = inspect_shell_actions(script.script) {
            for action in &actions {
                let effective = resolve_effective_cwd(script.start, &action.cd_steps);
                let (cwd, unmounted_scratch) = match &effective {
                    EffectiveCwd::Host(path) => (Some(path.as_path()), false),
                    EffectiveCwd::Unmounted { scratch } => (None, *scratch),
                };
                let scope = PathScope {
                    start,
                    cwd,
                    unmounted_scratch,
                    sandbox_dir,
                    grants,
                };
                self.inspect_action(action, scope, &mut found).await;
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
        scope: PathScope<'_>,
        out: &mut Vec<EditBypassUsage>,
    ) {
        match &action.kind {
            ShellActionKind::OutputRedirect { target } => {
                if target_is_existing_non_scratch(target, scope).await {
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
                    for usage in check_scripted_mutation(&mechanism, &targets, scope).await {
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
                    for usage in check_scripted_mutation(&mechanism, &targets, scope).await {
                        push_unique(out, usage);
                    }
                    return;
                }
                match eff_program {
                    "tee" => {
                        for target in tee_targets(eff_args) {
                            if target_is_existing_non_scratch(target, scope).await {
                                push_unique(out, EditBypassUsage::new("tee", Some(target)));
                            }
                        }
                    }
                    "cp" | "mv" => {
                        for target in
                            cp_or_mv_overwritten_targets(eff_program, eff_args, scope.cwd).await
                        {
                            if target_is_existing_non_scratch(&target, scope).await {
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
                                && target_is_existing_non_scratch(target, scope).await
                            {
                                push_unique(out, EditBypassUsage::new("dd", Some(target)));
                            }
                        }
                    }
                    "truncate" => {
                        for target in truncate_targets(eff_args) {
                            if target_is_existing_non_scratch(target, scope).await {
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

/// Returns `"sed -i"` or `"sed --in-place"` when `args` invoke in-place editing
/// (stopping at `--` and skipping `-e`/`-f`/`--expression`/`--file` values).
pub fn sed_in_place_flag(args: &[String]) -> Option<&'static str> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if arg == "--in-place" || arg.starts_with("--in-place=") {
            return Some("sed --in-place");
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
                    return Some("sed -i");
                }
                if matches!(ch, 'e' | 'f') {
                    // Remainder of cluster is the expression/filename, not flags.
                    break;
                }
            }
        }
        i += 1;
    }
    None
}

fn sed_in_place_usage(args: &[String]) -> Option<EditBypassUsage> {
    let mechanism = sed_in_place_flag(args)?;
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
        if has_script_file_arg(args, &["-W", "-X", "-Q"]) {
            return None;
        }
        return Some((program.to_string(), python_file_write_targets(stdin)));
    }
    if matches!(program, "node" | "bun" | "deno") {
        if inline_flag_value(args, "-e").is_some() || inline_flag_value(args, "--eval").is_some() {
            return None;
        }
        if has_script_file_arg(
            args,
            &[
                "-r",
                "--require",
                "--import",
                "-C",
                "--conditions",
                "--env-file",
            ],
        ) {
            return None;
        }
        return Some((program.to_string(), js_file_write_targets(stdin)));
    }
    if program == "ruby" {
        if inline_flag_value(args, "-e").is_some()
            || has_script_file_arg(args, &["-I", "-r", "-C", "-E", "-F"])
        {
            return None;
        }
        return Some(("ruby".to_string(), ruby_file_write_targets(stdin)));
    }
    None
}

fn has_script_file_arg(args: &[String], value_flags: &[&str]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" || arg == "-" {
            return false;
        }
        if value_flags.contains(&arg) {
            i += 2;
            continue;
        }
        if !arg.starts_with('-') {
            return true;
        }
        i += 1;
    }
    false
}

async fn check_scripted_mutation(
    mechanism: &str,
    targets: &[Option<String>],
    scope: PathScope<'_>,
) -> Vec<EditBypassUsage> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for target in targets {
        if let Some(raw) = target {
            if is_scratch_or_allowed_path(raw, scope).await {
                continue;
            }
            if let Some(host_cwd) = scope.cwd {
                if path_exists_as_file(&resolve_path(host_cwd, raw)).await {
                    push_unique(&mut found, EditBypassUsage::new(mechanism, Some(raw)));
                }
            } else if !scope.unmounted_scratch || Path::new(raw).is_absolute() {
                push_unique(&mut found, EditBypassUsage::new(mechanism, Some(raw)));
            }
        } else {
            // Unknown/dynamic target: allow only when cwd itself is clearly scratch.
            if scope.unmounted_scratch {
                continue;
            }
            if let Some(dir) = scope.cwd
                && is_scratch_directory(dir, scope).await
            {
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

async fn target_is_existing_non_scratch(raw: &str, scope: PathScope<'_>) -> bool {
    if is_scratch_or_allowed_path(raw, scope).await {
        return false;
    }
    let Some(host_cwd) = scope.cwd else {
        return false;
    };
    path_exists_as_file(&resolve_path(host_cwd, raw)).await
}

async fn path_exists_as_file(path: &Path) -> bool {
    async_fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

fn canonical_tmp_path(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    if let Ok(rest) = normalized.strip_prefix("/private/tmp") {
        return Path::new("/tmp").join(rest);
    }
    if let Ok(rest) = normalized.strip_prefix("/private/var") {
        return Path::new("/var").join(rest);
    }
    normalized
}

fn is_system_temp_root(canon: &Path) -> bool {
    canon == Path::new("/tmp") || canon == Path::new("/var/tmp")
}

fn is_under_system_temp(canon: &Path) -> bool {
    canon.starts_with("/tmp") || canon.starts_with("/var/tmp")
}

async fn is_scratch_or_allowed_path(raw: &str, scope: PathScope<'_>) -> bool {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return true;
    }
    // Special devices and temp/tasks variables.
    if trimmed == "/dev/null"
        || trimmed.starts_with("/dev/")
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
    // Check resolved path against session sandbox scratch, active workspace roots,
    // and only then system temporary directories.
    let resolved = if path.is_absolute() {
        normalize_path(path)
    } else if let Some(host_cwd) = scope.cwd {
        resolve_path(host_cwd, trimmed)
    } else {
        return false;
    };
    is_scratch_directory(&resolved, scope).await
}

async fn is_scratch_directory(resolved: &Path, scope: PathScope<'_>) -> bool {
    let canon = canonical_tmp_path(resolved);
    if canon.starts_with("/dev") {
        return true;
    }
    // Evaluate `sandbox_dir` before any `/tmp` heuristic: on Linux (or when
    // `HOME` is inside `/tmp`), the session sandbox itself lives under `/tmp`
    // and only its explicit scratch subdirectories count as scratch.
    if let Some(sandbox) = scope.sandbox_dir
        && canon.starts_with(sandbox)
    {
        return SCRATCH_PREFIXES
            .iter()
            .any(|prefix| canon.starts_with(sandbox.join(prefix)));
    }
    // Approved writable/readable workspace directories (and the shell's
    // starting workspace root) may also reside under `/tmp`; edits inside them
    // are never scratch.
    if let Some(grants) = scope.grants {
        let snapshot = grants.snapshot();
        if snapshot
            .writable
            .iter()
            .chain(snapshot.readable.iter())
            .any(|dir| {
                let dir_canon = canonical_tmp_path(dir);
                !is_system_temp_root(&dir_canon) && canon.starts_with(&dir_canon)
            })
        {
            return false;
        }
    }
    if let Some(root) = scope.start {
        let root_canon = canonical_tmp_path(root);
        if !is_system_temp_root(&root_canon) && canon.starts_with(&root_canon) {
            return false;
        }
    }
    if !is_under_system_temp(&canon) {
        return false;
    }
    // A real git repository checked out under `/tmp` (for example in an
    // unsandboxed evaluation harness) is never scratch.
    !has_git_ancestor_under_temp(resolved).await
}

async fn has_git_ancestor_under_temp(path: &Path) -> bool {
    let mut current = path.parent();
    while let Some(dir) = current {
        let canon = canonical_tmp_path(dir);
        if !is_under_system_temp(&canon) || is_system_temp_root(&canon) {
            break;
        }
        if async_fs::metadata(dir.join(".git")).await.is_ok() {
            return true;
        }
        current = dir.parent();
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
            // `io.open`, `codecs.open`, `builtins.open` take `(file, mode, ...)`
            // just like builtin `open`, whereas `Path(...).open(mode)` takes `mode` first.
            let is_path_method = call
                .dot_receiver
                .is_some_and(|recv| !matches!(recv, "io" | "_io" | "codecs" | "builtins"));
            if python_open_is_mutating(&args, is_path_method, has_non_stdio_write_method) {
                let target = if is_path_method {
                    call.dot_receiver
                        .and_then(|recv| resolve_python_path_expr(recv, &bindings))
                } else {
                    python_kwarg(&args, "file")
                        .or_else(|| args.first().copied())
                        .and_then(|expr| resolve_python_path_expr(expr, &bindings))
                };
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
        {
            let args = split_top_level_args(call.args);
            // `os.replace(src, dst)` takes 2 args; `Path.replace(dst)` takes 1 arg
            // (unlike `str.replace(old, new)` which takes 2-3 args).
            if recv == "os" || (args.len() == 1 && looks_like_python_path_receiver(recv, &bindings))
            {
                let dest_index = usize::from(recv == "os");
                let target = args
                    .get(dest_index)
                    .and_then(|expr| resolve_python_path_expr(expr, &bindings));
                targets.push(target);
                continue;
            }
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

#[derive(Clone, PartialEq, Eq)]
struct PythonPathBinding {
    path: String,
    is_path_obj: bool,
}

fn collect_python_path_bindings(source: &str) -> HashMap<String, Option<PythonPathBinding>> {
    let mut bindings: HashMap<String, Option<PythonPathBinding>> = HashMap::new();
    for line in source.lines() {
        let stripped = strip_trailing_hash_comment(line.trim());
        if stripped.is_empty() {
            continue;
        }
        for stmt in split_top_level_semicolons(stripped) {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
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
            let resolved = extract_direct_python_binding(rhs.trim());
            bindings
                .entry(name.to_string())
                .and_modify(|existing| {
                    if *existing != resolved {
                        *existing = None;
                    }
                })
                .or_insert(resolved);
        }
    }
    bindings
}

fn split_top_level_semicolons(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_string(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&line[start..]);
    out
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

fn extract_direct_python_binding(expr: &str) -> Option<PythonPathBinding> {
    let expr = expr.trim();
    if let Some(lit) = unquote_literal(expr) {
        return Some(PythonPathBinding {
            path: lit,
            is_path_obj: false,
        });
    }
    for prefix in ["Path(", "pathlib.Path("] {
        if let Some(rest) = expr.strip_prefix(prefix)
            && let Some(inner) = rest.strip_suffix(')')
        {
            let args = split_top_level_args(inner);
            if args.len() == 1 {
                return unquote_literal(args[0]).map(|path| PythonPathBinding {
                    path,
                    is_path_obj: true,
                });
            }
        }
    }
    None
}

fn resolve_python_path_expr(
    expr: &str,
    bindings: &HashMap<String, Option<PythonPathBinding>>,
) -> Option<String> {
    let expr = expr.trim();
    if let Some(direct) = extract_direct_python_binding(expr) {
        return Some(direct.path);
    }
    if is_ident(expr) {
        return bindings
            .get(expr)
            .and_then(|entry| entry.as_ref().map(|binding| binding.path.clone()));
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

fn looks_like_python_path_receiver(
    recv: &str,
    bindings: &HashMap<String, Option<PythonPathBinding>>,
) -> bool {
    let recv = recv.trim();
    recv.starts_with("Path(")
        || recv.starts_with("pathlib.Path(")
        || bindings
            .get(recv)
            .and_then(Option::as_ref)
            .is_some_and(|binding| binding.is_path_obj)
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
