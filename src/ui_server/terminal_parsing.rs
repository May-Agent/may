//! Native terminal-command tool event parsing for streaming UI updates.

use aither_core::llm::ToolResult;
use aither_sandbox::{OutputEntry, TerminalArgs, TerminalExecutionMode};
use serde::Deserialize;

use super::types::{
    ApiTerminalExecution, TERMINAL_TOOL_NAME, TerminalCommandMode, TerminalExecutionStatus,
    TerminalStateSnapshot, ToolEventRecord,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalTaskStatus {
    Running,
    Stopped,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) struct ParsedTerminalToolResult {
    #[schema(value_type = serde_json::Value)]
    pub(super) stdout: OutputEntry,
    pub(super) exit_code: i32,
    #[serde(default)]
    pub(super) status: Option<TerminalTaskStatus>,
}

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

pub(super) fn parse_terminal_result_payload(
    result: &ToolResult,
) -> Option<ParsedTerminalToolResult> {
    let ToolResult::Json { value } = result else {
        return None;
    };
    // Deserialize from `&Value` directly — `serde_json::from_value` would
    // deep-clone the whole tree (including full stdout) first.
    ParsedTerminalToolResult::deserialize(value).ok()
}

/// Preserve terminal tool output as structured JSON when possible.
fn parse_terminal_end_output_from_result(result: &ToolResult) -> Option<String> {
    if let ToolResult::Json { value } = result {
        return Some(value.to_string());
    }
    result.render_for_model().ok()
}

fn parse_terminal_status_from_result(result: &ToolResult) -> TerminalExecutionStatus {
    if result.is_error() {
        return TerminalExecutionStatus::Failed;
    }
    match parse_terminal_result_payload(result) {
        Some(parsed) => match parsed.status {
            Some(TerminalTaskStatus::Stopped) => TerminalExecutionStatus::Stopped,
            _ if parsed.exit_code != 0 => TerminalExecutionStatus::Failed,
            _ => TerminalExecutionStatus::Done,
        },
        None => TerminalExecutionStatus::Done,
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

            let output = parse_terminal_end_output_from_result(result);
            exec.output = output;
            exec.status = parse_terminal_status_from_result(result);

            snapshot.executions.push(exec);
        }
    }
}
