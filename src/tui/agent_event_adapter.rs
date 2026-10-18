use aither_core::llm::{ToolResult, tool::ToolResultPart};
use aither_sandbox::{
    Content, OutputEntry, OutputFormat, TerminalArgs, TerminalExecutionMode, TerminalResult,
};
use base64::Engine;

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

    let output = match result.render_for_model() {
        Ok(output) => output,
        Err(error) => return ToolEndKind::Error(error.to_string()),
    };

    // A bare binary image result (or a parts result containing one) carries
    // its bytes outside the terminal-output JSON envelope.
    if let Some(image) = binary_tool_image(result) {
        return ToolEndKind::Output(ToolOutput {
            text: output,
            image: Some(image),
        });
    }

    serde_json::from_str::<TerminalResult>(&output).map_or_else(
        |_| {
            ToolEndKind::Output(ToolOutput {
                text: output,
                image: None,
            })
        },
        |terminal_result| ToolEndKind::Output(extract_tool_output(&terminal_result)),
    )
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
            ToolResultPart::Text { .. }
            | ToolResultPart::Tsv { .. }
            | ToolResultPart::Json { .. } => None,
        }),
        ToolResult::Done
        | ToolResult::Text { .. }
        | ToolResult::Tsv { .. }
        | ToolResult::Json { .. }
        | ToolResult::Error { .. } => None,
    }
}

fn decode_image_data(data: &str) -> Option<ToolImage> {
    match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => Some(ToolImage::Bytes(bytes)),
        Err(error) => {
            tracing::warn!("invalid base64 image payload in tool output: {error}");
            None
        }
    }
}

fn is_image_path(path: &str) -> bool {
    mime_guess::from_path(path)
        .first()
        .is_some_and(|mime| mime.type_() == mime::IMAGE)
}

fn extract_tool_output(result: &TerminalResult) -> ToolOutput {
    match &result.stdout {
        OutputEntry::Empty => ToolOutput {
            text: String::new(),
            image: None,
        },
        OutputEntry::Inline { content } => match content {
            Content::Text { text, .. } => ToolOutput {
                text: text.clone(),
                image: None,
            },
            Content::Image { data, .. } => ToolOutput {
                text: "[image]".to_string(),
                image: decode_image_data(data),
            },
        },
        OutputEntry::Loaded {
            content,
            raw,
            format,
        } => {
            if matches!(content, Content::Image { .. }) || *format == OutputFormat::Image {
                return ToolOutput {
                    text: "[image]".to_string(),
                    image: Some(ToolImage::Bytes(raw.clone())),
                };
            }
            match content {
                Content::Text { text, .. } => ToolOutput {
                    text: text.clone(),
                    image: None,
                },
                Content::Image { .. } => unreachable!("image content handled above"),
            }
        }
        OutputEntry::Stored { url, path, content } => match content {
            Some(Content::Image { data, .. }) => ToolOutput {
                text: format!("[image] {path}"),
                image: decode_image_data(data),
            },
            Some(Content::Text { text, .. }) => ToolOutput {
                text: text.clone(),
                image: None,
            },
            None if is_image_path(path) || is_image_path(url) => ToolOutput {
                text: format!("[image] {path}"),
                image: Some(ToolImage::Stored {
                    url: url.clone(),
                    path: path.clone(),
                }),
            },
            None => ToolOutput {
                text: format!("[stored: {url}]"),
                image: None,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolEndKind, ToolImage, ToolStartKind, parse_tool_end, parse_tool_start};
    use aither_core::llm::{ToolResult, tool::ToolResultPart};
    use aither_sandbox::{Content, OutputEntry, TerminalExecutionMode, TerminalResult};

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

    fn terminal_result(stdout: OutputEntry) -> ToolResult {
        ToolResult::json(&TerminalResult {
            stdout,
            stderr: None,
            exit_code: 0,
            task_id: None,
            status: None,
            background_reason: None,
        })
        .expect("serialize terminal result")
    }

    #[test]
    fn parse_tool_end_extracts_inline_image_bytes() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let result = terminal_result(OutputEntry::Inline {
            content: Content::Image {
                data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png),
                media_type: "image/png".to_string(),
            },
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => {
                assert_eq!(out.text, "[image]");
                match out.image {
                    Some(ToolImage::Bytes(bytes)) => assert_eq!(bytes, png),
                    other => panic!("expected decoded image bytes, got {other:?}"),
                }
            }
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_extracts_stored_image_path() {
        let result = terminal_result(OutputEntry::Stored {
            url: "outputs/shot.png".to_string(),
            path: "/session/outputs/shot.png".to_string(),
            content: None,
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => match out.image {
                Some(ToolImage::Stored { url, path }) => {
                    assert_eq!(url, "outputs/shot.png");
                    assert_eq!(path, "/session/outputs/shot.png");
                }
                other => panic!("expected stored image reference, got {other:?}"),
            },
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn parse_tool_end_extracts_stored_image_preview() {
        let result = terminal_result(OutputEntry::Stored {
            url: "outputs/big.png".to_string(),
            path: "/session/outputs/big.png".to_string(),
            content: Some(Content::Image {
                data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 8]),
                media_type: "image/png".to_string(),
            }),
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => match out.image {
                Some(ToolImage::Bytes(bytes)) => assert_eq!(bytes, [7u8; 8]),
                other => panic!("expected preview bytes, got {other:?}"),
            },
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
        let result = terminal_result(OutputEntry::Inline {
            content: Content::Text {
                text: "hello".to_string(),
                truncated: false,
            },
        });
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
        let result = terminal_result(OutputEntry::Stored {
            url: "outputs/log.txt".to_string(),
            path: "/session/outputs/log.txt".to_string(),
            content: Some(Content::Text {
                text: "preview".to_string(),
                truncated: true,
            }),
        });
        match parse_tool_end(&result) {
            ToolEndKind::Output(out) => {
                assert_eq!(out.text, "preview");
                assert!(out.image.is_none());
            }
            ToolEndKind::Error(err) => panic!("unexpected error: {err}"),
        }
    }
}
