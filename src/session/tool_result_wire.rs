//! Wire form of a [`ToolResult`] in persisted tool events and the web API.
//!
//! The wire form is owned here so the stored and streamed JSON stays stable
//! and typed: a command run keeps its stdout, stderr and outcome as fields
//! the frontend reads directly, never prose it would have to parse. Every
//! other variant keeps the shape `ToolResult` itself serializes to.

use aither_core::llm::{
    CommandOutcome, CommandResult, CommandStream, StoredFile, ToolResult, tool::ToolResultPart,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One output stream of a command run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireStream {
    Inline {
        text: String,
    },
    Stored {
        file: StoredFile,
        preview: String,
        truncated: bool,
    },
    Image {
        media_type: String,
        file: StoredFile,
    },
}

impl From<&CommandStream> for WireStream {
    fn from(stream: &CommandStream) -> Self {
        match stream {
            CommandStream::Inline(text) => Self::Inline { text: text.clone() },
            CommandStream::Stored {
                file,
                preview,
                truncated,
            } => Self::Stored {
                file: file.clone(),
                preview: preview.clone(),
                truncated: *truncated,
            },
            CommandStream::Image { media_type, file } => Self::Image {
                media_type: media_type.clone(),
                file: file.clone(),
            },
        }
    }
}

impl From<WireStream> for CommandStream {
    fn from(stream: WireStream) -> Self {
        match stream {
            WireStream::Inline { text } => Self::Inline(text),
            WireStream::Stored {
                file,
                preview,
                truncated,
            } => Self::Stored {
                file,
                preview,
                truncated,
            },
            WireStream::Image { media_type, file } => Self::Image { media_type, file },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireCommand {
    stdout: WireStream,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stderr: Option<WireStream>,
    outcome: CommandOutcome,
}

/// Wire form of a [`ToolResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireToolResult {
    Done,
    Text { text: String },
    Tsv { text: String },
    Binary { mime: String, content: Vec<u8> },
    Error { message: String },
    Command(WireCommand),
    Parts { parts: Vec<ToolResultPart> },
}

impl From<&ToolResult> for WireToolResult {
    fn from(result: &ToolResult) -> Self {
        match result {
            ToolResult::Done => Self::Done,
            ToolResult::Text { text } => Self::Text { text: text.clone() },
            ToolResult::Tsv { text } => Self::Tsv { text: text.clone() },
            ToolResult::Binary { mime, content } => Self::Binary {
                mime: mime.clone(),
                content: content.clone(),
            },
            ToolResult::Error { message } => Self::Error {
                message: message.clone(),
            },
            ToolResult::Command(command) => Self::Command(WireCommand {
                stdout: WireStream::from(&command.stdout),
                stderr: command.stderr.as_ref().map(WireStream::from),
                outcome: command.outcome.clone(),
            }),
            ToolResult::Parts { parts } => Self::Parts {
                parts: parts.clone(),
            },
        }
    }
}

impl From<WireToolResult> for ToolResult {
    fn from(result: WireToolResult) -> Self {
        match result {
            WireToolResult::Done => Self::Done,
            WireToolResult::Text { text } => Self::Text { text },
            WireToolResult::Tsv { text } => Self::Tsv { text },
            WireToolResult::Binary { mime, content } => Self::Binary { mime, content },
            WireToolResult::Error { message } => Self::Error { message },
            WireToolResult::Command(command) => Self::Command(Box::new(CommandResult {
                stdout: command.stdout.into(),
                stderr: command.stderr.map(Into::into),
                outcome: command.outcome,
            })),
            WireToolResult::Parts { parts } => Self::Parts { parts },
        }
    }
}

/// `#[serde(with = ...)]` adapter serializing a [`ToolResult`] in wire form.
///
/// # Errors
///
/// Propagates the serializer's error.
pub fn serialize<S: Serializer>(result: &ToolResult, serializer: S) -> Result<S::Ok, S::Error> {
    WireToolResult::from(result).serialize(serializer)
}

/// `#[serde(with = ...)]` adapter deserializing a [`ToolResult`] from wire form.
///
/// # Errors
///
/// Propagates the deserializer's error.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ToolResult, D::Error> {
    WireToolResult::deserialize(deserializer).map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(stdout: CommandStream, stderr: Option<CommandStream>) -> ToolResult {
        ToolResult::Command(Box::new(CommandResult {
            stdout,
            stderr,
            outcome: CommandOutcome::Exited { code: 3 },
        }))
    }

    #[test]
    fn command_wire_shape_is_typed_fields() {
        let result = command(
            CommandStream::Inline("out".to_string()),
            Some(CommandStream::Stored {
                file: StoredFile {
                    url: "outputs/e.txt".to_string(),
                    path: "/s/outputs/e.txt".to_string(),
                },
                preview: "head".to_string(),
                truncated: true,
            }),
        );
        let value = serde_json::to_value(WireToolResult::from(&result)).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "command",
                "stdout": {"kind": "inline", "text": "out"},
                "stderr": {
                    "kind": "stored",
                    "file": {"url": "outputs/e.txt", "path": "/s/outputs/e.txt"},
                    "preview": "head",
                    "truncated": true,
                },
                "outcome": {"status": "exited", "code": 3},
            })
        );
    }

    #[test]
    fn every_variant_round_trips() {
        let results = [
            ToolResult::Done,
            ToolResult::text("hello"),
            ToolResult::tsv("a\tb\n"),
            ToolResult::image(vec![1, 2, 3], "image/png"),
            ToolResult::error("boom"),
            command(CommandStream::Inline("out".to_string()), None),
            command(
                CommandStream::Image {
                    media_type: "image/png".to_string(),
                    file: StoredFile {
                        url: "outputs/a.png".to_string(),
                        path: "/s/outputs/a.png".to_string(),
                    },
                },
                None,
            ),
            ToolResult::parts(vec![
                ToolResultPart::text("shot"),
                ToolResultPart::image(vec![9], "image/jpeg"),
            ]),
        ];
        for result in results {
            let json = serde_json::to_string(&WireToolResult::from(&result)).expect("serialize");
            let back: WireToolResult = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(ToolResult::from(back), result);
        }
    }
}
