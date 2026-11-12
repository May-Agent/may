//! `!` bash-mode execution shared by the TUI and the daemon.
//!
//! Runs a host command on the user's behalf — may's own environment plus
//! keychain secrets as env vars — and hands back a masked outcome safe
//! to display and to send to the agent.

use std::path::Path;

use crate::secrets::Secrets;

/// Masked result of a bash-mode command, ready for display and the agent.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashOutcome {
    /// Chat element id (`bash-*`) showing the run; the API leaves it out.
    #[serde(skip)]
    pub call_id: String,
    /// The command as typed (unmasked — the run already happened).
    pub command: String,
    /// Captured stdout (masked).
    pub stdout: String,
    /// Captured stderr (masked).
    pub stderr: String,
    /// Process exit code; `-1` when the signal had none.
    pub exit_code: i32,
    /// A secret was detected in the command or its output.
    pub had_secret: bool,
    /// Spawn/capture failure message, when the command never ran.
    pub error: Option<String>,
}

/// The text the agent receives for a bash-mode run: the command as shown
/// (`command`, secrets masked) and what it produced.
#[derive(Debug, askama::Template)]
#[template(path = "bash_outcome.txt", escape = "none")]
pub struct BashOutcomeMessage<'a> {
    command: &'a str,
    outcome: &'a BashOutcome,
}

impl<'a> BashOutcomeMessage<'a> {
    /// The agent's view of `outcome`, whose command was masked as `command`.
    #[must_use]
    pub const fn new(command: &'a str, outcome: &'a BashOutcome) -> Self {
        Self { command, outcome }
    }
}

/// Cap a captured output stream at `MAX` bytes on a char boundary.
fn cap_stream(bytes: &[u8]) -> String {
    const MAX: usize = 64 * 1024;
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX {
        return text.into_owned();
    }
    let end = text
        .char_indices()
        .nth(MAX)
        .map_or(text.len(), |(index, _)| index);
    format!("{}\n… (output truncated)", &text[..end])
}

/// Run a bash-mode (`!`) command in may's environment: `cwd` plus
/// keychain secrets injected as environment variables. Output is masked
/// before it returns.
#[must_use]
pub fn run_bash_command(command: &str, cwd: &Path, secrets: &Secrets) -> BashOutcome {
    let (_, command_secret) = secrets.mask(command);
    let spawn = || {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .envs(secrets.env_pairs())
            .output()
    };
    match spawn() {
        Err(error) => BashOutcome {
            call_id: String::new(),
            command: command.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            had_secret: command_secret,
            error: Some(format!("failed to run bash: {error}")),
        },
        Ok(output) => {
            let (stdout, out_secret) = secrets.mask(&cap_stream(&output.stdout));
            let (stderr, err_secret) = secrets.mask(&cap_stream(&output.stderr));
            // `had_secret` tracks output only — a command-side hit already
            // warned at submit time.
            BashOutcome {
                call_id: String::new(),
                command: command.to_string(),
                stdout,
                stderr,
                exit_code: output.status.code().unwrap_or(-1),
                had_secret: out_secret || err_secret,
                error: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use askama::Template as _;

    use super::{BashOutcome, BashOutcomeMessage};

    fn outcome(stdout: &str, stderr: &str, exit_code: i32, error: Option<&str>) -> BashOutcome {
        BashOutcome {
            call_id: String::new(),
            command: "ls".to_string(),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code,
            had_secret: false,
            error: error.map(str::to_string),
        }
    }

    fn message(outcome: &BashOutcome) -> String {
        BashOutcomeMessage::new("ls", outcome)
            .render()
            .expect("renders")
    }

    #[test]
    fn the_message_carries_each_stream_that_has_text() {
        assert_eq!(message(&outcome("", "", 0, None)), "! ls\n");
        assert_eq!(
            message(&outcome("a\nb", "warn", 2, None)),
            "! ls\n\n<stdout>\na\nb\n</stdout>\n<stderr>\nwarn\n</stderr>\n<exit>2</exit>"
        );
        assert_eq!(
            message(&outcome("ignored", "", 1, Some("no shell"))),
            "! ls\n\n<error>no shell</error>"
        );
    }
}
