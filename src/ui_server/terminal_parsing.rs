//! Native terminal-command tool event parsing for streaming UI updates.

use aither_core::llm::ToolResult;

use super::types::{
    ApiTerminalExecution, TERMINAL_TOOL_NAME, TerminalCommandMode, TerminalExecutionStatus,
    TerminalStateSnapshot, ToolEventRecord,
};

fn parse_terminal_mode(raw: &str) -> TerminalCommandMode {
    match raw {
        "default" => TerminalCommandMode::Default,
        "sandboxed" => TerminalCommandMode::Sandboxed,
        "unsafe" => TerminalCommandMode::Unsafe,
        "ssh" => TerminalCommandMode::Ssh,
        other => panic!("invalid terminal command mode: {other}"),
    }
}

pub(super) fn parse_terminal_start(arguments: &str) -> (String, Option<TerminalCommandMode>) {
    let parsed = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let script = parsed
        .as_ref()
        .and_then(|v| v.get("script"))
        .and_then(|v| v.as_str())
        .unwrap_or(arguments)
        .to_string();
    let mode = parsed
        .as_ref()
        .and_then(|v| v.get("mode"))
        .and_then(|v| v.as_str())
        .map(parse_terminal_mode);
    (script, mode)
}

/// Preserve terminal tool output as structured JSON when possible.
fn parse_terminal_end_output_from_result(result: &ToolResult) -> Option<String> {
    let Ok(serialized) = result.render_for_model() else {
        return None;
    };
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&serialized) {
        return Some(parsed.to_string());
    }
    Some(serialized)
}

/// Determine if a terminal execution failed from a typed tool result.
fn parse_terminal_failed_from_result(result: &ToolResult) -> bool {
    if result.is_error() {
        return true;
    }
    let Ok(serialized) = result.render_for_model() else {
        return true;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&serialized) else {
        return false;
    };
    let nonzero_exit = parsed
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|code| code != 0);
    let failed_status = parsed
        .get("status")
        .and_then(|v| v.as_str())
        .is_some_and(|status| status.eq_ignore_ascii_case("failed"));
    nonzero_exit || failed_status
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
