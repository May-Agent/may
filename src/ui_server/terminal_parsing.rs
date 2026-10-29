//! Native terminal-command tool event parsing for streaming UI updates.

use aither_core::llm::{ToolResult, tool::CommandOutcome};
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

/// Terminal output shown in the snapshot: the prose the model read.
fn parse_terminal_end_output_from_result(result: &ToolResult) -> String {
    result.render_for_model()
}

fn parse_terminal_status_from_result(result: &ToolResult) -> TerminalExecutionStatus {
    if result.is_error() {
        return TerminalExecutionStatus::Failed;
    }
    match result.command().map(|command| &command.outcome) {
        // A foreground run killed at its time bound failed; a user stop never yields `Killed`.
        Some(CommandOutcome::Killed { .. }) => TerminalExecutionStatus::Failed,
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

            exec.output = Some(parse_terminal_end_output_from_result(result));
            exec.status = parse_terminal_status_from_result(result);

            snapshot.executions.push(exec);
        }
        ToolEventRecord::Stopped { id, name } => {
            if !name.eq_ignore_ascii_case(TERMINAL_TOOL_NAME) {
                return;
            }
            let mut exec = snapshot.running_by_call_id.remove(id).unwrap_or_else(|| {
                panic!("terminal tool stop received without matching start: call_id={id}")
            });
            exec.output = Some(ToolEventRecord::stopped_result().render_for_model());
            exec.status = TerminalExecutionStatus::Stopped;
            snapshot.executions.push(exec);
        }
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::tool::{CommandOutcome, CommandResult, CommandStream, StoredFile};

    use super::*;

    fn command(stdout: CommandStream, outcome: CommandOutcome) -> ToolResult {
        ToolResult::Command(Box::new(CommandResult {
            stdout,
            stderr: None,
            outcome,
        }))
    }

    /// Terminal results are persisted as tool-event records, so every stream
    /// shape must survive a serde round trip.
    #[test]
    fn command_results_round_trip_through_tool_event_records() {
        let file = StoredFile {
            url: "outputs/x.txt".to_string(),
            path: "/sandbox/outputs/x.txt".to_string(),
        };
        for stdout in [
            CommandStream::Inline {
                text: "hi\n".to_string(),
            },
            CommandStream::Stored {
                file: file.clone(),
                preview: "head".to_string(),
                truncated: true,
            },
            CommandStream::Image {
                media_type: "image/png".to_string(),
                file,
            },
        ] {
            let result = command(stdout, CommandOutcome::Exited { code: 0 });
            let record = ToolEventRecord::End {
                id: "call".to_string(),
                name: "terminal".to_string(),
                result: result.clone(),
            };
            let json = serde_json::to_string(&record).expect("record serializes");
            let ToolEventRecord::End { result: back, .. } =
                serde_json::from_str(&json).expect("record deserializes")
            else {
                panic!("expected an end record");
            };
            assert_eq!(back, result);
        }
    }

    #[test]
    fn status_follows_the_command_outcome() {
        let exited = |code| {
            command(
                CommandStream::Inline {
                    text: String::new(),
                },
                CommandOutcome::Exited { code },
            )
        };
        assert_eq!(
            parse_terminal_status_from_result(&exited(0)),
            TerminalExecutionStatus::Done
        );
        assert_eq!(
            parse_terminal_status_from_result(&exited(2)),
            TerminalExecutionStatus::Failed
        );
        let killed = command(
            CommandStream::Inline {
                text: String::new(),
            },
            CommandOutcome::Killed {
                task_id: "t".to_string(),
                notice: "killed at the foreground limit".to_string(),
            },
        );
        assert_eq!(
            parse_terminal_status_from_result(&killed),
            TerminalExecutionStatus::Failed
        );
    }

    fn run_to_snapshot(end: &ToolEventRecord) -> ApiTerminalExecution {
        let mut snapshot = TerminalStateSnapshot::default();
        update_terminal_snapshot_from_tool_event(
            &mut snapshot,
            &ToolEventRecord::Start {
                id: "call".to_string(),
                name: "terminal".to_string(),
                arguments: "{\"description\":\"d\",\"script\":\"make\"}".to_string(),
            },
        );
        update_terminal_snapshot_from_tool_event(&mut snapshot, end);
        assert!(snapshot.running_by_call_id.is_empty());
        snapshot.executions.pop().expect("one execution")
    }

    #[test]
    fn stopped_call_is_stopped_and_killed_result_is_failed_in_the_snapshot() {
        let stopped = run_to_snapshot(&ToolEventRecord::Stopped {
            id: "call".to_string(),
            name: "terminal".to_string(),
        });
        assert_eq!(stopped.status, TerminalExecutionStatus::Stopped);

        let killed = run_to_snapshot(&ToolEventRecord::End {
            id: "call".to_string(),
            name: "terminal".to_string(),
            result: command(
                CommandStream::Inline {
                    text: String::new(),
                },
                CommandOutcome::Killed {
                    task_id: "t".to_string(),
                    notice: "killed at the foreground limit".to_string(),
                },
            ),
        });
        assert_eq!(killed.status, TerminalExecutionStatus::Failed);
    }
}
