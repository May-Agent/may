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

/// Role of a message in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl MessageRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }

    fn from_str(s: &str) -> Result<Self, ChatHistoryError> {
        match s.to_lowercase().as_str() {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "system" => Ok(Self::System),
            "tool" => Ok(Self::Tool),
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
}

impl ChatMessage {
    /// Create a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
        }
    }

    /// Create a new assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
        }
    }

    /// Create a new system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            timestamp: Some(Timestamp::now()),
            tool_call_id: None,
        }
    }
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
    pub fn to_aither_messages(&self) -> Vec<aither_core::llm::Message> {
        self.messages
            .iter()
            .map(|m| {
                use aither_core::llm::Message;
                match m.role {
                    MessageRole::User => Message::user(&m.content),
                    MessageRole::Assistant => Message::assistant(&m.content),
                    MessageRole::System => Message::system(&m.content),
                    MessageRole::Tool => {
                        // Tool messages need special handling
                        Message::user(&m.content)
                    }
                }
            })
            .collect()
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
        let content = r#"---
role: user
---
Hello, May!"#;

        let history = ChatHistory::parse(content).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history.messages()[0].role, MessageRole::User);
        assert_eq!(history.messages()[0].content, "Hello, May!");
    }

    #[test]
    fn test_parse_with_metadata() {
        let content = r#"---
title: My Session
model: claude-3-sonnet
---

---
role: user
timestamp: 2024-01-15T10:30:00Z
---
Hello!

---
role: assistant
timestamp: 2024-01-15T10:30:05Z
---
Hi! How can I help?"#;

        let history = ChatHistory::parse(content).unwrap();
        assert_eq!(history.metadata.title.as_deref(), Some("My Session"));
        assert_eq!(history.metadata.model.as_deref(), Some("claude-3-sonnet"));
        assert_eq!(history.len(), 2);
        assert_eq!(history.messages()[0].role, MessageRole::User);
        assert_eq!(history.messages()[1].role, MessageRole::Assistant);
    }

    #[test]
    fn test_parse_conversation() {
        let content = r#"---
role: user
timestamp: 2024-01-15T10:30:00Z
---
Hello!

---
role: assistant
timestamp: 2024-01-15T10:30:05Z
---
Hi! How can I help?"#;

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
        let content = r#"---
role: user
---
What's in this image?

![](images/img_001.png)"#;

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
        let corrupted_content = r#"---
role: assistant
---
Beneath the sky, both warm and wide,
---
Dreams and whispers gently glide."#;

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
