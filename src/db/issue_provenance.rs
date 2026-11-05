//! Local record of every issue change may makes, on both issue backends.

use super::{ChatDb, ChatDbError, query};

/// One issue change to record.
#[derive(Debug, Clone, Copy)]
pub struct NewIssueProvenance<'a> {
    /// Issue backend (`may` or `github`).
    pub backend: &'a str,
    /// Repository the issue belongs to, as the backend names it.
    pub repository: &'a str,
    /// Issue number.
    pub issue_number: i64,
    /// Comment id, for a change to a comment.
    pub comment_id: Option<i64>,
    /// What changed (`create`, `edit`, `comment`, ...).
    pub action: &'a str,
    /// What the change touched, for actions that need it.
    pub detail: Option<&'a str>,
    /// Session that made the change.
    pub session_id: &'a str,
    /// Agent that made the change: `main`, `subagent:<name>` or `exec`.
    pub agent: &'a str,
    /// Tool call that made the change; absent outside a model turn.
    pub tool_call_id: Option<&'a str>,
}

/// A stored issue change.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct IssueProvenanceRecord {
    /// Record id; records of one issue are ordered by it.
    pub id: i64,
    /// Issue backend.
    pub backend: String,
    /// Repository the issue belongs to.
    pub repository: String,
    /// Issue number.
    pub issue_number: i64,
    /// Comment id, for a change to a comment.
    pub comment_id: Option<i64>,
    /// What changed.
    pub action: String,
    /// What the change touched.
    pub detail: Option<String>,
    /// Session that made the change.
    pub session_id: String,
    /// Agent that made the change.
    pub agent: String,
    /// Tool call that made the change.
    pub tool_call_id: Option<String>,
    /// UTC time of the change (ISO 8601).
    pub recorded_at: String,
}

/// A user message of a session with its time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct UserMessageAt {
    /// Message body.
    pub content: String,
    /// Message time (ISO 8601).
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct ToolCallsRow {
    tool_calls: String,
}

impl ChatDb {
    /// Record an issue change.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the insert fails.
    pub async fn record_issue_provenance(
        &self,
        record: NewIssueProvenance<'_>,
    ) -> Result<(), ChatDbError> {
        let recorded_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO issue_provenance (backend, repository, issue_number, comment_id, action, detail, session_id, agent, tool_call_id, recorded_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            record.backend,
            record.repository,
            record.issue_number,
            record.comment_id,
            record.action,
            record.detail,
            record.session_id,
            record.agent,
            record.tool_call_id,
            recorded_at
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Every recorded change to one issue, oldest first.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn issue_provenance(
        &self,
        backend: &str,
        repository: &str,
        issue_number: i64,
    ) -> Result<Vec<IssueProvenanceRecord>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT id, backend, repository, issue_number, comment_id, action, detail, session_id, agent, tool_call_id, recorded_at FROM issue_provenance WHERE backend = ? AND repository = ? AND issue_number = ? ORDER BY id ASC",
            backend,
            repository,
            issue_number
        )
        .fetch_all::<IssueProvenanceRecord>()
        .await?)
    }

    /// The newest `limit` user messages of `session_id` sent at or before
    /// `before`, newest first.
    ///
    /// # Errors
    /// Returns [`ChatDbError::Storage`] when the query fails or a stored
    /// timestamp is not a valid instant.
    pub async fn user_messages_before(
        &self,
        session_id: &str,
        before: &str,
        limit: usize,
    ) -> Result<Vec<UserMessageAt>, ChatDbError> {
        let before = parse_instant(before)?;
        let rows = query!(
            self.db,
            "SELECT content, timestamp FROM messages WHERE session_id = ? AND role = 'user' AND timestamp IS NOT NULL ORDER BY id DESC",
            session_id
        )
        .fetch_all::<UserMessageAt>()
        .await?;
        let mut found = Vec::new();
        for row in rows {
            let sent = row.timestamp.as_deref().map(parse_instant).transpose()?;
            if sent.is_some_and(|sent| sent <= before) {
                found.push(row);
                if found.len() == limit {
                    break;
                }
            }
        }
        Ok(found)
    }

    /// The tool call `call_id` as the assistant message of `session_id`
    /// carrying it stored it: `(tool name, arguments)`.
    ///
    /// # Errors
    /// Returns [`ChatDbError::Storage`] when the query fails or a stored
    /// tool-call list does not parse.
    pub async fn session_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Option<(String, serde_json::Value)>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT tool_calls FROM messages WHERE session_id = ? AND role = 'assistant' AND tool_calls IS NOT NULL AND instr(tool_calls, ?) > 0",
            session_id,
            call_id
        )
        .fetch_all::<ToolCallsRow>()
        .await?;
        for row in rows {
            let calls = serde_json::from_str::<Vec<aither_core::llm::ToolCall>>(&row.tool_calls)
                .map_err(|error| {
                    ChatDbError::Storage(format!("stored tool calls do not parse: {error}"))
                })?;
            if let Some(call) = calls.into_iter().find(|call| call.id == call_id) {
                return Ok(Some((call.name, call.arguments)));
            }
        }
        Ok(None)
    }
}

fn parse_instant(value: &str) -> Result<jiff::Timestamp, ChatDbError> {
    value
        .parse()
        .map_err(|error| ChatDbError::Storage(format!("invalid timestamp {value:?}: {error}")))
}
