use aither_sandbox::{BashArgs, BashMode, BashResult, Content, OutputEntry};

/// Frontend-agnostic start event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolStartKind {
    Bash { command: String, mode: BashMode },
    Generic { name: String, arguments: String },
}

/// Frontend-agnostic end event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolEndKind {
    Output(String),
    Error(String),
}

/// Parse tool-call start payload into a minimal start event.
pub fn parse_tool_start(name: &str, arguments: &str) -> ToolStartKind {
    if name == "bash" {
        let args = serde_json::from_str::<BashArgs>(arguments)
            .unwrap_or_else(|e| panic!("invalid bash tool args payload: {e}"));
        ToolStartKind::Bash {
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
pub fn parse_tool_end(result: &Result<String, String>) -> ToolEndKind {
    match result {
        Ok(output) => {
            if let Ok(bash_result) = serde_json::from_str::<BashResult>(output) {
                ToolEndKind::Output(extract_output_text(&bash_result))
            } else {
                ToolEndKind::Output(output.clone())
            }
        }
        Err(e) => ToolEndKind::Error(e.clone()),
    }
}

fn extract_output_text(result: &BashResult) -> String {
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
                format!("[stored: {}]", url)
            }
        }
    }
}
