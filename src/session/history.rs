//! Chat history storage with YAML frontmatter format.
//!
//! Format:
//! ```markdown
//! ---
//! title: My Session
//! model: claude-3-sonnet
//! ---
//!
//! ---
//! role: user
//! timestamp: 2024-01-15T10:30:00Z
//! ---
//! Hello, May!
//!
//! ---
//! role: assistant
//! timestamp: 2024-01-15T10:30:05Z
//! ---
//! Hello! How can I help?
//! ```

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// Error parsing chat history.
#[derive(Debug, thiserror::Error)]
pub enum ChatHistoryError {
    #[error("Malformed frontmatter at position {0}")]
    MalformedFrontmatter(usize),
    #[error("Missing role in frontmatter")]
    MissingRole,
    #[error("Invalid role: {0}")]
    InvalidRole(String),
    #[error("YAML parse error: {0}")]
    Yaml(String),
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
    /// Hidden system context persisted for future turns but not shown in UI.
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
pub(crate) struct SessionSummaryUpdate {
    pub(crate) touch_updated_at: bool,
    pub(crate) update_preview: bool,
    pub(crate) increment_message_count: bool,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
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

    pub fn from_str(s: &str) -> Result<Self, ChatHistoryError> {
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
            _ => Err(ChatHistoryError::InvalidRole(s.to_string())),
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

    /// Create a permission request message.
    ///
    /// Content is JSON-serialized `PermissionRequestData`.
    pub fn permission_request(data: &PermissionRequestData) -> Self {
        Self {
            role: MessageRole::PermissionRequest,
            content: serde_json::to_string(data)
                .expect("PermissionRequestData is always serializable"),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create a permission response message.
    ///
    /// Content is JSON-serialized `PermissionResponseData`.
    pub fn permission_response(data: &PermissionResponseData) -> Self {
        Self {
            role: MessageRole::PermissionResponse,
            content: serde_json::to_string(data)
                .expect("PermissionResponseData is always serializable"),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create an ask-user request message.
    ///
    /// Content is JSON-serialized `AskUserRequestData`.
    pub fn ask_user_request(data: &AskUserRequestData) -> Self {
        Self {
            role: MessageRole::AskUserRequest,
            content: serde_json::to_string(data)
                .expect("AskUserRequestData is always serializable"),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create a workspace request message.
    ///
    /// Content is JSON-serialized `WorkspaceRequestData`.
    pub fn workspace_request(data: &WorkspaceRequestData) -> Self {
        Self {
            role: MessageRole::WorkspaceRequest,
            content: serde_json::to_string(data)
                .expect("WorkspaceRequestData is always serializable"),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create a workspace response message.
    ///
    /// Content is JSON-serialized `WorkspaceResponseData`.
    pub fn workspace_response(data: &WorkspaceResponseData) -> Self {
        Self {
            role: MessageRole::WorkspaceResponse,
            content: serde_json::to_string(data)
                .expect("WorkspaceResponseData is always serializable"),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Create an ask-user response message.
    ///
    /// Content is JSON-serialized `AskUserResponseData`.
    pub fn ask_user_response(data: &AskUserResponseData) -> Self {
        Self {
            role: MessageRole::AskUserResponse,
            content: serde_json::to_string(data)
                .expect("AskUserResponseData is always serializable"),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
            stats: None,
        }
    }

    /// Try to parse the content as a specific type.
    pub fn parse_content<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_str(&self.content).ok()
    }
}

/// Permission request data stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequestData {
    /// Unique request ID.
    pub id: String,
    /// Permission mode requested.
    pub mode: String,
    /// The script/command needing permission.
    pub script: String,
}

/// Permission response data stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResponseData {
    /// Request ID this responds to.
    pub id: String,
    /// Whether permission was granted.
    pub approved: bool,
}

/// Workspace request data stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRequestData {
    /// Unique request ID.
    pub id: String,
    /// Requested path.
    pub path: String,
    /// User-visible reason for the request.
    pub reason: String,
}

/// Workspace response data stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceResponseData {
    /// Request ID this responds to.
    pub id: String,
    /// Whether access was granted.
    pub approved: bool,
}

/// Ask-user request data stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserRequestData {
    /// Unique request ID.
    pub id: String,
    /// Questions asked (JSON representation of the questions).
    pub questions: serde_json::Value,
}

/// Ask-user response data stored in chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserResponseData {
    /// Request ID this responds to.
    pub id: String,
    /// Answers provided (JSON representation of answers).
    pub answers: serde_json::Value,
}

/// YAML frontmatter for a message or metadata block.
/// If `role` is None, this is a metadata block; otherwise it's a message.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    timestamp: Option<Timestamp>,
    #[serde(default)]
    tool_call_id: Option<String>,
    // Metadata fields (only present in header block)
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Escape content lines that are "---" (with optional trailing whitespace) to prevent splitting issues.
/// Lines like "---", "--- ", "---  " are all normalized to "\---" during serialization.
fn escape_content(s: &str) -> String {
    s.lines()
        .map(|line| if line.trim() == "---" { "\\---" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Unescape content lines that were escaped during serialization.
fn unescape_content(s: &str) -> String {
    s.lines()
        .map(|line| if line == "\\---" { "---" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Chat history with YAML frontmatter parsing.
#[derive(Debug, Clone, Default)]
pub struct ChatHistory {
    /// Session metadata (title, model).
    pub metadata: SessionMetadata,
    /// Chat messages.
    messages: Vec<ChatMessage>,
}

impl ChatHistory {
    /// Create an empty chat history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a chat history with metadata.
    pub fn with_metadata(metadata: SessionMetadata) -> Self {
        Self {
            metadata,
            messages: Vec::new(),
        }
    }

    /// Parse chat history from the markdown format with YAML frontmatter.
    pub fn parse(content: &str) -> Result<Self, ChatHistoryError> {
        let mut messages: Vec<ChatMessage> = Vec::new();
        let mut metadata = SessionMetadata::default();

        // Skip leading whitespace
        let content = content.trim_start();
        if content.is_empty() {
            return Ok(Self::new());
        }

        // Split on message boundaries (--- at start of line)
        let parts: Vec<&str> = content.split("\n---\n").collect();

        let mut i = 0;
        let mut found_metadata = false;

        while i < parts.len() {
            let part = parts[i].trim();

            // Skip empty parts
            if part.is_empty() {
                i += 1;
                continue;
            }

            // Determine if this part is a frontmatter block or message content.
            // Frontmatter blocks either:
            // 1. Start with --- (first block or after metadata)
            // 2. Start with known frontmatter fields (role:, title:, model:)
            //    because the split on \n---\n consumed the opening ---
            let is_frontmatter = part.starts_with("---")
                || part.starts_with("role:")
                || part.starts_with("title:")
                || part.starts_with("model:");

            if !is_frontmatter {
                // This is stray content from unescaped separator in message content.
                // Append to the previous message's content.
                if let Some(last_msg) = messages.last_mut() {
                    last_msg.content.push_str("\n---\n");
                    last_msg.content.push_str(&unescape_content(part));
                }
                i += 1;
                continue;
            }

            // Remove leading --- if present and parse as YAML
            let yaml_content = part.strip_prefix("---").unwrap_or(part).trim();

            // Parse frontmatter
            let frontmatter: Frontmatter = serde_yml::from_str(yaml_content)
                .map_err(|e| ChatHistoryError::Yaml(e.to_string()))?;

            // Check if this is a metadata block (no role) or a message block
            if let Some(role_str) = &frontmatter.role {
                // This is a message block
                let role = MessageRole::from_str(role_str)?;

                // Get message content from next part
                i += 1;
                let content = if i < parts.len() {
                    // Unescape any escaped "---" lines in content
                    unescape_content(parts[i].trim())
                } else {
                    String::new()
                };

                messages.push(ChatMessage {
                    role,
                    content,
                    timestamp: frontmatter.timestamp,
                    tool_call_id: frontmatter.tool_call_id,
                    stats: None,
                });
            } else if !found_metadata {
                // This is the metadata block (first block without role)
                metadata = SessionMetadata {
                    title: frontmatter.title,
                    model: frontmatter.model,
                };
                found_metadata = true;
                // Skip the empty content part after metadata
                i += 1;
            }

            i += 1;
        }

        Ok(Self { metadata, messages })
    }

    /// Serialize to the markdown format with YAML frontmatter.
    pub fn serialize(&self) -> String {
        let mut output = String::new();

        // Write metadata block first (even if empty, to maintain consistent format)
        output.push_str("---\n");
        if let Some(title) = &self.metadata.title {
            let encoded = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".to_string());
            let _ = writeln!(output, "title: {encoded}");
        }
        if let Some(model) = &self.metadata.model {
            let encoded = serde_json::to_string(model).unwrap_or_else(|_| "\"\"".to_string());
            let _ = writeln!(output, "model: {encoded}");
        }
        output.push_str("---\n\n");

        // Write messages
        for msg in &self.messages {
            output.push_str("---\n");
            let _ = writeln!(output, "role: {}", msg.role.as_str());
            if let Some(ts) = &msg.timestamp {
                let _ = writeln!(output, "timestamp: {ts}");
            }
            if let Some(id) = &msg.tool_call_id {
                let _ = writeln!(output, "tool_call_id: {id}");
            }
            output.push_str("---\n");
            // Escape lines that are exactly "---" to prevent them from being
            // treated as message separators during parsing
            output.push_str(&escape_content(&msg.content));
            output.push_str("\n\n");
        }

        output
    }

    /// Add a message to history.
    pub fn push(&mut self, message: ChatMessage) {
        self.messages.push(message);
    }

    /// Get all messages.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Get mutable access to messages.
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }

    /// Check if history is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Convert to aither messages.
    ///
    /// Skips interaction messages (permission/ask-user) which are not part of the LLM conversation.
    pub fn to_aither_messages(&self) -> Vec<aither_core::llm::Message> {
        self.messages
            .iter()
            .filter_map(|m| {
                use aither_core::llm::Message;
                match m.role {
                    MessageRole::User => Some(Message::user(&m.content)),
                    MessageRole::Assistant => Some(Message::assistant(&m.content)),
                    MessageRole::System | MessageRole::HiddenContext => {
                        Some(Message::system(&m.content))
                    }
                    MessageRole::Tool => Some(Message::user(&m.content)),
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

    /// Check whether network permission was previously approved in this session.
    pub fn has_network_approval(&self) -> bool {
        self.was_permission_mode_approved("network")
    }

    /// Find unanswered permission requests (no matching PermissionResponse).
    pub fn pending_permission_requests(&self) -> Vec<PermissionRequestData> {
        let responded_ids: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::PermissionResponse)
            .filter_map(|m| m.parse_content::<PermissionResponseData>())
            .map(|d| d.id)
            .collect();

        self.messages
            .iter()
            .filter(|m| m.role == MessageRole::PermissionRequest)
            .filter_map(|m| m.parse_content::<PermissionRequestData>())
            .filter(|d| !responded_ids.contains(&d.id))
            .collect()
    }

    /// Find unanswered ask-user requests (no matching AskUserResponse).
    pub fn pending_ask_user_requests(&self) -> Vec<AskUserRequestData> {
        let responded_ids: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::AskUserResponse)
            .filter_map(|m| m.parse_content::<AskUserResponseData>())
            .map(|d| d.id)
            .collect();

        self.messages
            .iter()
            .filter(|m| m.role == MessageRole::AskUserRequest)
            .filter_map(|m| m.parse_content::<AskUserRequestData>())
            .filter(|d| !responded_ids.contains(&d.id))
            .collect()
    }

    /// Check if a specific permission mode was ever approved in this session's history.
    pub fn was_permission_mode_approved(&self, mode: &str) -> bool {
        // Collect all approved response IDs
        let approved_ids: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::PermissionResponse)
            .filter_map(|m| m.parse_content::<PermissionResponseData>())
            .filter(|d| d.approved)
            .map(|d| d.id)
            .collect();

        // Check if any approved response matches a request with the given mode
        self.messages
            .iter()
            .filter(|m| m.role == MessageRole::PermissionRequest)
            .filter_map(|m| m.parse_content::<PermissionRequestData>())
            .any(|d| d.mode == mode && approved_ids.contains(&d.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty() {
        let history = ChatHistory::parse("").unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_parse_single_message() {
        let content = include_str!("testdata/history_parse_single_message.md");

        let history = ChatHistory::parse(content).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history.messages()[0].role, MessageRole::User);
        assert_eq!(history.messages()[0].content, "Hello, May!");
    }

    #[test]
    fn test_parse_conversation() {
        let content = include_str!("testdata/history_parse_conversation.md");

        let history = ChatHistory::parse(content).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history.messages()[0].role, MessageRole::User);
        assert_eq!(history.messages()[1].role, MessageRole::Assistant);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut history = ChatHistory::with_metadata(SessionMetadata {
            title: Some("Test Session".to_string()),
            model: Some("test-model".to_string()),
        });
        history.push(ChatMessage::user("Hello!"));
        history.push(ChatMessage::assistant("Hi there!"));

        let serialized = history.serialize();
        let parsed = ChatHistory::parse(&serialized).unwrap();

        assert_eq!(parsed.metadata.title.as_deref(), Some("Test Session"));
        assert_eq!(parsed.metadata.model.as_deref(), Some("test-model"));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.messages()[0].content, "Hello!");
        assert_eq!(parsed.messages()[1].content, "Hi there!");
    }

    #[test]
    fn test_message_with_image() {
        let content = include_str!("testdata/history_message_with_image.md");

        let history = ChatHistory::parse(content).unwrap();
        assert_eq!(history.len(), 1);
        assert!(
            history.messages()[0]
                .content
                .contains("![](images/img_001.png)")
        );
    }

    #[test]
    fn test_content_with_triple_dash() {
        // Content containing "---" should be properly escaped during serialization
        // and unescaped during parsing
        let mut history = ChatHistory::new();
        let poem = "Beneath the willows\n---\nA verse continues\n---\nThe end.";
        history.push(ChatMessage::assistant(poem));

        let serialized = history.serialize();
        // Verify the escaped form in serialized output
        assert!(serialized.contains("\\---"));

        // Parse it back and verify content is preserved exactly
        let parsed = ChatHistory::parse(&serialized).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.messages()[0].content, poem);
    }

    #[test]
    fn test_content_with_triple_dash_trailing_whitespace() {
        // Content containing "--- " (with trailing space) should also be escaped
        let mut history = ChatHistory::new();
        let poem = "A poem:\n--- \nSome verse\n---  \nThe end.";
        history.push(ChatMessage::assistant(poem));

        let serialized = history.serialize();
        // Verify the escaped form in serialized output (trailing whitespace normalized away)
        assert!(serialized.contains("\\---"));
        // Should not contain unescaped "--- " with trailing space followed by newline
        assert!(!serialized.contains("\n--- \n"));
        assert!(!serialized.contains("\n---  \n"));

        // Parse it back - note: trailing whitespace on --- lines is normalized to just ---
        let parsed = ChatHistory::parse(&serialized).unwrap();
        assert_eq!(parsed.len(), 1);
        // The content is normalized (trailing whitespace on --- lines removed)
        assert_eq!(
            parsed.messages()[0].content,
            "A poem:\n---\nSome verse\n---\nThe end."
        );
    }

    #[test]
    fn test_escape_unescape_content() {
        assert_eq!(escape_content("hello\n---\nworld"), "hello\n\\---\nworld");
        assert_eq!(unescape_content("hello\n\\---\nworld"), "hello\n---\nworld");
        // No change when no --- present
        assert_eq!(escape_content("hello world"), "hello world");
        assert_eq!(unescape_content("hello world"), "hello world");
        // Test with trailing whitespace
        assert_eq!(escape_content("hello\n--- \nworld"), "hello\n\\---\nworld");
        assert_eq!(escape_content("hello\n---  \nworld"), "hello\n\\---\nworld");
    }

    #[test]
    fn test_parse_corrupted_unescaped_separator() {
        // Simulate old/corrupted data where content contains unescaped ---
        // This would have been saved by an older version without proper escaping
        let corrupted_content =
            include_str!("testdata/history_parse_corrupted_unescaped_separator.md");

        // This should NOT fail with YAML parse error
        let history = ChatHistory::parse(corrupted_content).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history.messages()[0].role, MessageRole::Assistant);
        // The content should be reconstructed with the --- separator
        assert!(history.messages()[0].content.contains("Beneath the sky"));
        assert!(history.messages()[0].content.contains("---"));
        assert!(
            history.messages()[0]
                .content
                .contains("Dreams and whispers")
        );
    }
}
