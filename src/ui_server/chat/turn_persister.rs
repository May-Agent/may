//! Atomic turn persistence for the agent event loop.
//!
//! Buffers all messages for one agent turn and writes them atomically
//! at the end of the turn. SSE events continue streaming immediately —
//! only the DB writes are deferred and batched.

use jiff::Timestamp;

use crate::db::ChatDb;
use crate::session::history::{ChatMessage, MessageRole};

/// A buffered message to be persisted at the end of the turn.
struct PendingMessage {
    role: MessageRole,
    content: String,
    tool_call_id: Option<String>,
    /// When the event happened, as stamped at its emission.
    at: Timestamp,
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

    /// Buffer an assistant text message emitted at `at`.
    pub fn push_assistant(&mut self, content: String, at: Timestamp) {
        self.pending.push(PendingMessage {
            role: MessageRole::Assistant,
            content,
            tool_call_id: None,
            at,
        });
    }

    /// Buffer a tool event (start or end) with its serialized record, emitted at `at`.
    pub fn push_tool_event(&mut self, tool_call_id: String, serialized: String, at: Timestamp) {
        self.pending.push(PendingMessage {
            role: MessageRole::Tool,
            content: serialized,
            tool_call_id: Some(tool_call_id),
            at,
        });
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
                attachments: Vec::new(),
                timestamp: Some(msg.at),
                tool_call_id: msg.tool_call_id.clone(),
                stats: None,
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
            };
            let row = db.append_chat_message(&self.session_id, &chat_msg).await?;
            last_id = Some(i64::from(row.id));
        }

        Ok(last_id)
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use tempfile::tempdir;

    use super::TurnPersister;
    use crate::db::ChatDb;
    use crate::session::history::ChatMessage;
    use crate::session::load_history_from_db;

    #[tokio::test]
    async fn restored_entries_keep_their_emission_times() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should open");
        let session_id = "session-times";
        db.create_session(session_id, None, None)
            .await
            .expect("session should be created");

        let before_user = Timestamp::now();
        let user = ChatMessage::user("hello");
        let after_user = Timestamp::now();
        let user_at = user
            .timestamp
            .expect("a user message is stamped on receipt");
        assert!(before_user <= user_at && user_at <= after_user);
        db.append_chat_message(session_id, &user)
            .await
            .expect("user message should persist");

        // Emission times well before "now" prove the persister keeps the
        // stamp it was handed rather than taking its own at flush time.
        let tool_start_at: Timestamp = "2024-03-01T10:00:01.250Z".parse().expect("valid time");
        let tool_end_at: Timestamp = "2024-03-01T10:00:04.750Z".parse().expect("valid time");
        let answer_at: Timestamp = "2024-03-01T10:00:06.500Z".parse().expect("valid time");
        let mut persister = TurnPersister::new(session_id);
        persister.push_tool_event("call_1".to_string(), "start".to_string(), tool_start_at);
        persister.push_tool_event("call_1".to_string(), "end".to_string(), tool_end_at);
        persister.push_assistant("done".to_string(), answer_at);
        persister.flush(&db).await.expect("turn should flush");

        let history = load_history_from_db(&db, session_id)
            .await
            .expect("history should load");
        let times: Vec<_> = history.messages().iter().map(|m| m.timestamp).collect();
        assert_eq!(
            times,
            [
                Some(user_at),
                Some(tool_start_at),
                Some(tool_end_at),
                Some(answer_at)
            ]
        );
    }
}
