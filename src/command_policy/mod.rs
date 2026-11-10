//! May's rules on what a terminal script may run, as aither script policies.
//!
//! Every script of a shell built with [`policies`] is put to them before it
//! runs: one that kills processes by name or pattern, or is not valid shell
//! and so cannot be checked for that; a foreground `sleep`; a re-read of
//! files unchanged since their last read; and, in the managed bash, the host
//! `grep`, `awk` and `find` and the `sed` forms `aither_sed::supports` does
//! not accept. Each refusal names what to do instead. None of them approves
//! a script that leaves the sandbox: each [passes](ScriptDecision::Pass) one
//! it has no objection to.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use aither_core::llm::tool::CallIdentity;
use aither_sandbox::{
    AnyScriptPolicy, ScriptCheck, ScriptDecision, ScriptEnd, ScriptOrigin, ScriptOutcome,
    ScriptPolicy, ScriptRefusal, ShellCommand, ShellScript,
};
use askama::Template;

/// Whether the scripts of a session run in the managed bash: always inside a
/// container, and on the host everywhere but Windows, where `cmd.exe` runs
/// them with the host's own programs.
#[must_use]
pub const fn runs_managed_bash(in_container: bool) -> bool {
    in_container || cfg!(not(windows))
}

/// The calls the unchanged-read policy refused, for the agent's turn
/// boundary to tell a turn made only of repeated reads from one that
/// learned something. Every clone shares the record.
#[derive(Debug, Clone, Default)]
pub struct RefusedReads(Arc<Mutex<HashSet<CallIdentity>>>);

impl RefusedReads {
    fn record(&self, call: CallIdentity) {
        self.0
            .lock()
            .expect("the refused read record is never held across a panic")
            .insert(call);
    }

    /// Whether `call` was refused as an unchanged re-read; each call is
    /// answered once.
    #[must_use]
    pub fn take(&self, call: &CallIdentity) -> bool {
        self.0
            .lock()
            .expect("the refused read record is never held across a panic")
            .remove(call)
    }
}

/// May's policies for the shell of one agent; `managed_bash` is
/// [`runs_managed_bash`] of the session, and `refused` the record the
/// agent's turn boundary reads.
#[must_use]
pub fn policies(managed_bash: bool, refused: RefusedReads) -> Vec<AnyScriptPolicy> {
    let mut policies = vec![
        AnyScriptPolicy::new(KillByName),
        AnyScriptPolicy::new(ForegroundSleep),
        AnyScriptPolicy::new(UnchangedFileReads { refused }),
        AnyScriptPolicy::new(PythonWithUv),
    ];
    if managed_bash {
        policies.push(AnyScriptPolicy::new(ManagedShellCommands));
    }
    policies
}

fn refuse(template: &impl Template) -> ScriptDecision {
    ScriptDecision::Refuse(ScriptRefusal::new(
        template
            .render()
            .expect("a script policy template renders typed input"),
    ))
}

/// The script parsed as shell; `None` for a script in another language or
/// one that is not valid shell.
const fn parsed<'a>(script: &ScriptCheck<'a>) -> Option<&'a ShellScript> {
    match script.shell {
        Some(Ok(shell)) => Some(shell),
        Some(Err(_)) | None => None,
    }
}

#[derive(Template)]
#[template(path = "refused_kill_by_name.txt")]
struct KillByNameRefused<'a> {
    program: &'a str,
}

#[derive(Template)]
#[template(path = "refused_unparsable_command.txt")]
struct UnparsableRefused<'a> {
    reason: &'a str,
}

/// Refuses a script that signals processes selected by name or pattern,
/// which may hit processes the session did not start, and a shell script
/// that does not parse, which cannot be checked for that.
#[derive(Debug, Clone, Copy, Default)]
pub struct KillByName;

/// Programs that signal processes selected by name, user or other pattern.
const KILLERS: &[&str] = &["pkill", "killall", "killall5", "skill"];

/// The program that kills by name or pattern in `script`, if any.
fn kill_by_name(script: &ShellScript) -> Option<String> {
    for command in script.commands() {
        let arguments = command.arguments();
        let by_name = match command.program() {
            "fuser" => arguments.iter().any(|argument| {
                argument == "--kill"
                    || argument
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.starts_with('-') && flags.contains('k'))
            }),
            "taskkill" => arguments.iter().any(|argument| {
                let selector = argument.trim_start_matches(['/', '-']).to_ascii_lowercase();
                argument.starts_with(['/', '-']) && matches!(selector.as_str(), "im" | "fi")
            }),
            name => KILLERS.contains(&name),
        };
        if by_name {
            return Some(command.program().to_string());
        }
    }
    let lookups = script
        .commands()
        .iter()
        .filter(|command| matches!(command.program(), "pgrep" | "pidof"));
    for lookup in lookups {
        // `kill $(pgrep NAME)`.
        let substituted_into_kill = lookup
            .substituted_into()
            .is_some_and(|line| script.line(line).any(|command| command.program() == "kill"));
        // `pgrep NAME | xargs kill`.
        let piped_into_kill = lookup.pipeline().is_some_and(|producer| {
            script.commands().iter().any(|command| {
                command.program() == "kill"
                    && command.pipeline().is_some_and(|consumer| {
                        consumer.pipeline == producer.pipeline && consumer.stage > producer.stage
                    })
                    && script
                        .wrappers(command)
                        .any(|wrapper| wrapper.program() == "xargs")
            })
        });
        if substituted_into_kill || piped_into_kill {
            return Some("kill of pgrep/pidof output".to_string());
        }
    }
    None
}

impl ScriptPolicy for KillByName {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let decision = match script.shell {
            None => ScriptDecision::Pass,
            Some(Err(error)) => refuse(&UnparsableRefused {
                reason: error.reason(),
            }),
            Some(Ok(shell)) => kill_by_name(shell).map_or(ScriptDecision::Pass, |program| {
                refuse(&KillByNameRefused { program: &program })
            }),
        };
        std::future::ready(decision)
    }
}

#[derive(Template)]
#[template(path = "refused_sleep.txt")]
struct SleepRefused;

/// Refuses a foreground `terminal` script that runs `sleep` where the
/// script waits on it: it only serves polling. A detached script
/// (`background: true`) may sleep, as a timer or while it waits on a
/// condition.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForegroundSleep;

impl ScriptPolicy for ForegroundSleep {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let sleeps = script.origin == ScriptOrigin::Foreground
            && parsed(script).is_some_and(|shell| {
                shell
                    .commands()
                    .iter()
                    .any(|command| !command.is_background() && command.program() == "sleep")
            });
        std::future::ready(if sleeps {
            refuse(&SleepRefused)
        } else {
            ScriptDecision::Pass
        })
    }
}

/// Programs that print a file's content. `tail -f` follows instead of
/// re-reading and is not collected.
const READERS: &[&str] = &["cat", "head", "tail", "less", "more", "bat", "view"];

/// The words `script` reads files from: the positional words of every
/// reader, `tail`'s follow forms excluded. Words that name no file are
/// dropped when the caller finds no file under them.
fn file_reads(script: &ShellScript) -> Vec<&str> {
    let mut reads = Vec::new();
    for command in script.commands() {
        if !READERS.contains(&command.program()) {
            continue;
        }
        let follows = command.program() == "tail"
            && command.arguments().iter().any(|argument| {
                argument == "-f"
                    || argument == "-F"
                    || argument.starts_with("--follow")
                    || argument
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.starts_with('-') && flags.contains('f'))
            });
        if !follows {
            reads.extend(
                command
                    .arguments()
                    .iter()
                    .filter(|argument| !argument.starts_with('-'))
                    .map(String::as_str),
            );
        }
    }
    reads
}

/// A file's `(len, modified)` stamp as a read found it: two reads whose
/// stamps match return the same bytes.
type Fingerprint = (u64, Option<SystemTime>);

/// What one agent's file reads returned with.
#[derive(Debug, Default)]
pub struct FileReads {
    inner: Mutex<FileReadsInner>,
}

#[derive(Debug, Default)]
struct FileReadsInner {
    /// Path → the stamp its last successful read returned with.
    recorded: HashMap<PathBuf, Fingerprint>,
    /// Task id → the stamps its script's reads were admitted with.
    pending: HashMap<String, Vec<(PathBuf, Fingerprint)>>,
}

impl FileReads {
    fn inner(&self) -> std::sync::MutexGuard<'_, FileReadsInner> {
        self.inner
            .lock()
            .expect("the file read record is never held across a panic")
    }
}

#[derive(Template)]
#[template(path = "refused_unchanged_file_read.txt")]
struct UnchangedFileReadRefused<'a> {
    paths: &'a [&'a Path],
}

/// Refuses a `terminal` script whose every file read is of a file unchanged
/// since its last successful read: re-reading it spends a turn for nothing,
/// and only a background event can say something new.
#[derive(Debug, Clone)]
pub struct UnchangedFileReads {
    refused: RefusedReads,
}

/// The stamp of every existing file `reads` names, relative paths resolved
/// against `working_dir`, the directory the script starts in.
async fn fingerprints(working_dir: &Path, reads: &[&str]) -> Vec<(PathBuf, Fingerprint)> {
    let stamps = futures_util::future::join_all(reads.iter().map(|read| async move {
        let path = working_dir.join(read);
        match async_fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => Some((path, (meta.len(), meta.modified().ok()))),
            Ok(_) | Err(_) => None,
        }
    }))
    .await;
    stamps.into_iter().flatten().collect()
}

impl ScriptPolicy for UnchangedFileReads {
    type State = FileReads;

    async fn check(&self, script: &ScriptCheck<'_>, state: &FileReads) -> ScriptDecision {
        if script.origin == ScriptOrigin::Run {
            return ScriptDecision::Pass;
        }
        let Some(shell) = parsed(script) else {
            return ScriptDecision::Pass;
        };
        // A script on a server reads files this host cannot stamp.
        let Some(start) = script.start.host() else {
            return ScriptDecision::Pass;
        };
        let reads = file_reads(shell);
        if reads.is_empty() {
            return ScriptDecision::Pass;
        }
        let stamps = fingerprints(start, &reads).await;
        if stamps.is_empty() {
            return ScriptDecision::Pass;
        }
        let unchanged = {
            let inner = state.inner();
            stamps
                .iter()
                .all(|(path, stamp)| inner.recorded.get(path) == Some(stamp))
        };
        if unchanged {
            let paths: Vec<&Path> = stamps.iter().map(|(path, _)| path.as_path()).collect();
            if let Some(call) = script.call {
                self.refused.record(call.clone());
            }
            return refuse(&UnchangedFileReadRefused { paths: &paths });
        }
        state
            .inner()
            .pending
            .insert(script.task_id.to_string(), stamps);
        ScriptDecision::Pass
    }

    fn ended(&self, end: &ScriptEnd<'_>, state: &FileReads) -> impl Future<Output = ()> + Send {
        let stamps = state.inner().pending.remove(end.task_id);
        // Only a read that succeeded counts: a denied or failed read must
        // run again and show its failure.
        if let Some(stamps) = stamps
            && end.outcome == (ScriptOutcome::Exited { code: 0 })
        {
            state.inner().recorded.extend(stamps);
        }
        std::future::ready(())
    }
}

#[derive(Template)]
#[template(path = "refused_python_setup.txt")]
struct PythonSetupRefused<'a> {
    command: &'a str,
    replacement: &'a str,
}

/// Refuses a script that installs Python packages with pip or creates a
/// virtual environment with `venv` or `virtualenv`: uv does both, and the
/// managed interpreter refuses a bare `pip install` anyway.
#[derive(Debug, Clone, Copy, Default)]
pub struct PythonWithUv;

const PIP_INSTALL_WITH_UV: &str =
    "`uv add <package>` in a project or `uv pip install <package>` in an environment";
const VENV_WITH_UV: &str = "`uv venv`";

/// Whether `version` is empty or a version suffix such as `3` or `3.13`.
fn is_version_suffix(version: &str) -> bool {
    version
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

/// Whether `program` is `pip`, `pip3` or a versioned `pip3.13`.
fn is_pip(program: &str) -> bool {
    program.strip_prefix("pip").is_some_and(is_version_suffix)
}

/// Whether `program` is `python`, `python3` or a versioned `python3.13`.
fn is_python(program: &str) -> bool {
    program
        .strip_prefix("python")
        .is_some_and(is_version_suffix)
}

/// The first word of `arguments` that is not an option.
fn subcommand(arguments: &[String]) -> Option<&str> {
    arguments
        .iter()
        .map(String::as_str)
        .find(|argument| !argument.starts_with('-'))
}

/// The refused form `command` is, with the uv command that replaces it.
fn python_setup(command: &ShellCommand) -> Option<(String, &'static str)> {
    let program = command.program();
    let arguments = command.arguments();
    if is_pip(program) {
        return (subcommand(arguments) == Some("install"))
            .then(|| (format!("{program} install"), PIP_INSTALL_WITH_UV));
    }
    if program == "virtualenv" {
        return Some((program.to_string(), VENV_WITH_UV));
    }
    if !is_python(program) {
        return None;
    }
    let module = arguments.iter().position(|argument| argument == "-m")? + 1;
    let module_name = arguments.get(module)?.as_str();
    match module_name {
        "pip" => (subcommand(&arguments[module + 1..]) == Some("install"))
            .then(|| (format!("{program} -m pip install"), PIP_INSTALL_WITH_UV)),
        "venv" | "virtualenv" => Some((format!("{program} -m {module_name}"), VENV_WITH_UV)),
        _ => None,
    }
}

impl ScriptPolicy for PythonWithUv {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let decision = parsed(script)
            .and_then(|shell| shell.commands().iter().find_map(python_setup))
            .map_or(ScriptDecision::Pass, |(command, replacement)| {
                refuse(&PythonSetupRefused {
                    command: &command,
                    replacement,
                })
            });
        std::future::ready(decision)
    }
}

#[derive(Template)]
#[template(path = "refused_host_command.txt")]
struct HostCommandRefused<'a> {
    name: &'a str,
    replacement: &'a str,
}

#[derive(Template)]
#[template(path = "refused_sed.txt")]
struct SedRefused<'a> {
    unsupported: &'a aither_sed::Unsupported,
}

/// Host programs a script's own shell does not run, each with what to use
/// instead.
const HOST_COMMANDS: [(&str, &str); 3] = [
    ("grep", "`rg`"),
    ("awk", "`python3` or `jq`"),
    ("find", "`fd`"),
];

/// Refuses a script whose own shell runs `grep`, `awk` or `find`, or `sed`
/// in a form `aither_sed::supports` does not accept (the line ranges); the
/// allowed forms run the host `sed`.
///
/// The rule binds the script's own shell, the way shell functions would: a
/// command it runs by name, through `time` or `eval` included. A program
/// named by path, run through a wrapper such as `env`, `xargs` or
/// `command`, or by another shell (`sh -c`, a script file) is the host's.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManagedShellCommands;

/// Whether `script`'s own shell runs `command` by its name.
fn run_by_own_shell(script: &ShellScript, command: &ShellCommand) -> bool {
    let mut current = command;
    loop {
        let runner = match (current.wrapper(), current.interpreter()) {
            (Some(wrapper), _) => (script.get(wrapper), "time"),
            (None, Some(interpreter)) => (script.get(interpreter), "eval"),
            (None, None) => return true,
        };
        if runner.0.program() != runner.1 {
            return false;
        }
        current = runner.0;
    }
}

/// The refusal of the first command `script`'s own shell runs that the
/// managed bash does not provide.
fn host_command(script: &ShellScript) -> Option<ScriptDecision> {
    script.commands().iter().find_map(|command| {
        let name = command.argv().first()?.as_str();
        if !run_by_own_shell(script, command) {
            return None;
        }
        if name == "sed" {
            let arguments = command.arguments().iter().map(OsString::from).collect();
            return aither_sed::supports(arguments).err().map(|unsupported| {
                refuse(&SedRefused {
                    unsupported: &unsupported,
                })
            });
        }
        HOST_COMMANDS
            .iter()
            .find(|(blocked, _)| *blocked == name)
            .map(|&(name, replacement)| refuse(&HostCommandRefused { name, replacement }))
    })
}

impl ScriptPolicy for ManagedShellCommands {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        std::future::ready(
            parsed(script)
                .and_then(host_command)
                .unwrap_or(ScriptDecision::Pass),
        )
    }
}

#[cfg(test)]
mod tests;
