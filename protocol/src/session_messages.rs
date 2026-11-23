//! The chat-message wire types persisted in the history database.

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
        /// The call ended in a refusal, not a failure — an unchanged
        /// re-read the policy rejected; the task it read stays running.
        /// Absent on rows stored before refusals were marked.
        #[serde(default)]
        refused: bool,
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

/// How a persisted row updates the session's list summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSummaryUpdate {
    /// Whether the row bumps the session's `updated_at`.
    pub touch_updated_at: bool,
    /// Whether the row's content feeds the preview text.
    pub update_preview: bool,
    /// Whether the row counts toward `message_count`.
    pub increment_message_count: bool,
}

/// A reply translated into the communication language for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTranslation {
    /// The reply translated by the fast model.
    pub text: String,
    /// The communication language's ISO 639-3 code.
    pub language: String,
}

impl ToolEventRecord {
    /// What the model and the UI see as the result of a stopped call.
    #[must_use]
    pub fn stopped_result() -> aither_core::llm::ToolResult {
        aither_core::llm::ToolResult::text("Stopped by the user before it finished.")
    }
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

    /// The summary update a persisted row of this role applies.
    #[must_use]
    pub const fn session_summary_update(&self) -> SessionSummaryUpdate {
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
    pub attachments: Vec<crate::session_runtime::PersistedAttachment>,
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
    /// The card of a stored `terminal` call start whose command renders as
    /// one, attached when the daemon serves the history; never stored on the
    /// row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<crate::command_reading::Card>,
    /// The reasoning text streamed before an assistant message, kept for
    /// display. Never part of the model's context: provider reasoning state
    /// lives in `reasoning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

impl ChatMessage {
    /// Create a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::user_with_attachments(content, Vec::new())
    }

    /// Create a new user message carrying attachments.
    pub fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<crate::session_runtime::PersistedAttachment>,
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
            card: None,
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
            card: None,
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
            card: None,
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
            card: None,
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
            card: None,
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
            card: None,
            thinking: None,
        }
    }
}
