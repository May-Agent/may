//! In-memory chat history reconstructed from SQLite-backed session storage.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Role of a message in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    /// Hidden per-turn context stored for audit/debugging but filtered from normal UI recall.
    HiddenContext,
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

    pub fn from_str(s: &str) -> Result<Self, String> {
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
    pub role: MessageRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<ChatMessageStats>,
}

impl ChatMessage {
    /// Create a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
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
    #[must_use]
    pub fn to_aither_messages(&self) -> Vec<aither_core::llm::Message> {
        self.messages
            .iter()
            .filter_map(|m| {
                use aither_core::llm::Message;
                match m.role {
                    MessageRole::User => Some(Message::user(&m.content)),
                    MessageRole::Assistant => Some(Message::assistant(&m.content)),
                    MessageRole::System => Some(Message::system(&m.content)),
                    MessageRole::HiddenContext => Some(Message::system(&m.content)),
                    MessageRole::Tool => {
                        let tool_call_id = m.tool_call_id.as_deref().unwrap_or_else(|| {
                            panic!("tool history entry is missing tool_call_id: {}", m.content)
                        });
                        Some(Message::tool(tool_call_id, &m.content))
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

#[cfg(test)]
mod tests {
    use super::*;
    use aither_core::llm::Role;

    #[test]
    fn hidden_context_replays_as_system_message() {
        let mut history = ChatHistory::new();
        history.messages_mut().push(ChatMessage::hidden_context(
            "<system-reminder>notice</system-reminder>",
        ));
        history.messages_mut().push(ChatMessage::user("hello"));
        history.messages_mut().push(ChatMessage::assistant("world"));

        let messages = history.to_aither_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role(), Role::System);
        assert_eq!(messages[1].role(), Role::User);
        assert_eq!(messages[2].role(), Role::Assistant);
        assert_eq!(messages[0].content(), "<system-reminder>notice</system-reminder>");
    }

    #[test]
    fn tool_messages_preserve_tool_call_id_when_replayed() {
        let mut history = ChatHistory::new();
        history
            .messages_mut()
            .push(ChatMessage::tool("call_123", "{\"ok\":true}"));

        let messages = history.to_aither_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), Role::Tool);
        assert_eq!(messages[0].tool_call_id(), Some("call_123"));
    }
}
