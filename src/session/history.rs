//! In-memory chat history reconstructed from SQLite-backed session storage.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur when replaying chat history into LLM messages.
#[derive(Debug, Error)]
pub enum HistoryReplayError {
    /// Tool history entry is missing `tool_call_id`.
    #[error("tool history entry is missing tool_call_id (content preview: {preview})")]
    MissingToolCallId {
        /// First 100 characters of the offending entry.
        preview: String,
    },
    /// Tool history entry is not a valid persisted tool event.
    #[error("tool history entry is not a valid persisted tool event")]
    InvalidToolEvent(#[source] serde_json::Error),
    /// Tool event id does not match the message `tool_call_id`.
    #[error("tool event id '{event_id}' does not match message tool_call_id '{message_id}'")]
    ToolEventIdMismatch {
        /// Id recorded in the tool event.
        event_id: String,
        /// `tool_call_id` stored on the message.
        message_id: String,
    },
    /// Tool start contains invalid JSON arguments.
    #[error("tool start '{tool_call_id}' contains invalid JSON arguments")]
    InvalidToolArguments {
        /// Id of the malformed tool call.
        tool_call_id: String,
        /// Underlying JSON parse error.
        #[source]
        source: serde_json::Error,
    },
    /// Tool result could not be rendered for the model.
    #[error("tool result '{tool_call_id}' could not be rendered for the model")]
    RenderToolResult {
        /// Id of the tool call whose result failed to render.
        tool_call_id: String,
        /// Underlying render error.
        #[source]
        source: aither_core::Error,
    },
    /// A persisted attachment could not be replayed into model form.
    #[error("attachment '{name}' could not be replayed: {reason}")]
    InvalidAttachment {
        /// Display name of the offending attachment.
        name: String,
        /// Why the attachment could not be replayed.
        reason: String,
    },
}

/// Durable tool lifecycle record stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage")]
pub enum ToolEventRecord {
    /// Tool call started.
    #[serde(rename = "start")]
    Start {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
        /// Raw JSON arguments.
        arguments: String,
    },
    /// Tool call completed.
    #[serde(rename = "end")]
    End {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
        /// Rendered tool result.
        result: aither_core::llm::ToolResult,
    },
}

/// Session metadata stored in the chat history header.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Session title (generated after first response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Model bound to this session (set via /model command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Token/context stats attached to an assistant message.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessageStats {
    /// Provider that served the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Prompt token count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    /// Completion token count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    /// Total tokens billed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Tokens served from the provider cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    /// Tokens written to the provider cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    /// Model context window at request time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Request wall-clock time in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Role of a message in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// Message from the user.
    User,
    /// Message produced by the model.
    Assistant,
    /// System/context message.
    System,
    /// Hidden per-turn context stored for audit/debugging but filtered from normal UI recall.
    HiddenContext,
    /// Tool event record.
    Tool,
    /// Agent requests permission to run a command.
    PermissionRequest,
    /// User's response to a permission request.
    PermissionResponse,
    /// Agent requests workspace access.
    WorkspaceRequest,
    /// User's response to a workspace request.
    WorkspaceResponse,
    /// Agent asks the user a question.
    AskUserRequest,
    /// User's answer to an ask-user question.
    AskUserResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSummaryUpdate {
    pub(crate) touch_updated_at: bool,
    pub(crate) update_preview: bool,
    pub(crate) increment_message_count: bool,
}

impl MessageRole {
    /// Persisted `snake_case` string form.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::HiddenContext => "hidden_context",
            Self::Tool => "tool",
            Self::PermissionRequest => "permission_request",
            Self::PermissionResponse => "permission_response",
            Self::WorkspaceRequest => "workspace_request",
            Self::WorkspaceResponse => "workspace_response",
            Self::AskUserRequest => "ask_user_request",
            Self::AskUserResponse => "ask_user_response",
        }
    }

    /// Whether a persisted row of this role replays into the LLM-visible
    /// history (`ChatHistory::to_aither_messages`). Interaction rows are
    /// metadata and produce no LLM message.
    #[must_use]
    pub const fn is_llm_visible(&self) -> bool {
        matches!(
            self,
            Self::User | Self::Assistant | Self::System | Self::HiddenContext | Self::Tool
        )
    }

    #[must_use]
    pub(crate) const fn session_summary_update(&self) -> SessionSummaryUpdate {
        match self {
            Self::User | Self::Assistant | Self::System => SessionSummaryUpdate {
                touch_updated_at: true,
                update_preview: true,
                increment_message_count: true,
            },
            Self::HiddenContext
            | Self::Tool
            | Self::PermissionRequest
            | Self::PermissionResponse
            | Self::WorkspaceRequest
            | Self::WorkspaceResponse
            | Self::AskUserRequest
            | Self::AskUserResponse => SessionSummaryUpdate {
                touch_updated_at: true,
                update_preview: false,
                increment_message_count: false,
            },
        }
    }
}

impl core::str::FromStr for MessageRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "system" => Ok(Self::System),
            "hidden_context" => Ok(Self::HiddenContext),
            "tool" => Ok(Self::Tool),
            "permission_request" => Ok(Self::PermissionRequest),
            "permission_response" => Ok(Self::PermissionResponse),
            "workspace_request" => Ok(Self::WorkspaceRequest),
            "workspace_response" => Ok(Self::WorkspaceResponse),
            "ask_user_request" => Ok(Self::AskUserRequest),
            "ask_user_response" => Ok(Self::AskUserResponse),
            _ => Err(s.to_string()),
        }
    }
}

/// A single message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Message role.
    pub role: MessageRole,
    /// Message body (JSON payload for tool events).
    pub content: String,
    /// Attachments the message carried (images, file references, pastes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<super::runtime::PersistedAttachment>,
    /// Message timestamp, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Timestamp>,
    /// Tool call this message is the result of, when it is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Usage/context stats for assistant messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<ChatMessageStats>,
}

impl ChatMessage {
    /// Create a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::user_with_attachments(content, Vec::new())
    }

    /// Create a new user message carrying attachments.
    pub fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<super::runtime::PersistedAttachment>,
    ) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            attachments,
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create a new assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create a new assistant message with usage stats.
    pub fn assistant_with_stats(content: impl Into<String>, stats: ChatMessageStats) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: Some(stats),
        }
    }

    /// Create a new system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create a new tool message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: Some(tool_call_id.into()),
            stats: None,
        }
    }

    /// Create a hidden context message.
    ///
    /// This message is treated as system context for the model but is not
    /// surfaced in chat history APIs.
    pub fn hidden_context(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::HiddenContext,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }
}

/// Chat history reconstructed from persisted session rows.
#[derive(Debug, Clone, Default)]
pub struct ChatHistory {
    /// Session metadata (title, model).
    pub metadata: SessionMetadata,
    /// Chat messages.
    messages: Vec<ChatMessage>,
}

impl ChatHistory {
    /// Create an empty chat history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a chat history with metadata.
    #[must_use]
    pub const fn with_metadata(metadata: SessionMetadata) -> Self {
        Self {
            metadata,
            messages: Vec::new(),
        }
    }

    /// Add a message to history.
    pub fn push(&mut self, message: ChatMessage) {
        self.messages.push(message);
    }

    /// Get all messages.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Get mutable access to messages.
    pub const fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }

    /// Check if history is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get number of messages.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.messages.len()
    }

    /// Convert to aither messages.
    ///
    /// Rebuild durable LLM history from persisted chat messages.
    ///
    /// Interaction metadata stays filtered, while hidden context is restored as
    /// system context and tool messages keep their original tool-call IDs.
    ///
    /// # Errors
    /// Returns [`HistoryReplayError`] when a persisted tool entry is malformed, has mismatched ids, or carries invalid JSON arguments.
    pub fn to_aither_messages(&self) -> Result<Vec<aither_core::llm::Message>, HistoryReplayError> {
        self.messages
            .iter()
            .filter_map(|m| {
                use aither_core::llm::Message;
                match m.role {
                    MessageRole::User => Some(replay_user_message(m)),
                    MessageRole::Assistant => Some(Ok(Message::assistant(&m.content))),
                    MessageRole::System | MessageRole::HiddenContext => {
                        Some(Ok(Message::system(&m.content)))
                    }
                    MessageRole::Tool => {
                        let Some(tool_call_id) = m.tool_call_id.as_deref() else {
                            let preview: String = m.content.chars().take(100).collect();
                            return Some(Err(HistoryReplayError::MissingToolCallId { preview }));
                        };
                        let event = match serde_json::from_str::<ToolEventRecord>(&m.content) {
                            Ok(event) => event,
                            Err(error) => {
                                return Some(Err(HistoryReplayError::InvalidToolEvent(error)));
                            }
                        };
                        match event {
                            ToolEventRecord::Start {
                                id,
                                name,
                                arguments,
                            } => {
                                if id != tool_call_id {
                                    return Some(Err(HistoryReplayError::ToolEventIdMismatch {
                                        event_id: id,
                                        message_id: tool_call_id.to_string(),
                                    }));
                                }
                                let arguments = match serde_json::from_str(&arguments) {
                                    Ok(arguments) => arguments,
                                    Err(source) => {
                                        return Some(Err(
                                            HistoryReplayError::InvalidToolArguments {
                                                tool_call_id: id,
                                                source,
                                            },
                                        ));
                                    }
                                };
                                Some(Ok(Message::assistant_with_tool_calls(
                                    "",
                                    vec![aither_core::llm::ToolCall::new(id, name, arguments)],
                                )))
                            }
                            ToolEventRecord::End { id, result, .. } => {
                                if id != tool_call_id {
                                    return Some(Err(HistoryReplayError::ToolEventIdMismatch {
                                        event_id: id,
                                        message_id: tool_call_id.to_string(),
                                    }));
                                }
                                match result.render_for_model() {
                                    Ok(content) => Some(Ok(Message::tool(id, content))),
                                    Err(source) => {
                                        Some(Err(HistoryReplayError::RenderToolResult {
                                            tool_call_id: id,
                                            source,
                                        }))
                                    }
                                }
                            }
                        }
                    }
                    // Interaction messages are metadata, not LLM context
                    MessageRole::PermissionRequest
                    | MessageRole::PermissionResponse
                    | MessageRole::WorkspaceRequest
                    | MessageRole::WorkspaceResponse
                    | MessageRole::AskUserRequest
                    | MessageRole::AskUserResponse => None,
                }
            })
            .collect()
    }
}

fn invalid_attachment(
    attachment: &super::runtime::PersistedAttachment,
    reason: impl std::fmt::Display,
) -> HistoryReplayError {
    HistoryReplayError::InvalidAttachment {
        name: attachment.name.clone(),
        reason: reason.to_string(),
    }
}

/// Resolve a persisted attachment's `file://` URL to a local path.
fn attachment_local_path(
    attachment: &super::runtime::PersistedAttachment,
) -> Result<std::path::PathBuf, HistoryReplayError> {
    url::Url::parse(&attachment.url)
        .map_err(|error| invalid_attachment(attachment, format!("invalid url: {error}")))?
        .to_file_path()
        .map_err(|()| invalid_attachment(attachment, "url is not a local file"))
}

/// Rebuild the model-facing user message a persisted [`ChatMessage`]
/// represents: pasted text becomes its XML block, file references their
/// marker line, and binary attachments become typed LLM attachments —
/// mirroring what the frontends sent at send time.
fn replay_user_message(
    message: &ChatMessage,
) -> Result<aither_core::llm::Message, HistoryReplayError> {
    use super::runtime::AttachmentKind;
    use std::fmt::Write as _;

    let mut content = message.content.clone();
    let mut attachments: Vec<aither_core::llm::Attachment> = Vec::new();
    for attachment in &message.attachments {
        match attachment.attachment_type {
            AttachmentKind::Text => {
                let text = match attachment.text_content.as_deref() {
                    Some(text) => text.to_string(),
                    None => std::fs::read_to_string(attachment_local_path(attachment)?).map_err(
                        |error| {
                            invalid_attachment(attachment, format!("read pasted text: {error}"))
                        },
                    )?,
                };
                let saved_path = attachment_local_path(attachment).ok();
                let xml = crate::xml::pasted_text_xml(&text, saved_path.as_deref())
                    .map_err(|error| invalid_attachment(attachment, error))?;
                let _ = write!(content, "\n{xml}");
            }
            AttachmentKind::File => {
                let label = attachment_local_path(attachment).map_or_else(
                    |_| attachment.url.clone(),
                    |path| path.display().to_string(),
                );
                let _ = write!(content, "\n[File reference: {label}]");
            }
            AttachmentKind::Image
            | AttachmentKind::Document
            | AttachmentKind::Pdf
            | AttachmentKind::Audio
            | AttachmentKind::Video => {
                let url = attachment_url(attachment)?;
                let mime = attachment_media_type(attachment, &url)?;
                attachments.push(aither_core::llm::Attachment::new(url, mime));
            }
        }
    }
    Ok(aither_core::llm::Message::user(&content).with_attachments(attachments))
}

/// Resolve the URL to hand the provider for a binary attachment. The
/// recorded `file://` URL wins while the file still exists; otherwise the
/// inline `data:` URL is used when one was stored.
fn attachment_url(
    attachment: &super::runtime::PersistedAttachment,
) -> Result<url::Url, HistoryReplayError> {
    let url = url::Url::parse(&attachment.url)
        .map_err(|error| invalid_attachment(attachment, format!("invalid url: {error}")))?;
    let file_gone = url.to_file_path().is_ok_and(|path| !path.exists());
    if !file_gone {
        return Ok(url);
    }
    attachment
        .upload_data_url
        .as_deref()
        .and_then(|raw| url::Url::parse(raw).ok())
        .ok_or_else(|| invalid_attachment(attachment, "file missing and no stored bytes"))
}

fn attachment_media_type(
    attachment: &super::runtime::PersistedAttachment,
    url: &url::Url,
) -> Result<mime::Mime, HistoryReplayError> {
    mime_guess::from_path(&attachment.name)
        .first()
        .or_else(|| mime_guess::from_path(url.path()).first())
        .or_else(|| {
            // data:<media-type>;base64,... — the payload prefix carries the
            // type when the file path and name do not.
            (url.scheme() == "data")
                .then(|| url.path().split([',', ';']).next().unwrap_or_default())
                .and_then(|media_type| media_type.parse().ok())
        })
        .ok_or_else(|| invalid_attachment(attachment, "unknown media type"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_core::llm::{Role, ToolResult};

    #[test]
    fn hidden_context_replays_as_system_message() {
        let mut history = ChatHistory::new();
        history.messages_mut().push(ChatMessage::hidden_context(
            "<system-reminder>notice</system-reminder>",
        ));
        history.messages_mut().push(ChatMessage::user("hello"));
        history.messages_mut().push(ChatMessage::assistant("world"));

        let messages = history.to_aither_messages().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role(), Role::System);
        assert_eq!(messages[1].role(), Role::User);
        assert_eq!(messages[2].role(), Role::Assistant);
        assert_eq!(
            messages[0].content(),
            "<system-reminder>notice</system-reminder>"
        );
    }

    #[test]
    fn tool_lifecycle_replays_as_matching_assistant_call_and_result() {
        let mut history = ChatHistory::new();
        let start = serde_json::to_string(&ToolEventRecord::Start {
            id: "call_123".to_string(),
            name: "lookup".to_string(),
            arguments: "{\"query\":\"rust\"}".to_string(),
        })
        .expect("serialize tool start");
        let end = serde_json::to_string(&ToolEventRecord::End {
            id: "call_123".to_string(),
            name: "lookup".to_string(),
            result: ToolResult::text("done"),
        })
        .expect("serialize tool end");
        history
            .messages_mut()
            .push(ChatMessage::tool("call_123", start));
        history
            .messages_mut()
            .push(ChatMessage::tool("call_123", end));

        let messages = history.to_aither_messages().expect("replay tool lifecycle");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), Role::Assistant);
        assert_eq!(messages[0].tool_calls()[0].id, "call_123");
        assert_eq!(messages[0].tool_calls()[0].name, "lookup");
        assert_eq!(messages[1].role(), Role::Tool);
        assert_eq!(messages[1].tool_call_id(), Some("call_123"));
        assert_eq!(messages[1].content(), "done");
    }

    #[test]
    fn unstructured_legacy_tool_history_fails_fast() {
        let mut history = ChatHistory::new();
        history
            .messages_mut()
            .push(ChatMessage::tool("call_123", "plain legacy output"));
        assert!(matches!(
            history.to_aither_messages(),
            Err(HistoryReplayError::InvalidToolEvent(_))
        ));
    }
}
