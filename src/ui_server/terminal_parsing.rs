//! Native terminal-command tool event parsing for streaming UI updates.

use aither_core::llm::ToolResult;
use aither_sandbox::{OutputEntry, TerminalArgs, TerminalExecutionMode};

use super::types::{
    ApiTerminalExecution, TERMINAL_TOOL_NAME, TerminalCommandMode, TerminalExecutionStatus,
    TerminalStateSnapshot, ToolEventRecord,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalTaskStatus {
    Running,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) struct ParsedTerminalToolResult {
    pub(super) stdout: OutputEntry,
    pub(super) exit_code: i32,
    #[serde(default)]
    pub(super) status: Option<TerminalTaskStatus>,
}

fn parse_terminal_mode(mode: TerminalExecutionMode) -> TerminalCommandMode {
    match mode {
        TerminalExecutionMode::Default => TerminalCommandMode::Default,
        TerminalExecutionMode::Sandboxed => TerminalCommandMode::Sandboxed,
        TerminalExecutionMode::Unsafe => TerminalCommandMode::Unsafe,
        TerminalExecutionMode::Ssh => TerminalCommandMode::Ssh,
    }
}

pub(super) fn parse_terminal_start(arguments: &str) -> (String, Option<TerminalCommandMode>) {
    let parsed: TerminalArgs = serde_json::from_str(arguments).unwrap_or_else(|error| {
        panic!("invalid terminal tool start payload: {error}; payload: {arguments}")
    });
    (parsed.script, Some(parse_terminal_mode(parsed.mode)))
}

pub(super) fn parse_terminal_result_payload(
    result: &ToolResult,
) -> Option<ParsedTerminalToolResult> {
    let ToolResult::Json { value } = result else {
        return None;
    };
    serde_json::from_value(value.clone()).ok()
}

/// Preserve terminal tool output as structured JSON when possible.
fn parse_terminal_end_output_from_result(result: &ToolResult) -> Option<String> {
    if let ToolResult::Json { value } = result {
        return Some(value.to_string());
    }
    result.render_for_model().ok()
}

/// Determine if a terminal execution failed from a typed tool result.
fn parse_terminal_failed_from_result(result: &ToolResult) -> bool {
    if result.is_error() {
        return true;
    }
    parse_terminal_result_payload(result).is_some_and(|parsed| parsed.exit_code != 0)
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
            let (script, mode) = parse_terminal_start(arguments);
            let mode = mode.unwrap_or(TerminalCommandMode::Default);
            snapshot.running_by_call_id.insert(
                id.clone(),
                ApiTerminalExecution {
                    id: id.clone(),
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
            let mut exec = snapshot
                .running_by_call_id
                .remove(id)
                .unwrap_or(ApiTerminalExecution {
                    id: id.clone(),
                    input: "(terminal)".to_string(),
                    output: None,
                    status: TerminalExecutionStatus::Done,
                    mode: TerminalCommandMode::Default,
                });

            let output = parse_terminal_end_output_from_result(result);
            let failed = parse_terminal_failed_from_result(result);
            exec.output = output;
            exec.status = if failed {
                TerminalExecutionStatus::Failed
            } else {
                TerminalExecutionStatus::Done
            };

            snapshot.executions.push(exec);
        }
    }
}
