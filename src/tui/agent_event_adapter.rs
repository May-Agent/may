use aither_sandbox::{
    Content, OutputEntry, TerminalArgs, TerminalExecutionMode, TerminalMode, TerminalResult,
};

/// Frontend-agnostic start event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolStartKind {
    Terminal { command: String, mode: TerminalMode },
    Generic { name: String, arguments: String },
}

/// Frontend-agnostic end event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolEndKind {
    Output(String),
    Error(String),
}

/// Parse tool-call start payload into a minimal start event.
#[must_use]
pub fn parse_tool_start(name: &str, arguments: &str) -> ToolStartKind {
    if name == "terminal" {
        let args = serde_json::from_str::<TerminalArgs>(arguments)
            .unwrap_or_else(|e| panic!("invalid terminal tool args payload: {e}"));
        ToolStartKind::Terminal {
            command: args.script,
            mode: display_mode_from_execution_mode(args.mode),
        }
    } else {
        ToolStartKind::Generic {
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }
}

const fn display_mode_from_execution_mode(mode: TerminalExecutionMode) -> TerminalMode {
    match mode {
        TerminalExecutionMode::Default | TerminalExecutionMode::Ssh => TerminalMode::Network,
        TerminalExecutionMode::Sandboxed => TerminalMode::Sandboxed,
        TerminalExecutionMode::Unsafe => TerminalMode::Unsafe,
    }
}

/// Parse tool-call completion payload into a minimal end event.
#[must_use]
pub fn parse_tool_end(result: &Result<String, String>) -> ToolEndKind {
    match result {
        Ok(output) => {
            if let Ok(terminal_result) = serde_json::from_str::<TerminalResult>(output) {
                ToolEndKind::Output(extract_output_text(&terminal_result))
            } else {
                ToolEndKind::Output(output.clone())
            }
        }
        Err(e) => ToolEndKind::Error(e.clone()),
    }
}

fn extract_output_text(result: &TerminalResult) -> String {
    match &result.stdout {
        OutputEntry::Empty => String::new(),
        OutputEntry::Inline { content } | OutputEntry::Loaded { content, .. } => match content {
            Content::Text { text, .. } => text.clone(),
            Content::Image { .. } => "[image]".to_string(),
        },
        OutputEntry::Stored { url, content } => {
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
    use aither_sandbox::TerminalMode;

    #[test]
    fn parse_tool_start_uses_actual_execution_mode() {
        let args = r#"{"script":"echo hi","mode":"unsafe","timeout":5}"#;
        let start = parse_tool_start("terminal", args);
        match start {
            ToolStartKind::Terminal { command, mode } => {
                assert_eq!(command, "echo hi");
                assert_eq!(mode, TerminalMode::Unsafe);
            }
            ToolStartKind::Generic { .. } => panic!("expected terminal start event"),
        }
    }
}
