//! Native terminal-command tool event parsing for streaming UI updates.

use aither_core::llm::{CommandOutcome, ToolResult};
use aither_sandbox::{TerminalArgs, TerminalExecutionMode};

use super::types::{
    ApiTerminalExecution, TERMINAL_TOOL_NAME, TerminalCommandMode, TerminalExecutionStatus,
    TerminalStateSnapshot, ToolEventRecord,
};

const fn parse_terminal_mode(mode: TerminalExecutionMode) -> TerminalCommandMode {
    match mode {
        TerminalExecutionMode::Default => TerminalCommandMode::Default,
        TerminalExecutionMode::Sandboxed => TerminalCommandMode::Sandboxed,
        TerminalExecutionMode::Unsafe => TerminalCommandMode::Unsafe,
        TerminalExecutionMode::Ssh => TerminalCommandMode::Ssh,
    }
}

pub(super) fn parse_terminal_start(
    arguments: &str,
) -> (String, String, Option<TerminalCommandMode>) {
    match serde_json::from_str::<TerminalArgs>(arguments) {
        Ok(parsed) => (
            parsed.description,
            parsed.script,
            Some(parse_terminal_mode(parsed.mode)),
        ),
        Err(error) => {
            tracing::warn!(
                error = %error,
                payload = %arguments,
                "terminal tool start payload did not satisfy TerminalArgs; surfacing as malformed call"
            );
            (String::new(), String::new(), None)
        }
    }
}

/// Text recorded as a terminal execution's output.
fn terminal_end_output(result: &ToolResult) -> String {
    result.render_for_model()
}

fn terminal_status_from_result(result: &ToolResult) -> TerminalExecutionStatus {
    if result.is_error() {
        return TerminalExecutionStatus::Failed;
    }
    match result.command().map(|command| &command.outcome) {
        Some(CommandOutcome::Killed { .. }) => TerminalExecutionStatus::Stopped,
        Some(CommandOutcome::Exited { code }) if *code != 0 => TerminalExecutionStatus::Failed,
        Some(CommandOutcome::Exited { .. } | CommandOutcome::Running { .. }) | None => {
            TerminalExecutionStatus::Done
        }
    }
}

pub(super) fn update_terminal_snapshot_from_tool_event(
    snapshot: &mut TerminalStateSnapshot,
    record: &ToolEventRecord,
) {
    match record {
        ToolEventRecord::Start {
            id,
            name,
            arguments,
        } => {
            if !name.eq_ignore_ascii_case(TERMINAL_TOOL_NAME) {
                return;
            }
            let (description, script, mode) = parse_terminal_start(arguments);
            let mode = mode.unwrap_or(TerminalCommandMode::Default);
            snapshot.running_by_call_id.insert(
                id.clone(),
                ApiTerminalExecution {
                    id: id.clone(),
                    description,
                    input: script,
                    output: None,
                    status: TerminalExecutionStatus::Running,
                    mode,
                },
            );
        }
        ToolEventRecord::End { id, name, result } => {
            if !name.eq_ignore_ascii_case(TERMINAL_TOOL_NAME) {
                return;
            }
            let mut exec = snapshot.running_by_call_id.remove(id).unwrap_or_else(|| {
                panic!("terminal tool end received without matching start: call_id={id}")
            });

            exec.output = Some(terminal_end_output(result));
            exec.status = terminal_status_from_result(result);

            snapshot.executions.push(exec);
        }
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::{CommandOutcome, CommandResult, CommandStream, ToolResult};

    use super::terminal_status_from_result;
    use crate::ui_server::types::TerminalExecutionStatus;

    fn command(outcome: CommandOutcome) -> ToolResult {
        ToolResult::Command(Box::new(CommandResult {
            stdout: CommandStream::Inline(String::new()),
            stderr: None,
            outcome,
        }))
    }

    #[test]
    fn terminal_status_follows_the_command_outcome() {
        assert_eq!(
            terminal_status_from_result(&command(CommandOutcome::Exited { code: 0 })),
            TerminalExecutionStatus::Done
        );
        assert_eq!(
            terminal_status_from_result(&command(CommandOutcome::Exited { code: 2 })),
            TerminalExecutionStatus::Failed
        );
        assert_eq!(
            terminal_status_from_result(&command(CommandOutcome::Killed {
                task_id: "t".to_string(),
                notice: "stopped".to_string(),
            })),
            TerminalExecutionStatus::Stopped
        );
        assert_eq!(
            terminal_status_from_result(&ToolResult::error("boom")),
            TerminalExecutionStatus::Failed
        );
    }
}
