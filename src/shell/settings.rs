//! The names and descriptions the model reads for the shell's tools.

use std::borrow::Cow;
use std::collections::BTreeMap;

use aither_sandbox::{MonitorSettings, ShellToolSettings, ToolIdentity};
use askama::Template;

use super::ShellBudgets;

/// Name of the tool that runs a script.
pub const SHELL_TOOL: &str = "terminal";
/// Name of the tool that reads a job's output.
pub const READ_TOOL: &str = "terminal_read";
/// Name of the tool that writes to a job's stdin.
pub const INPUT_TOOL: &str = "terminal_input";
/// Name of the tool that kills a job.
pub const KILL_TOOL: &str = "terminal_kill";
/// Name of the tool that watches a source.
pub const MONITOR_TOOL: &str = "monitor";

#[derive(Template)]
#[template(path = "shell/tool_shell.txt", escape = "none")]
struct ShellDescription {
    read: &'static str,
    input: &'static str,
    kill: &'static str,
    timeout_cap_minutes: u64,
    foreground_secs: u64,
}

#[derive(Template)]
#[template(path = "shell/tool_monitor.txt", escape = "none")]
struct MonitorDescription {
    max_timeout_secs: u64,
}

#[derive(Template)]
#[template(path = "shell/tool_read.txt", escape = "none")]
struct ReadDescription;

#[derive(Template)]
#[template(path = "shell/tool_input.txt", escape = "none")]
struct InputDescription;

#[derive(Template)]
#[template(path = "shell/tool_kill.txt", escape = "none")]
struct KillDescription;

fn rendered(template: &impl Template) -> Cow<'static, str> {
    template
        .render()
        .expect("a shell tool description renders typed input")
        .into()
}

/// What the model reads about each argument any of the shell's tools takes.
fn arguments() -> BTreeMap<&'static str, Cow<'static, str>> {
    [
        (
            "description",
            "A short, human-readable explanation of what this command does. It is shown instead of the script's source.",
        ),
        ("script", "The source to run."),
        (
            "interpreter",
            "The interpreter the source runs under: `shell` (the default) or `python`, which runs the source from a file so tracebacks carry line numbers.",
        ),
        (
            "sandbox",
            "How the command runs: `sandboxed` (the default) inside the sandbox, or `unsandboxed` without it, after approval.",
        ),
        (
            "location",
            "Where the command runs: omitted or empty for the home environment, otherwise the name of a configured SSH server.",
        ),
        (
            "resolution",
            "The resolution media in the output is delivered at: `auto` (the default) keeps the source resolution; `low`, `medium` and `high` downscale to a bounding box; `native` keeps the original bytes.",
        ),
        (
            "max_lines",
            "The most output lines to include inline before the output is saved to a file. For a command detached with `background: true` or promoted past the foreground window, only the first `max_lines` are returned at once and the full output goes to a file. Omitted, the default.",
        ),
        (
            "raw",
            "When true, skip all output compression and return the verbatim stdout bytes, for checksums, binary protocols and diff inputs. Default false.",
        ),
        (
            "timeout",
            "The command's maximum runtime in seconds. Required. The command and its process tree are killed when it passes.",
        ),
        (
            "background",
            "Start the command detached and return at once with its task id instead of waiting in the foreground. Default false: the call waits up to the foreground window and promotes a still-running command to a background task.",
        ),
        (
            "task_id",
            "The task identifier returned when the command was backgrounded.",
        ),
        (
            "cursor",
            "The offset after which output is read; the previous read's next cursor. Default 0.",
        ),
        (
            "max_bytes",
            "The most bytes to return. Omitted, the default.",
        ),
        ("input", "The text written to the task's stdin."),
        (
            "append_newline",
            "Append a trailing newline before writing. Default true.",
        ),
        (
            "source",
            "What to watch: a command to run inside the sandbox, or a file to follow.",
        ),
        ("command", "The source text to run inside the sandbox."),
        (
            "file",
            "The path of the file to follow, resolved against the working directory when relative.",
        ),
        (
            "interval_secs",
            "Seconds between output collections; only non-empty increments are delivered. Omitted, the default.",
        ),
        ("filter", "A regex; only matching new lines are delivered."),
        ("stop_on", "A regex; the first matching new line ends the monitor."),
        (
            "kill_on_stop",
            "With `stop_on`, also kill the watched process on the match. Default false: the command keeps running detached.",
        ),
        (
            "stall_secs",
            "Report the run as stuck once if no new output arrives for this many seconds.",
        ),
        (
            "timeout_secs",
            "The monitor's maximum lifetime in seconds. Required.",
        ),
    ]
    .into_iter()
    .map(|(name, description)| (name, Cow::Borrowed(description)))
    .collect()
}

fn identity(name: &'static str, description: Cow<'static, str>) -> ToolIdentity {
    ToolIdentity {
        name: name.into(),
        description,
        arguments: arguments(),
    }
}

/// The identities of the shell tool and its three job tools, describing the
/// bounds `budgets` holds now.
#[must_use]
pub fn shell_tool_settings(budgets: &ShellBudgets) -> ShellToolSettings {
    let limits = budgets.limits();
    ShellToolSettings {
        shell: identity(
            SHELL_TOOL,
            rendered(&ShellDescription {
                read: READ_TOOL,
                input: INPUT_TOOL,
                kill: KILL_TOOL,
                timeout_cap_minutes: limits.timeout_cap.get().as_secs() / 60,
                foreground_secs: limits.foreground_wait.get().as_secs(),
            }),
        ),
        read: identity(READ_TOOL, rendered(&ReadDescription)),
        input: identity(INPUT_TOOL, rendered(&InputDescription)),
        kill: identity(KILL_TOOL, rendered(&KillDescription)),
    }
}

/// The identity and bounds of the `monitor` tool.
#[must_use]
pub fn monitor_settings(budgets: &ShellBudgets) -> MonitorSettings {
    MonitorSettings {
        identity: identity(
            MONITOR_TOOL,
            rendered(&MonitorDescription {
                max_timeout_secs: budgets.monitor_max_timeout().get().as_secs(),
            }),
        ),
        max_timeout: budgets.monitor_max_timeout(),
        default_interval: budgets.monitor_default_interval(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShellSettings;

    #[test]
    fn the_shell_description_names_the_bounds_in_force() {
        let budgets = ShellBudgets::of(&ShellSettings::default());
        let settings = shell_tool_settings(&budgets);
        let text = &settings.shell.description;
        assert!(text.contains("at most 120 minutes"), "{text}");
        assert!(text.contains("up to 10 seconds"), "{text}");
        assert!(text.contains("`terminal_read`"), "{text}");
    }

    #[test]
    fn the_shell_description_names_only_commands_the_shell_runs() {
        let budgets = ShellBudgets::of(&ShellSettings::default());
        let settings = shell_tool_settings(&budgets);
        let text = &settings.shell.description;
        // The managed shell refuses `grep`, and `less` is not in it.
        for absent in ["grep", "less"] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        assert!(text.contains("`rg`, `head` and `tail`"), "{text}");
        assert!(text.contains("`ask`"), "{text}");
        // Nothing addressed to the application embedding the tool, and no
        // restatement of the sandbox rule the permissions section states.
        for absent in ["host application", "sandbox"] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
    }
}
