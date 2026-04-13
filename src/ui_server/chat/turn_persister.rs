//! Atomic turn persistence for the agent event loop.
//!
//! Buffers all messages for one agent turn and writes them atomically
//! at the end of the turn. SSE events continue streaming immediately —
//! only the DB writes are deferred and batched.

use crate::db::ChatDb;
use crate::session::history::{ChatMessage, MessageRole};

/// A buffered message to be persisted at the end of the turn.
struct PendingMessage {
    role: MessageRole,
    content: String,
    tool_call_id: Option<String>,
}

/// Buffers chat messages for a turn and writes them atomically.
pub(super) struct TurnPersister {
    session_id: String,
    pending: Vec<PendingMessage>,
}

impl TurnPersister {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            pending: Vec::new(),
        }
    }

    /// Buffer an assistant text message.
    pub fn push_assistant(&mut self, content: String) {
        self.pending.push(PendingMessage {
            role: MessageRole::Assistant,
            content,
            tool_call_id: None,
        });
    }

    /// Buffer a tool event (start or end) with its serialized record.
    pub fn push_tool_event(&mut self, tool_call_id: String, serialized: String) {
        self.pending.push(PendingMessage {
            role: MessageRole::Tool,
            content: serialized,
            tool_call_id: Some(tool_call_id),
        });
    }

    /// Check if any tool events have been buffered.
    pub fn has_tool_activity(&self) -> bool {
        self.pending.iter().any(|m| m.role == MessageRole::Tool)
    }

    /// Flush all buffered messages to the database.
    ///
    /// Returns the ID of the last inserted message, or `None` if nothing was flushed.
    pub async fn flush(&mut self, db: &ChatDb) -> Result<Option<i64>, crate::db::ChatDbError> {
        if self.pending.is_empty() {
            return Ok(None);
        }

        let messages = std::mem::take(&mut self.pending);
        let mut last_id = None;

        for msg in &messages {
            let chat_msg = ChatMessage {
                role: msg.role.clone(),
                content: msg.content.clone(),
                timestamp: Some(jiff::Timestamp::now()),
                tool_call_id: msg.tool_call_id.clone(),
                stats: None,
            };
            let row = db
                .append_chat_message(&self.session_id, &chat_msg)
                .await?;
            last_id = Some(i64::from(row.id));
        }

        Ok(last_id)
    }
}
