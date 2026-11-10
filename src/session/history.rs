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
        /// How May read the script of a terminal call as it started, with
        /// line numbers a later edit of the files would change; absent for
        /// other calls and for rows stored before it was kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reading: Option<crate::command_reading::CommandReading>,
    },
    /// Tool call completed.
    #[serde(rename = "end")]
    End {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
        /// Tool result, stored in its wire form.
        #[serde(with = "super::tool_result_wire")]
        result: aither_core::llm::ToolResult,
    },
    /// Tool call the user stopped before it produced a result.
    #[serde(rename = "stopped")]
    Stopped {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
    },
}

impl ToolEventRecord {
    /// What the model and the UI see as the result of a stopped call.
    #[must_use]
    pub fn stopped_result() -> aither_core::llm::ToolResult {
        aither_core::llm::ToolResult::text("Stopped by the user before it finished.")
    }
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
    /// Daemon-generated catch-up summary shown to a returning user; never
    /// replays into model context.
    Recap,
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
            Self::Recap => "recap",
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
            | Self::AskUserResponse
            | Self::Recap => SessionSummaryUpdate {
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
            "recap" => Ok(Self::Recap),
            _ => Err(s.to_string()),
        }
    }
}

/// A single message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Row id in the session's stored history; `None` for a message not
    /// stored yet. The daemon sends it with the history so a client can tell
    /// whether a replayed turn's prompt is already loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i32>,
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
    /// Provider reasoning state carried by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<aither_core::llm::ReasoningState>,
    /// Tool calls carried by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<aither_core::llm::ToolCall>,
    /// The display translation of an assistant reply that drifted from the
    /// communication language. Never part of the model's context: the
    /// content above stays the original.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<MessageTranslation>,
    /// How the daemon read the script of a stored `terminal` call start,
    /// attached when the daemon serves the history; never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<crate::command_reading::CommandReading>,
    /// The reasoning text streamed before an assistant message, kept for
    /// display. Never part of the model's context: provider reasoning state
    /// lives in `reasoning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

/// A reply translated into the communication language for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTranslation {
    /// The reply translated by the fast model.
    pub text: String,
    /// The communication language's ISO 639-3 code.
    pub language: String,
}

/// The display name of the language with ISO 639-3 `code`; the code itself
/// when the detector does not know it.
#[must_use]
pub fn language_display_name(code: &str) -> String {
    code.parse::<aither_agent::Language>()
        .map_or_else(|_| code.to_string(), |language| language.name().to_string())
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
            id: None,
            role: MessageRole::User,
            content: content.into(),
            attachments,
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
        }
    }

    /// Create a new assistant message the agent emitted at `at`.
    pub fn assistant(content: impl Into<String>, at: Timestamp) -> Self {
        Self {
            id: None,
            role: MessageRole::Assistant,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(at),
            tool_call_id: None,
            stats: None,
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
        }
    }

    /// Create a new assistant message, emitted at `at`, with usage stats.
    pub fn assistant_with_stats(
        content: impl Into<String>,
        stats: ChatMessageStats,
        at: Timestamp,
    ) -> Self {
        Self {
            id: None,
            role: MessageRole::Assistant,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(at),
            tool_call_id: None,
            stats: Some(stats),
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
        }
    }

    /// Create a new system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            id: None,
            role: MessageRole::System,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
        }
    }

    /// Create a new tool message recorded at `at`.
    pub fn tool(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        at: Timestamp,
    ) -> Self {
        Self {
            id: None,
            role: MessageRole::Tool,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(at),
            tool_call_id: Some(tool_call_id.into()),
            stats: None,
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
        }
    }

    /// Create a hidden context message.
    ///
    /// This message is treated as system context for the model but is not
    /// surfaced in chat history APIs.
    pub fn hidden_context(content: impl Into<String>) -> Self {
        Self {
            id: None,
            role: MessageRole::HiddenContext,
            content: content.into(),
            attachments: Vec::new(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
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

    /// The input messages stored after `since`: the user messages and hidden
    /// context a run's agent was given, in order, as aither messages.
    ///
    /// An agent restored from a checkpoint older than these rows never saw
    /// them. Everything a run produces (replies, tool events) is left out: its
    /// checkpoints carry what it committed.
    ///
    /// # Errors
    /// Returns [`HistoryReplayError`] when a stored user message carries an
    /// attachment that cannot be replayed.
    pub fn input_messages_after(
        &self,
        since: Timestamp,
    ) -> Result<Vec<aither_core::llm::Message>, HistoryReplayError> {
        self.messages
            .iter()
            .filter(|message| message.timestamp.is_some_and(|stored| stored > since))
            .filter_map(|message| match message.role {
                MessageRole::User => Some(replay_user_message(message)),
                MessageRole::HiddenContext => {
                    Some(Ok(aither_core::llm::Message::system(&message.content)))
                }
                _ => None,
            })
            .collect()
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
                    MessageRole::Assistant => Some(Ok(Message::assistant_with_reasoning(
                        m.content.clone(),
                        m.tool_calls.clone(),
                        m.reasoning.clone(),
                    ))),
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
                                ..
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
                                Some(Ok(Message::tool(id, result.render_for_model())))
                            }
                            ToolEventRecord::Stopped { id, .. } => {
                                if id != tool_call_id {
                                    return Some(Err(HistoryReplayError::ToolEventIdMismatch {
                                        event_id: id,
                                        message_id: tool_call_id.to_string(),
                                    }));
                                }
                                Some(Ok(Message::tool(
                                    id,
                                    ToolEventRecord::stopped_result().render_for_model(),
                                )))
                            }
                        }
                    }
                    // Interaction messages are metadata, not LLM context
                    MessageRole::PermissionRequest
                    | MessageRole::PermissionResponse
                    | MessageRole::WorkspaceRequest
                    | MessageRole::WorkspaceResponse
                    | MessageRole::AskUserRequest
                    | MessageRole::AskUserResponse
                    | MessageRole::Recap => None,
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
                let block = crate::pasted_text::pasted_text_block(&text, saved_path.as_deref());
                let _ = write!(content, "\n{block}");
            }
            AttachmentKind::File => {
                let label = attachment_local_path(attachment).map_or_else(
                    |_| attachment.url.clone(),
                    |path| crate::fs_utils::display_path(&path),
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
/// inline `data:` bytes recorded at send time win: they deliver on every
/// provider without depending on the file still existing or on a Files API
/// the endpoint may not serve. Otherwise the recorded URL is used.
fn attachment_url(
    attachment: &super::runtime::PersistedAttachment,
) -> Result<url::Url, HistoryReplayError> {
    url::Url::parse(&attachment.url)
        .map_err(|error| invalid_attachment(attachment, format!("invalid url: {error}")))
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
    fn the_row_id_crosses_the_wire_with_the_history() {
        let mut message = ChatMessage::user("hello");
        message.id = Some(41);
        let wire = serde_json::to_string(&message).expect("serializes");
        let back: ChatMessage = serde_json::from_str(&wire).expect("deserializes");
        assert_eq!(back.id, Some(41));

        let unstored: ChatMessage =
            serde_json::from_str(&serde_json::to_string(&ChatMessage::user("hi")).unwrap())
                .expect("deserializes");
        assert_eq!(unstored.id, None);
    }

    #[test]
    fn hidden_context_replays_as_system_message() {
        let mut history = ChatHistory::new();
        history.messages_mut().push(ChatMessage::hidden_context(
            "<system-reminder>notice</system-reminder>",
        ));
        history.messages_mut().push(ChatMessage::user("hello"));
        history
            .messages_mut()
            .push(ChatMessage::assistant("world", Timestamp::now()));

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
    fn input_after_a_snapshot_is_the_user_and_hidden_rows_stored_later() {
        let mut history = ChatHistory::new();
        let mut checkpointed = ChatMessage::user("answered");
        checkpointed.timestamp = Some("2026-01-01T00:00:00Z".parse().unwrap());
        history.messages_mut().push(checkpointed);
        let mut reply = ChatMessage::assistant("reply", "2026-01-01T00:00:01Z".parse().unwrap());
        reply.timestamp = Some("2026-01-01T00:00:03Z".parse().unwrap());
        history.messages_mut().push(reply);
        let mut hidden = ChatMessage::hidden_context("<note/>");
        hidden.timestamp = Some("2026-01-01T00:00:04Z".parse().unwrap());
        history.messages_mut().push(hidden);
        let mut interrupted = ChatMessage::user("interrupted");
        interrupted.timestamp = Some("2026-01-01T00:00:05Z".parse().unwrap());
        history.messages_mut().push(interrupted);

        let since = "2026-01-01T00:00:02Z".parse().unwrap();
        let messages = history.input_messages_after(since).unwrap();
        assert_eq!(messages.len(), 2, "only later input rows: {messages:?}");
        assert_eq!(messages[0].role(), Role::System);
        assert_eq!(messages[1].role(), Role::User);
        assert_eq!(messages[1].content(), "interrupted");
    }

    #[test]
    fn tool_lifecycle_replays_as_matching_assistant_call_and_result() {
        let mut history = ChatHistory::new();
        let start = serde_json::to_string(&ToolEventRecord::Start {
            id: "call_123".to_string(),
            name: "lookup".to_string(),
            arguments: "{\"query\":\"rust\"}".to_string(),
            reading: None,
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
            .push(ChatMessage::tool("call_123", start, Timestamp::now()));
        history
            .messages_mut()
            .push(ChatMessage::tool("call_123", end, Timestamp::now()));

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
    fn command_result_survives_storage_and_replays_as_prose() {
        use aither_core::llm::{CommandOutcome, CommandResult, CommandStream};

        let result = ToolResult::Command(Box::new(CommandResult {
            stdout: CommandStream::Inline {
                text: "hi\n".to_string(),
            },
            stderr: Some(CommandStream::Inline {
                text: "warn\n".to_string(),
            }),
            outcome: CommandOutcome::Exited { code: 3 },
            interruptions: Vec::new(),
        }));
        let stored = serde_json::to_string(&ToolEventRecord::End {
            id: "call_1".to_string(),
            name: "terminal".to_string(),
            result: result.clone(),
        })
        .expect("serialize command tool end");
        let ToolEventRecord::End { result: loaded, .. } =
            serde_json::from_str::<ToolEventRecord>(&stored).expect("deserialize command tool end")
        else {
            panic!("expected an end record");
        };
        assert_eq!(loaded, result);
        assert_eq!(
            loaded.render_for_model(),
            "hi\nstderr:\nwarn\nExit code: 3.\n"
        );
    }

    #[test]
    fn stopped_tool_call_replays_with_a_text_result() {
        let mut history = ChatHistory::new();
        let start = serde_json::to_string(&ToolEventRecord::Start {
            id: "call_9".to_string(),
            name: "terminal".to_string(),
            arguments: "{}".to_string(),
            reading: None,
        })
        .expect("serialize tool start");
        let stopped = serde_json::to_string(&ToolEventRecord::Stopped {
            id: "call_9".to_string(),
            name: "terminal".to_string(),
        })
        .expect("serialize tool stop");
        history
            .messages_mut()
            .push(ChatMessage::tool("call_9", start, Timestamp::now()));
        history
            .messages_mut()
            .push(ChatMessage::tool("call_9", stopped, Timestamp::now()));

        let messages = history.to_aither_messages().expect("replay stopped call");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tool_calls()[0].id, "call_9");
        assert_eq!(messages[1].role(), Role::Tool);
        assert_eq!(messages[1].tool_call_id(), Some("call_9"));
        assert_eq!(
            messages[1].content(),
            ToolEventRecord::stopped_result().render_for_model()
        );
    }

    #[test]
    fn stopped_record_round_trips_through_serde() {
        let json = serde_json::to_string(&ToolEventRecord::Stopped {
            id: "c".to_string(),
            name: "terminal".to_string(),
        })
        .expect("serialize");
        assert!(json.contains("\"stage\":\"stopped\""));
        let back: ToolEventRecord = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back,
            ToolEventRecord::Stopped { ref id, ref name } if id == "c" && name == "terminal"
        ));
    }

    #[test]
    fn unstructured_legacy_tool_history_fails_fast() {
        let mut history = ChatHistory::new();
        history.messages_mut().push(ChatMessage::tool(
            "call_123",
            "plain legacy output",
            Timestamp::now(),
        ));
        assert!(matches!(
            history.to_aither_messages(),
            Err(HistoryReplayError::InvalidToolEvent(_))
        ));
    }
}

/// Rebuilds `ChatMessage` rows from transcript entries (the inverse of
/// the daemon's folding).
///
/// A `toolCall` becomes its start record plus an end/stopped record, so
/// rendering, export and injection replay the same rows.
#[must_use]
pub fn chat_messages_from_transcript(
    entries: &[crate::client::WireTranscriptEntry],
) -> Vec<ChatMessage> {
    use crate::client::WireTranscriptEntry as E;

    fn timestamp(ts: Option<&str>) -> Option<Timestamp> {
        ts.and_then(|t| t.parse().ok())
    }
    fn row_id(id: &str) -> Option<i32> {
        id.parse().ok()
    }
    fn base(row: &str, role: MessageRole, content: String, ts: Option<&str>) -> ChatMessage {
        ChatMessage {
            id: row_id(row),
            role,
            content,
            attachments: Vec::new(),
            timestamp: timestamp(ts),
            tool_call_id: None,
            stats: None,
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            translation: None,
            reading: None,
            thinking: None,
        }
    }

    let mut messages = Vec::new();
    for entry in entries {
        match entry {
            E::User {
                id,
                timestamp: ts,
                text,
                attachments,
            } => messages.push(ChatMessage {
                attachments: attachments.clone(),
                ..base(id, MessageRole::User, text.clone(), ts.as_deref())
            }),
            E::Assistant {
                id,
                timestamp: ts,
                text,
                thinking,
                translation,
                stats,
            } => {
                messages.push(ChatMessage {
                    thinking: thinking.clone(),
                    translation: translation.clone(),
                    stats: stats.clone(),
                    ..base(id, MessageRole::Assistant, text.clone(), ts.as_deref())
                });
            }
            E::ToolCall {
                id,
                timestamp: ts,
                call_id,
                name,
                arguments,
                status,
                result,
                reading,
            } => {
                let start = ToolEventRecord::Start {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    reading: None,
                };
                messages.push(ChatMessage {
                    tool_call_id: Some(call_id.clone()),
                    // The reading rides on the row, as the daemon served it.
                    reading: reading.clone(),
                    ..base(
                        id,
                        MessageRole::Tool,
                        serde_json::to_string(&start).unwrap_or_default(),
                        ts.as_deref(),
                    )
                });
                let end = match (status.as_str(), result) {
                    ("done", Some(result)) => Some(ToolEventRecord::End {
                        id: call_id.clone(),
                        name: name.clone(),
                        result: result.clone(),
                    }),
                    ("stopped", _) => Some(ToolEventRecord::Stopped {
                        id: call_id.clone(),
                        name: name.clone(),
                    }),
                    _ => None,
                };
                if let Some(end) = end {
                    messages.push(ChatMessage {
                        id: None,
                        tool_call_id: Some(call_id.clone()),
                        ..base(
                            "",
                            MessageRole::Tool,
                            serde_json::to_string(&end).unwrap_or_default(),
                            ts.as_deref(),
                        )
                    });
                }
            }
            E::InteractionRequest {
                id,
                timestamp: ts,
                interaction,
                content,
            } => {
                let role = match interaction.as_str() {
                    "permission" => MessageRole::PermissionRequest,
                    "workspace" => MessageRole::WorkspaceRequest,
                    _ => MessageRole::AskUserRequest,
                };
                messages.push(base(id, role, content.clone(), ts.as_deref()));
            }
            E::InteractionResponse {
                id,
                timestamp: ts,
                interaction,
                content,
            } => {
                let role = match interaction.as_str() {
                    "permission" => MessageRole::PermissionResponse,
                    "workspace" => MessageRole::WorkspaceResponse,
                    _ => MessageRole::AskUserResponse,
                };
                messages.push(base(id, role, content.clone(), ts.as_deref()));
            }
            E::HiddenContext {
                id,
                timestamp: ts,
                text,
            } => {
                messages.push(base(
                    id,
                    MessageRole::HiddenContext,
                    text.clone(),
                    ts.as_deref(),
                ));
            }
            E::Recap {
                id,
                timestamp: ts,
                content,
            } => {
                messages.push(base(id, MessageRole::Recap, content.clone(), ts.as_deref()));
            }
            E::System {
                id,
                timestamp: ts,
                text,
            } => {
                messages.push(base(id, MessageRole::System, text.clone(), ts.as_deref()));
            }
        }
    }
    messages
}

#[cfg(test)]
mod transcript_tests {
    use super::*;

    #[test]
    fn transcript_entries_fold_back_into_the_same_rows() {
        let at = "2025-01-01T00:00:00Z".to_string();
        let result = aither_core::llm::ToolResult::text("done!");
        let entries = vec![
            crate::client::WireTranscriptEntry::User {
                id: "1".into(),
                timestamp: Some(at.clone()),
                text: "hi".into(),
                attachments: Vec::new(),
            },
            crate::client::WireTranscriptEntry::Assistant {
                id: "2".into(),
                timestamp: Some(at.clone()),
                text: "hello".into(),
                thinking: None,
                translation: None,
                stats: None,
            },
            crate::client::WireTranscriptEntry::ToolCall {
                id: "3".into(),
                timestamp: Some(at.clone()),
                call_id: "c1".into(),
                name: "terminal".into(),
                arguments: "{}".into(),
                status: "done".into(),
                result: Some(result),
                reading: Some(crate::command_reading::CommandReading::of("cat a.rs")),
            },
            crate::client::WireTranscriptEntry::ToolCall {
                id: "4".into(),
                timestamp: Some(at.clone()),
                call_id: "c2".into(),
                name: "monitor".into(),
                arguments: "{}".into(),
                status: "stopped".into(),
                result: None,
                reading: None,
            },
            crate::client::WireTranscriptEntry::ToolCall {
                id: "5".into(),
                timestamp: Some(at.clone()),
                call_id: "c3".into(),
                name: "read".into(),
                arguments: "{}".into(),
                status: "running".into(),
                result: None,
                reading: None,
            },
            crate::client::WireTranscriptEntry::HiddenContext {
                id: "6".into(),
                timestamp: Some(at),
                text: "ctx".into(),
            },
        ];

        let messages = chat_messages_from_transcript(&entries);
        assert_eq!(messages.len(), 8);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].id, Some(1));
        assert_eq!(messages[0].content, "hi");
        assert_eq!(messages[1].role, MessageRole::Assistant);

        // The done call's rows parse back as its Start and End records.
        let start: ToolEventRecord =
            serde_json::from_str(&messages[2].content).expect("start record");
        assert!(matches!(start, ToolEventRecord::Start { ref id, .. } if id == "c1"));
        let end: ToolEventRecord = serde_json::from_str(&messages[3].content).expect("end record");
        assert!(matches!(end, ToolEventRecord::End { ref id, .. } if id == "c1"));
        // The served reading rides on the call's start row for the clients.
        assert_eq!(
            messages[2].reading,
            Some(crate::command_reading::CommandReading::of("cat a.rs"))
        );
        assert!(messages[3].reading.is_none());

        // Stopped: Start + Stopped; running: Start only.
        let stopped: ToolEventRecord =
            serde_json::from_str(&messages[5].content).expect("stopped record");
        assert!(matches!(stopped, ToolEventRecord::Stopped { ref id, .. } if id == "c2"));
        let running: ToolEventRecord =
            serde_json::from_str(&messages[6].content).expect("running start");
        assert!(matches!(running, ToolEventRecord::Start { ref id, .. } if id == "c3"));

        assert_eq!(messages[7].role, MessageRole::HiddenContext);
        assert_eq!(messages[7].id, Some(6));
    }

    #[test]
    fn rewind_requests_the_user_entry_id() {
        let body = serde_json::to_string(&crate::client::RewindRequest {
            entry_id: "7".into(),
        })
        .expect("serialize");
        assert_eq!(body, "{\"entryId\":\"7\"}");
    }
}
