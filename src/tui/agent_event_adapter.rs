use aither_core::llm::ToolResult;
use aither_sandbox::{Content, OutputEntry, TerminalArgs, TerminalExecutionMode, TerminalResult};

/// Frontend-agnostic start event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolStartKind {
    /// A terminal tool invocation.
    Terminal {
        /// Command or script being executed.
        command: String,
        /// Requested execution backend for the command.
        mode: TerminalExecutionMode,
    },
    /// Any other tool invocation.
    Generic {
        /// Tool name.
        name: String,
        /// Raw arguments payload.
        arguments: String,
    },
}

/// Frontend-agnostic end event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolEndKind {
    /// The tool call completed; payload is the text to display.
    Output(String),
    /// The tool call failed; payload is the error message.
    Error(String),
}

/// Parse tool-call start payload into a minimal start event.
///
/// # Panics
///
/// Panics when `name` is `terminal` but `arguments` does not deserialize as
/// terminal tool args — the agent and its tool schema are versioned together,
/// so a malformed payload indicates a bug.
#[must_use]
pub fn parse_tool_start(name: &str, arguments: &str) -> ToolStartKind {
    if name == "terminal" {
        let args = serde_json::from_str::<TerminalArgs>(arguments)
            .unwrap_or_else(|e| panic!("invalid terminal tool args payload: {e}"));
        ToolStartKind::Terminal {
            command: args.script,
            mode: args.mode,
        }
    } else {
        ToolStartKind::Generic {
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }
}

/// Parse tool-call completion payload into a minimal end event.
#[must_use]
pub fn parse_tool_end(result: &ToolResult) -> ToolEndKind {
    if result.is_error() {
        return ToolEndKind::Error(result.error_message().unwrap_or("tool failed").to_string());
    }

    let output = match result.render_for_model() {
        Ok(output) => output,
        Err(error) => return ToolEndKind::Error(error.to_string()),
    };
    serde_json::from_str::<TerminalResult>(&output).map_or_else(
        |_| ToolEndKind::Output(output),
        |terminal_result| ToolEndKind::Output(extract_output_text(&terminal_result)),
    )
}

fn extract_output_text(result: &TerminalResult) -> String {
    match &result.stdout {
        OutputEntry::Empty => String::new(),
        OutputEntry::Inline { content } | OutputEntry::Loaded { content, .. } => match content {
            Content::Text { text, .. } => text.clone(),
            Content::Image { .. } => "[image]".to_string(),
        },
        OutputEntry::Stored { url, content, .. } => {
            if let Some(Content::Text { text, .. }) = content {
                text.clone()
            } else {
                format!("[stored: {url}]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolStartKind, parse_tool_start};
    use aither_sandbox::TerminalExecutionMode;

    #[test]
    fn parse_tool_start_uses_actual_execution_mode() {
        for (mode_json, expected) in [
            ("default", TerminalExecutionMode::Default),
            ("sandboxed", TerminalExecutionMode::Sandboxed),
            ("unsafe", TerminalExecutionMode::Unsafe),
            ("ssh", TerminalExecutionMode::Ssh),
        ] {
            let args = format!(
                r#"{{"description":"say hi","script":"echo hi","mode":"{mode_json}","timeout":5}}"#
            );
            let start = parse_tool_start("terminal", &args);
            match start {
                ToolStartKind::Terminal { command, mode } => {
                    assert_eq!(command, "echo hi");
                    assert_eq!(mode, expected);
                }
                ToolStartKind::Generic { .. } => panic!("expected terminal start event"),
            }
        }
    }
}
