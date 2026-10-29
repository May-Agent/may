use aither_core::llm::{CommandResult, CommandStream, ToolResult, tool::ToolResultPart};
use aither_sandbox::{TerminalArgs, TerminalExecutionMode};

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

/// An image produced by a tool call, carried alongside its text output.
#[derive(Debug, Clone)]
pub enum ToolImage {
    /// Decoded image bytes taken straight from the result payload.
    Bytes(Vec<u8>),
    /// Image data the output store kept on disk.
    Stored {
        /// Sandbox-relative reference (for example `outputs/x.png`).
        url: String,
        /// Absolute path the store reported.
        path: String,
    },
}

/// Display payload for a completed tool call.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text rendered from the result, shown as the output preview.
    pub text: String,
    /// Image payload produced by the call, when present.
    pub image: Option<ToolImage>,
}

/// Frontend-agnostic end event for a tool call.
#[derive(Debug, Clone)]
pub enum ToolEndKind {
    /// The tool call completed.
    Output(ToolOutput),
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

    let output = result.render_for_model();

    // A bare binary image result (or a parts result containing one) carries
    // its bytes outside the typed command record.
    if let Some(image) = binary_tool_image(result) {
        return ToolEndKind::Output(ToolOutput {
            text: output,
            image: Some(image),
        });
    }

    if let Some(command) = result.command() {
        return ToolEndKind::Output(extract_tool_output(command));
    }

    ToolEndKind::Output(ToolOutput {
        text: output,
        image: None,
    })
}

/// Extract raw image bytes when the result is (or contains) an image binary
/// payload — for example a relayed MCP tool returning a screenshot.
fn binary_tool_image(result: &ToolResult) -> Option<ToolImage> {
    fn image_part(mime: &str, content: &[u8]) -> Option<ToolImage> {
        mime.parse::<mime::Mime>()
            .ok()
            .filter(|mime| mime.type_() == mime::IMAGE)
            .map(|_| ToolImage::Bytes(content.to_vec()))
    }

    match result {
        ToolResult::Binary { mime, content } => image_part(mime, content),
        ToolResult::Parts { parts } => parts.iter().find_map(|part| match part {
            ToolResultPart::Binary { mime, content } => image_part(mime, content),
            ToolResultPart::Text { .. } | ToolResultPart::Tsv { .. } => None,
        }),
        ToolResult::Done
        | ToolResult::Text { .. }
        | ToolResult::Tsv { .. }
        | ToolResult::Command(_)
        | ToolResult::Error { .. } => None,
    }
}

fn extract_tool_output(result: &CommandResult) -> ToolOutput {
    match &result.stdout {
        CommandStream::Inline(text) => ToolOutput {
            text: text.clone(),
            image: None,
        },
        CommandStream::Image { file, .. } => ToolOutput {
            text: format!("[image] {}", file.path),
            image: Some(ToolImage::Stored {
                url: file.url.clone(),
                path: file.path.clone(),
            }),
        },
        CommandStream::Stored { file, preview, .. } if preview.is_empty() => ToolOutput {
            text: format!("[stored: {}]", file.url),
            image: None,
        },
        CommandStream::Stored { preview, .. } => ToolOutput {
            text: preview.clone(),
            image: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolEndKind, ToolImage, ToolStartKind, parse_tool_end, parse_tool_start};
    use aither_core::llm::{
        CommandOutcome, CommandResult, CommandStream, StoredFile, ToolResult, tool::ToolResultPart,
    };
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

    fn terminal_result(stdout: CommandStream) -> ToolResult {
        ToolResult::Command(Box::new(CommandResult {
            stdout,
            stderr: None,
            outcome: CommandOutcome::Exited { code: 0 },
        }))
    }

    fn stored(url: &str, path: &str) -> StoredFile {
        StoredFile {
            url: url.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn parse_tool_end_extracts_stored_image_path() {
        let result = terminal_result(CommandStream::Image {
            media_type: "image/png".to_string(),
            file: stored("outputs/shot.png", "/session/outputs/shot.png"),
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => {
                assert_eq!(out.text, "[image] /session/outputs/shot.png");
                match out.image {
                    Some(ToolImage::Stored { url, path }) => {
                        assert_eq!(url, "outputs/shot.png");
                        assert_eq!(path, "/session/outputs/shot.png");
                    }
                    other => panic!("expected stored image reference, got {other:?}"),
                }
            }
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_extracts_binary_image_result() {
        let result = ToolResult::image(vec![1, 2, 3], "image/png");
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => match out.image {
                Some(ToolImage::Bytes(bytes)) => assert_eq!(bytes, [1, 2, 3]),
                other => panic!("expected binary image bytes, got {other:?}"),
            },
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_extracts_image_part_from_parts_result() {
        let result = ToolResult::parts(vec![
            ToolResultPart::text("screenshot taken"),
            ToolResultPart::image(vec![9, 8, 7], "image/jpeg"),
        ]);
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => {
                assert!(out.text.contains("screenshot taken"));
                match out.image {
                    Some(ToolImage::Bytes(bytes)) => assert_eq!(bytes, [9, 8, 7]),
                    other => panic!("expected part image bytes, got {other:?}"),
                }
            }
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_keeps_text_output() {
        let result = terminal_result(CommandStream::Inline("hello".to_string()));
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => {
                assert_eq!(out.text, "hello");
                assert!(out.image.is_none());
            }
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_keeps_stored_text_preview() {
        let result = terminal_result(CommandStream::Stored {
            file: stored("outputs/log.txt", "/session/outputs/log.txt"),
            preview: "preview".to_string(),
            truncated: true,
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => {
                assert_eq!(out.text, "preview");
                assert!(out.image.is_none());
            }
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_names_stored_output_without_preview() {
        let result = terminal_result(CommandStream::Stored {
            file: stored("outputs/blob.bin", "/session/outputs/blob.bin"),
            preview: String::new(),
            truncated: false,
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => assert_eq!(out.text, "[stored: outputs/blob.bin]"),
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }
}
