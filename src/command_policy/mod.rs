//! May's rules on what a terminal script may run, as aither command
//! policies.
//!
//! Every script of a registry built with [`register`] is put to them before
//! it runs: one that kills processes by name or pattern, or is not valid
//! shell and so cannot be checked for that; a foreground `sleep`; a re-read
//! of files unchanged since their last read; and, in the managed bash, the
//! host `grep`, `awk` and `find` and the `sed` forms `aither_sed::supports`
//! does not accept. Each refusal names what to do instead.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use aither_sandbox::{
    CommandPolicy, PolicyDecision, PolicyRefusal, ScriptCheck, ScriptEnd, ScriptOrigin,
    ScriptOutcome, ShellCommand, ShellScript, ToolRegistryBuilder,
};
use askama::Template;

/// Puts every script `builder`'s terminals run to May's policies.
pub fn register(builder: &mut ToolRegistryBuilder) {
    builder.command_policy(KillByName);
    builder.command_policy(ForegroundSleep);
    builder.command_policy(UnchangedFileReads);
    // The managed bash runs every script but on Windows, where `cmd.exe`
    // runs them with the host's own programs.
    #[cfg(not(windows))]
    builder.command_policy(ManagedShellCommands);
}

fn refuse(template: &impl Template) -> PolicyDecision {
    PolicyDecision::Refuse(PolicyRefusal::new(
        template
            .render()
            .expect("a command policy template renders typed input"),
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

impl CommandPolicy for KillByName {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = PolicyDecision> + Send {
        let decision = match script.shell {
            None => PolicyDecision::Allow,
            Some(Err(error)) => refuse(&UnparsableRefused {
                reason: error.reason(),
            }),
            Some(Ok(shell)) => kill_by_name(shell).map_or(PolicyDecision::Allow, |program| {
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

impl CommandPolicy for ForegroundSleep {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = PolicyDecision> + Send {
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
            PolicyDecision::Allow
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
#[derive(Debug, Clone, Copy, Default)]
pub struct UnchangedFileReads;

/// The stamp of every existing file `reads` names, relative paths resolved
/// against `working_dir`.
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

impl CommandPolicy for UnchangedFileReads {
    type State = FileReads;

    async fn check(&self, script: &ScriptCheck<'_>, state: &FileReads) -> PolicyDecision {
        if script.origin == ScriptOrigin::Run {
            return PolicyDecision::Allow;
        }
        let Some(shell) = parsed(script) else {
            return PolicyDecision::Allow;
        };
        let reads = file_reads(shell);
        if reads.is_empty() {
            return PolicyDecision::Allow;
        }
        let stamps = fingerprints(script.working_dir, &reads).await;
        if stamps.is_empty() {
            return PolicyDecision::Allow;
        }
        let unchanged = {
            let inner = state.inner();
            stamps
                .iter()
                .all(|(path, stamp)| inner.recorded.get(path) == Some(stamp))
        };
        if unchanged {
            let paths: Vec<&Path> = stamps.iter().map(|(path, _)| path.as_path()).collect();
            return PolicyDecision::Refuse(PolicyRefusal::unchanged_read(
                UnchangedFileReadRefused { paths: &paths }
                    .render()
                    .expect("the unchanged file read template renders typed input"),
            ));
        }
        state
            .inner()
            .pending
            .insert(script.task_id.to_string(), stamps);
        PolicyDecision::Allow
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
fn host_command(script: &ShellScript) -> Option<PolicyDecision> {
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

impl CommandPolicy for ManagedShellCommands {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = PolicyDecision> + Send {
        std::future::ready(
            parsed(script)
                .and_then(host_command)
                .unwrap_or(PolicyDecision::Allow),
        )
    }
}

#[cfg(test)]
mod tests;
